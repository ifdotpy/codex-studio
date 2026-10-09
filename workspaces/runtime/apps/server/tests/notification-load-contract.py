#!/usr/bin/env python3
"""Isolated notification queue load and Runtime lock attribution. No model or user state."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import argparse
from contextlib import contextmanager
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
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from studio_api.testing import read_runtime_state
from codex_runtime import AppServer, Runtime
from codex_lock_metrics import MeasuredRLock


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(0.05)
            self.changed.clear()


def percentile(values, fraction):
    values = sorted(values)
    return round(values[int((len(values) - 1) * fraction)], 3) if values else 0


def measure(count=320, agents=24, storm=False, legacy_checkpoint=False,
            live_scheduler=False, idle_roster=0):
    with tempfile.TemporaryDirectory(prefix="studio-notification-load-") as temporary:
        root = Path(temporary)
        runtime_type = Runtime if live_scheduler else QuietRuntime
        runtime = runtime_type(root / "state", server_factory=lambda *args: None)
        try:
            lead = runtime.create({"name": "Fixture", "cwd": temporary, "prompt": ""}, draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                original = runtime.agent(lead["id"], db)
                for index in range(agents):
                    agent = {**original, "id": str(uuid.uuid4()), "name": f"Agent {index}",
                             "threadId": f"fixture-thread-{index}", "turnId": f"fixture-turn-{index}",
                             "inFlight": True, "status": "running"}
                    runtime.put(db, "agents", agent)
                if idle_roster:
                    db.executemany("INSERT INTO runtime_agents(id,record) VALUES (?,?)",
                                   ((f"idle-{index}", json.dumps({**original,
                                     "id": f"idle-{index}", "name": f"Idle {index}",
                                     "status": "waiting", "autoWake": False,
                                     "threadId": None, "turnId": None, "inFlight": False}))
                                    for index in range(idle_roster)))
            traced = MeasuredRLock(runtime.lock)
            runtime.lock = traced
            db_durations = []
            original_notification_db = runtime.notification_db
            @contextmanager
            def measured_notification_db():
                began = time.perf_counter()
                with original_notification_db() as db:
                    yield db
                db_durations.append((time.perf_counter() - began) * 1000)
            runtime.notification_db = measured_notification_db
            server = AppServer.__new__(AppServer)
            server.supervisor_mode = storm
            server.callbacks = queue.Queue(maxsize=AppServer.CALLBACK_QUEUE_LIMIT)
            server.callback_lock = threading.RLock()
            server.dispatch_stopped = False
            server.reader_done = threading.Event()
            server.dispatcher_done = threading.Event()
            server.log = io.BytesIO()
            if storm:
                from codex_process_supervisor import Journal as SupervisorJournal
                journal = SupervisorJournal(root / 'supervisor')
                with journal.db() as db:
                    db.execute("INSERT INTO handles(id,signature,pid,sequence,created,generation) "
                               "VALUES ('fixture','fixture',1,?,?,1)", (count, time.time()))
                    db.executemany("INSERT INTO events(handle,sequence,kind,payload,size,generation) "
                                   "VALUES ('fixture',?,'stdout','{}',2,1)",
                                   ((i + 1,) for i in range(count)))
                class Journal:
                    def ack(self, sequence):
                        with journal.db() as db:
                            db.execute("DELETE FROM events WHERE handle='fixture' AND sequence<=?", (sequence,))
                            db.execute("UPDATE handles SET acknowledged=? WHERE id='fixture'", (sequence,))
                            db.commit()
                        if legacy_checkpoint:
                            with journal.db() as db:
                                db.execute('PRAGMA wal_checkpoint(TRUNCATE)')
                        self.cursor = sequence
                    def ack_applied_deltas(self, applied):
                        return 0
                    def register_event_batch(self, sequences):
                        pass
                server.proc = Journal()
                server.supervisor_event_applied = lambda sequence: runtime.supervisor_event_applied('fixture', sequence)
                server.supervisor_commit = lambda message, sequence: runtime.commit_supervisor_event(
                    'fixture', message, sequence, 'default', None)
            server.closed = True
            server.died = lambda: None
            delays, durations, connections, callback_kinds = [], [], set(), {}
            blocker, release = threading.Event(), threading.Event()

            def notify(message):
                if message["method"] == "fixture/block":
                    blocker.set()
                    release.wait(30)
                    return
                start = time.perf_counter()
                runtime.notification(message)
                connection = getattr(runtime._callback_db, "connection", None)
                if connection is not None:
                    connections.add(id(connection))
                durations.append((time.perf_counter() - start) * 1000)
                callback_kinds.setdefault(message['method'], []).append(durations[-1])
                delays.append((message["_studioDispatchedAt"] - message["_studioReceivedAt"]) * 1000)

            server.notification = notify
            server.request = lambda message: None
            dispatch = threading.Thread(target=server.dispatch, daemon=True)
            dispatch.start()
            server.enqueue(notify, {"method": "fixture/block"})
            assert blocker.wait(2)
            input_started = time.perf_counter()
            for i in range(count):
                index = i % agents
                common = {"threadId": f"fixture-thread-{index}", "turnId": f"fixture-turn-{index}"}
                if storm:
                    phase = (i // agents) % 20
                    item_id = f"message-{i // (agents * 20)}-{index}"
                    if phase == 0:
                        method = 'item/started'
                        params = {**common, 'item': {'id': item_id, 'type': 'agentMessage'}}
                    elif phase == 19:
                        method = 'item/completed'
                        params = {**common, 'item': {'id': item_id, 'type': 'agentMessage',
                                                   'text': 'complete response'}}
                    else:
                        method = 'item/agentMessage/delta'
                        params = {**common, 'itemId': item_id, 'delta': 'response fragment\n'}
                elif i % 10 < 2:
                    method = "account/rateLimits/updated"
                    params = {"rateLimits": {"limitId": "codex", "primary": {"usedPercent": i % 100}}}
                elif i % 10 < 4:
                    method = "thread/tokenUsage/updated"
                    params = {**common, "tokenUsage": {"last": {"totalTokens": i + 1},
                              "total": {"totalTokens": i + 1}, "modelContextWindow": 100000}}
                elif i % 10 < 6:
                    method = "turn/diff/updated"
                    params = {**common, "diff": "file change\n" * 20}
                elif i % 10 < 8:
                    method = "item/started" if i % 2 else "item/completed"
                    params = {**common, "item": {"id": f"item-{i}", "type": "reasoning"}}
                else:
                    method = "thread/status/changed"
                    params = {**common, "status": {"type": "active"}}
                message = {"method": method, "params": params, "_studioReceivedAt": time.time()}
                if storm:
                    message['_studioSupervisorSequence'] = i + 1
                server.enqueue(notify, message)
            input_ended = time.perf_counter()
            queued = server.callbacks.qsize()
            workload_errors = []
            def workload(name, calls):
                try:
                    for call in calls:
                        call()
                except Exception as error:
                    workload_errors.append((name, repr(error)))
            main_thread = f"fixture-thread-0"
            tools = [{"id": f"tool-{i}", "method": "item/tool/call", "params": {
                "threadId": main_thread, "callId": f"call-{i}", "tool": "orchestration_status",
                "arguments": {}}} for i in range(12)]
            workloads = [
                threading.Thread(target=workload, args=("snapshot", [
                    lambda: read_runtime_state(runtime, include_work=False) for _ in range(12)])),
                threading.Thread(target=workload, args=("tool", [
                    lambda message=message: runtime.reserve_tool_request(message) for message in tools])),
                threading.Thread(target=workload, args=("scheduler", [
                    lambda: runtime.rules_tick() for _ in range(4)] +
                    [lambda: runtime.capacity_tick() for _ in range(4)] +
                    [lambda: runtime.usage_resume_tick() for _ in range(4)] +
                    [lambda: runtime.dispatch() for _ in range(2)])),
            ]
            release.set()
            processing_started = time.perf_counter()
            for worker in workloads:
                worker.start()
            server.callbacks.join()
            processing_ended = time.perf_counter()
            for worker in workloads:
                worker.join(10)
                assert not worker.is_alive()
            assert not workload_errors, workload_errors
            server.reader_done.set()
            dispatch.join(5)
            assert not dispatch.is_alive()
            assert len(delays) > 0
            assert len(connections) == 1, connections
            metrics = traced.runtime_lock_metrics()
            by_wait = sorted(metrics, key=lambda row: row["totalWaitMs"], reverse=True)
            return {"input": count, "callbacks": len(delays), "queuedBeforeRelease": queued,
                    "journalAcked": server.proc.cursor if storm else None,
                    "stormAgents": agents if storm else None,
                    "idleRoster": idle_roster,
                    "liveScheduler": live_scheduler,
                    "inputEventsPerSecond": round(count / max(.001, input_ended - input_started), 1),
                    "processedCallbacksPerSecond": round(len(delays) / max(.001, processing_ended - processing_started), 1),
                    "dispatcherConnections": len(connections),
                    "queueDelayMs": {"p50": percentile(delays, .5), "p95": percentile(delays, .95),
                                     "max": percentile(delays, 1)},
                    "callbackDurationMs": {"p50": percentile(durations, .5), "p95": percentile(durations, .95)},
                    "lockWaitTotalMs": round(sum(row["totalWaitMs"] for row in metrics), 3),
                    "lockHoldTotalMs": round(sum(row["totalHoldMs"] for row in metrics), 3),
                    "notificationDbMs": {"p50": percentile(db_durations, .5),
                                         "p95": percentile(db_durations, .95),
                                         "total": round(sum(db_durations), 3)},
                    "callbackKinds": {kind: {"count": len(values), "p50Ms": percentile(values, .5),
                                            "p95Ms": percentile(values, .95), "totalMs": round(sum(values), 3)}
                                      for kind, values in callback_kinds.items()},
                    "lock": by_wait[:20]}
        finally:
            runtime.servers.clear()
            runtime.server = None
            runtime.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", type=Path, default=REPOSITORY_ROOT /
                        "docs/verification/2026-09-28-notification-baseline.json")
    parser.add_argument("--count", type=int, default=320)
    parser.add_argument("--agents", type=int, default=24)
    parser.add_argument("--storm", action="store_true")
    parser.add_argument("--live-scheduler", action="store_true")
    parser.add_argument("--idle-roster", type=int, default=0)
    args = parser.parse_args()
    result = measure(args.count, args.agents, args.storm,
                     live_scheduler=args.live_scheduler, idle_roster=args.idle_roster)
    if args.baseline:
        before = json.loads(args.baseline.read_text())
        result = {"before": before, "after": result}
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
