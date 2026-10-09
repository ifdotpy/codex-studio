#!/usr/bin/env python3
"""Profile durable supervisor stream receipts across concurrent agent output."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from collections import defaultdict
import io
import json
from pathlib import Path
import queue
import sys
import tempfile
import threading
import time
import uuid

sys.dont_write_bytecode = True
ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import AppServer, Runtime
from codex_streaming import StreamBuffer
import codex_sqlite
import codex_sync_entities


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.05)
            self.changed.clear()


class PhaseProbe:
    def __init__(self):
        self.local = threading.local()
        self.guard = threading.Lock()
        self.times = defaultdict(lambda: [0, 0.0])

    def wrap(self, owner, name, phase):
        original = getattr(owner, name)

        def measured(*args, **kwargs):
            method = getattr(self.local, "method", "background")
            started = time.perf_counter()
            try:
                return original(*args, **kwargs)
            finally:
                elapsed = (time.perf_counter() - started) * 1000
                with self.guard:
                    row = self.times[(method, phase)]
                    row[0] += 1
                    row[1] += elapsed
                    row.append(elapsed)

        setattr(owner, name, measured)
        return original


class TracedLock:
    def __init__(self, lock, probe):
        self.lock, self.probe = lock, probe
        self.local = threading.local()
        self.guard = threading.Lock()
        self.events = defaultdict(lambda: [0, 0.0, 0.0])

    def acquire(self, *args, **kwargs):
        started = time.perf_counter()
        acquired = self.lock.acquire(*args, **kwargs)
        entered = time.perf_counter()
        if acquired:
            depth = getattr(self.local, "depth", 0)
            self.local.depth = depth + 1
            if depth == 0:
                method = getattr(self.probe.local, "method", "background")
                with self.guard:
                    row = self.events[method]
                    row[0] += 1
                    row[1] += entered - started
                self.local.method, self.local.entered = method, entered
        return acquired

    def release(self):
        depth = self.local.depth - 1
        self.local.depth = depth
        if depth == 0:
            with self.guard:
                self.events[self.local.method][2] += time.perf_counter() - self.local.entered
        self.lock.release()

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_):
        self.release()


class ReceiptProc:
    def __init__(self, count):
        self.handle = "fixture:account"
        self.read_cursor = count
        self.cursor = 0
        self.ack_pending = set()
        self.event_batches = {}

    def ack_applied_deltas(self, _):
        return 0

    def register_event_batch(self, sequences):
        if (not sequences or len(sequences) > 128
                or any(b != a + 1 for a, b in zip(sequences, sequences[1:]))):
            raise RuntimeError("invalid fixture batch")
        self.event_batches[sequences[-1]] = tuple(sequences)

    def ack(self, sequence):
        self.ack_pending.add(sequence)
        self.ack_pending.update(self.event_batches.pop(sequence, ()))
        while self.cursor + 1 in self.ack_pending:
            self.cursor += 1
            self.ack_pending.remove(self.cursor)


def sqlite_snapshot():
    return codex_sqlite.diagnostics()


def sqlite_delta(before, after):
    result = {}
    for group in ("writeWait", "transaction"):
        first, last = before.get(group, {}), after.get(group, {})
        fields = ("writeWaits", "writeWaitMs") if group == "writeWait" else (
            "transactions", "longTransactions")
        result[group] = {key: round(last.get(key, 0) - first.get(key, 0), 3) for key in fields}
        if group == "transaction":
            result[group]["p95Ms"] = last.get("p95TransactionMs", 0)
        changed = {}
        for site, row in last.get("sites", {}).items():
            old = first.get("sites", {}).get(site, {})
            values = {key: round(row.get(key, 0) - old.get(key, 0), 3)
                      for key in ("writeWaits", "writeWaitMs", "transactions")}
            if any(values.values()):
                if group == "transaction":
                    values["p95Ms"] = row.get("p95TransactionMs", 0)
                changed[site] = values
        result[group]["sites"] = changed
    return result


def percentile(values, fraction):
    values = sorted(values)
    return round(values[min(len(values) - 1, int((len(values) - 1) * fraction))], 3) if values else 0


def measure(batch_limit, agents=16, deltas_per_agent=20):
    with tempfile.TemporaryDirectory(prefix="studio-supervisor-stream-load-") as temporary:
        root = Path(temporary)
        runtime = QuietRuntime(root / "state", server_factory=lambda *args: None)
        probe = PhaseProbe()
        lead = runtime.create({"name": "Load fixture", "cwd": temporary, "prompt": ""},
                              draft=True, defer=True)
        records = []
        with runtime.lock, runtime.db() as db:
            base = runtime.agent(lead["id"], db)
            for index in range(agents):
                agent = {**base, "id": str(uuid.uuid4()), "name": f"Agent {index}",
                         "threadId": f"load-thread-{index}", "turnId": f"load-turn-{index}",
                         "accountKey": "default", "inFlight": True, "status": "running",
                         "events": 0}
                runtime.put(db, "agents", agent)
                item_id = f"command-{index}"
                runtime.item(db, agent["id"], item_id, "output",
                             json.dumps({"id": item_id, "type": "commandExecution",
                                         "aggregatedOutput": "", "outputTruncated": False}),
                             "commandExecution", toolStatus="running", turnId=agent["turnId"],
                             index_search=False)
                runtime.put(db, "tasks", {"id": agent["id"] + ":" + item_id, "agent": agent["id"],
                                           "itemId": item_id, "turnId": agent["turnId"],
                                           "kind": "command", "type": "commandExecution",
                                           "status": "running", "tail": ""})
                records.append((agent, item_id))

        traced = TracedLock(runtime.lock, probe)
        runtime.lock = traced
        for name, phase in (("commit_supervisor_event", "supervisorCommit"),
                            ("analytics_safe", "analytics"), ("put", "runtimePut"),
                            ("item", "transcriptItem"), ("output", "monitorOutput")):
            probe.wrap(runtime, name, phase)
        stream_flush_original = probe.wrap(StreamBuffer, "flush_locked", "streamBufferFlush")
        sync_put_original = probe.wrap(codex_sync_entities, "put", "syncEntityPut")
        connection_current_original = runtime.connection_current
        probe.wrap(runtime, "supervisor_event_applied", "receiptLookup")
        server = AppServer.__new__(AppServer)
        server.supervisor_mode = True
        server.callbacks = queue.Queue(maxsize=AppServer.CALLBACK_QUEUE_LIMIT)
        server.callback_lock = threading.RLock()
        server.dispatch_stopped = False
        server.reader_done = threading.Event()
        server.dispatcher_done = threading.Event()
        server.log = io.BytesIO()
        server.close_log_if_idle = lambda: None
        server.closed = True
        server.died = lambda: None
        server.proc = ReceiptProc(agents * 2 + agents * deltas_per_agent)
        server._supervisor_stream_batch_limit = batch_limit
        server.supervisor_event_applied = lambda sequence: runtime.supervisor_event_applied(
            server.proc.handle, sequence)
        server.supervisor_commit = lambda message, sequence: runtime.commit_supervisor_event(
            server.proc.handle, message, sequence, "default", None)

        delivered = []
        queue_delays = []
        blocked, release = threading.Event(), threading.Event()

        def notify(message):
            if message.get("method") == "fixture/block":
                blocked.set()
                release.wait(10)
                return
            queue_delays.append(max(0, (time.time() - message["_studioReceivedAt"]) * 1000))
            runtime.notification(message, "default", None)
            delivered.append(message)

        # Install a single wrapper, not a new closure per notification.
        original_notification = runtime.notification
        def measured_notification(message, account="default", connection=None):
            method = message.get("method", "receipt")
            previous = getattr(probe.local, "method", "background")
            probe.local.method = method
            started = time.perf_counter()
            try:
                return original_notification(message, account, connection)
            finally:
                elapsed = (time.perf_counter() - started) * 1000
                with probe.guard:
                    row = probe.times[(method, "callback")]
                    row[0] += 1
                    row[1] += elapsed
                    row.append(elapsed)
                probe.local.method = previous
        runtime.notification = measured_notification
        server.notification = notify
        server.request = lambda _: None
        server.supervisor_commit = lambda message, sequence: (
            setattr(probe.local, "method", message.get("method", "receipt")),
            runtime.commit_supervisor_event(server.proc.handle, message, sequence, "default", None))[-1]

        total = agents * 2 + agents * deltas_per_agent
        sqlite_before = sqlite_snapshot()
        sequence = 0
        events = []
        for agent, item_id in records:
            sequence += 1
            events.append({"method": "item/started", "params": {
                "threadId": agent["threadId"], "turnId": agent["turnId"],
                "item": {"id": item_id, "type": "commandExecution", "command": "fixture"}}})
        for chunk in range(deltas_per_agent):
            for agent, item_id in records:
                sequence += 1
                events.append({"method": "item/commandExecution/outputDelta", "params": {
                    "threadId": agent["threadId"], "turnId": agent["turnId"],
                    "itemId": item_id, "delta": f"{chunk:03d}:{agent['name']},"}})
        for agent, item_id in records:
            sequence += 1
            events.append({"method": "item/completed", "params": {
                "threadId": agent["threadId"], "turnId": agent["turnId"],
                "item": {"id": item_id, "type": "commandExecution", "exitCode": 0}}})
        assert sequence == total
        server.enqueue(notify, {"method": "fixture/block"})
        dispatch = threading.Thread(target=server.dispatch, daemon=True)
        dispatch.start()
        assert blocked.wait(2)
        received_at = time.time()
        for seq, event in enumerate(events, 1):
            server.enqueue(notify, {**event, "_studioSupervisorSequence": seq,
                                    "_studioReceivedAt": received_at})
        server.proc.read_cursor = total
        queued = server.callbacks.qsize()
        release.set()
        server.callbacks.join()
        server.reader_done.set()
        dispatch.join(5)
        assert not dispatch.is_alive()
        assert server.proc.cursor == total, (server.proc.cursor, total)
        assert not server.proc.ack_pending
        assert [e["_studioSupervisorSequence"] for e in delivered] == list(range(1, total + 1))

        with runtime.read_db() as db:
            for agent, item_id in records:
                row = db.execute("SELECT record FROM runtime_items WHERE id=?",
                                 (agent["id"] + ":" + item_id,)).fetchone()
                text = json.loads(row[0])
                expected = "".join(f"{chunk:03d}:{agent['name']}," for chunk in range(deltas_per_agent))
                assert json.loads(text["text"])["aggregatedOutput"] == expected
        with runtime.db() as db:
            delta_count = db.execute("SELECT coalesce(sum(count),0) FROM analytics_notifications "
                                     "WHERE method='item/commandExecution/outputDelta'").fetchone()[0]
        assert delta_count == agents * deltas_per_agent

        sqlite_after = sqlite_snapshot()
        phase_rows = {method: {phase: {"calls": row[0], "totalMs": round(row[1], 2),
                                      "p95Ms": percentile(row[2:], .95),
                                      "maxMs": round(max(row[2:], default=0), 3)}
                               for (key_method, phase), row in probe.times.items()
                               if key_method == method}
                      for method in sorted({key[0] for key in probe.times})}
        result = {
            "agents": agents, "deltas": agents * deltas_per_agent,
            "queuedBeforeRelease": queued, "batchLimit": batch_limit,
            "callbackDelayP95MaxMs": {"p95": percentile(queue_delays, .95),
                                      "max": round(max(queue_delays, default=0), 3)},
            "phaseMsByMethod": phase_rows,
            "runtimeLock": {name: {"count": row[0], "waitMs": round(row[1]*1000, 2),
                                   "holdMs": round(row[2]*1000, 2)}
                            for name, row in traced.events.items()},
            "sqlite": sqlite_delta(sqlite_before, sqlite_after),
        }
        StreamBuffer.flush_locked = stream_flush_original
        codex_sync_entities.put = sync_put_original
        runtime.lock = traced.lock
        runtime.close()
        return result


def main():
    before = measure(batch_limit=1)
    after = measure(batch_limit=128)
    print(json.dumps({"before": before, "after": after}, indent=2))


if __name__ == "__main__":
    main()
