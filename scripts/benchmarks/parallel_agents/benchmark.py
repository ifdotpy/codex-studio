#!/usr/bin/env python3
"""Measure a busy Runtime with queued native callbacks and 1,000 agents."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import queue
import resource
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from codex_runtime import Runtime

fixture_spec = importlib.util.spec_from_file_location(
    "parallel_runtime_fixture", Path(__file__).resolve().parents[3] / "tests" / "runtime-contract.py")
fixture = importlib.util.module_from_spec(fixture_spec)
fixture_spec.loader.exec_module(fixture)

AGENT_ROWS = 1000
ROUNDS = 3
DELTAS = 16
COMMAND_DELTAS = 8
CASE_TIMEOUT = 90


def percentile(values, fraction):
    ordered = sorted(values)
    return round(ordered[min(len(ordered) - 1, max(0, int(len(ordered) * fraction + .999999) - 1))], 3)


class MeasuredLock:
    def __init__(self, underlying):
        self.underlying = underlying
        self.local = threading.local()
        self.held_ns = 0
        self.holders = {}
        self.guard = threading.Lock()

    def __getattr__(self, key):
        return getattr(self.underlying, key)

    def acquire(self, *args, **kwargs):
        acquired = self.underlying.acquire(*args, **kwargs)
        if acquired:
            depth = getattr(self.local, "depth", 0)
            if depth == 0:
                self.local.start = time.monotonic_ns()
                frame = sys._getframe(1)
                if frame.f_code.co_name == "__enter__":
                    frame = frame.f_back
                self.local.caller = Path(frame.f_code.co_filename).name + ":" + frame.f_code.co_name
            self.local.depth = depth + 1
        return acquired

    def release(self):
        depth = self.local.depth - 1
        if depth == 0:
            elapsed = time.monotonic_ns() - self.local.start
            with self.guard:
                self.held_ns += elapsed
                self.holders[self.local.caller] = self.holders.get(self.local.caller, 0) + elapsed
        self.local.depth = depth
        self.underlying.release()

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_args):
        self.release()


class FakeServer(fixture.FakeServer):
    """A single native callback lane with bounded, observable input."""
    def __init__(self, root, notification, request, died):
        super().__init__(root, notification, request, died)
        self.callbacks = queue.Queue(maxsize=20000)
        self.latencies = []
        self.peak = 0
        self.errors = []
        self.measuring = False
        self.wake_cap = 0
        self.wake_starts = 0
        self.wake_turns = {}
        self.wake_existing = []
        self.callback_methods = {}
        self.closed = False
        self.worker = threading.Thread(target=self.dispatch, name="fake-app-server", daemon=True)
        self.worker.start()

    def offer(self, message):
        if message.get("method") == "turn/completed":
            params = message["params"]
            self.active_turns.pop(params["threadId"], None)
        message["_studioReceivedAt"] = time.time()
        admitted = time.monotonic_ns()
        self.callbacks.put((message, admitted), timeout=10)
        self.peak = max(self.peak, self.callbacks.qsize())

    def notify(self, message):
        self.offer(message)

    def call(self, method, params, timeout=60):
        existing = self.active_turns.get(params.get("threadId")) if method == "turn/start" else None
        result = super().call(method, params, timeout)
        if method == "turn/start" and self.measuring:
            if existing:
                self.wake_existing.append((params["threadId"], existing["id"]))
            if self.wake_starts >= self.wake_cap:
                raise RuntimeError("fake app-server exceeded the wake cap")
            self.wake_starts += 1
            thread = params["threadId"]
            turn = result["turn"]["id"]
            self.wake_turns[thread] = turn
            self.notify({"method": "item/completed", "params": {"threadId": thread,
                        "turnId": turn, "item": {"id": turn + "-answer", "type": "agentMessage",
                                                   "text": "Fixed wake result"}}})
            self.notify({"method": "turn/completed", "params": {"threadId": thread,
                        "turn": {"id": turn, "status": "completed"}}})
        return result

    def dispatch(self):
        while True:
            entry = self.callbacks.get()
            try:
                if entry is None:
                    return
                message, admitted = entry
                message["_studioDispatchedAt"] = time.time()
                self._notify(message)
                self.latencies.append((time.monotonic_ns() - admitted) / 1e6)
                method = message.get("method")
                self.callback_methods[method] = self.callback_methods.get(method, 0) + 1
            except Exception as error:
                self.errors.append(f"{type(error).__name__}: {error}")
            finally:
                self.callbacks.task_done()

    def close(self):
        if not self.closed:
            super().close()
            self.callbacks.put(None, timeout=10)

    def join_callbacks(self, timeout=10):
        self.worker.join(timeout)
        return not self.worker.is_alive()


def seed(runtime, root, active_count):
    active = []
    for index in range(active_count):
        agent = runtime.create({"name": f"Benchmark {index}", "cwd": str(root),
                                "prompt": "Parallel backend benchmark", "yolo_mode": True})
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            current = runtime.agent(agent["id"])
            if current["status"] == "running" and current.get("turnId"):
                active.append(current)
                break
            if current["status"] in {"failed", "interrupted"}:
                raise RuntimeError(f"agent start failed: {current.get('error')}")
            time.sleep(.01)
        else:
            raise TimeoutError("fake app-server did not start an agent")
    template = dict(active[0])
    template.update(status="completed", autoWake=False, inFlight=False, turnId=None,
                    role="reviewer", isLead=False, parentId=active[0]["id"],
                    rootId=active[0]["id"], prompt="Synthetic archived work " + "x" * 4000)
    with runtime.lock, runtime.db() as db:
        for index in range(AGENT_ROWS - active_count):
            record = {**template, "id": str(uuid.uuid5(uuid.NAMESPACE_URL, f"bench-archive-{index}")),
                      "threadId": f"bench-archive-thread-{index}", "name": f"Archived {index}"}
            runtime.put(db, "agents", record)
    with runtime.db() as db:
        count = db.execute("SELECT COUNT(*) FROM runtime_agents").fetchone()[0]
    if count != AGENT_ROWS:
        raise AssertionError(f"expected {AGENT_ROWS} agents, found {count}")
    return active


def workload(active):
    """Interleave one complete native turn per active agent."""
    per_agent = []
    for agent in active:
        thread, turn = agent["threadId"], agent["turnId"]
        events = []
        for round_index in range(ROUNDS):
            item_id = f"answer-{round_index}"
            events.append(("item/started", {"threadId": thread, "turnId": turn,
                           "item": {"id": item_id, "type": "agentMessage", "text": ""}}))
            for delta_index in range(DELTAS):
                events.append(("item/agentMessage/delta", {"threadId": thread, "turnId": turn,
                               "itemId": item_id, "delta": f"{round_index}:{delta_index} " * 20}))
            events.append(("item/completed", {"threadId": thread, "turnId": turn,
                           "item": {"id": item_id, "type": "agentMessage",
                                    "text": "".join(f"{round_index}:{i} " * 20 for i in range(DELTAS))}}))
        command_id = "command"
        events.append(("item/started", {"threadId": thread, "turnId": turn,
                       "item": {"id": command_id, "type": "commandExecution",
                                "command": "synthetic command", "status": "inProgress"}}))
        for delta_index in range(COMMAND_DELTAS):
            events.append(("item/commandExecution/outputDelta", {"threadId": thread,
                           "turnId": turn, "itemId": command_id, "delta": f"output {delta_index}\n"}))
        events.append(("item/completed", {"threadId": thread, "turnId": turn,
                       "item": {"id": command_id, "type": "commandExecution",
                                "command": "synthetic command", "status": "completed",
                                "aggregatedOutput": "".join(f"output {i}\n" for i in range(COMMAND_DELTAS)),
                                "exitCode": 0}}))
        events.append(("thread/tokenUsage/updated", {"threadId": thread, "turnId": turn,
                       "responseId": f"bench-response-{thread}", "tokenUsage": {
                           "total": {"inputTokens": 1200, "outputTokens": 300, "totalTokens": 1500},
                           "last": {"inputTokens": 1200, "outputTokens": 300, "totalTokens": 1500}}}))
        events.append(("turn/completed", {"threadId": thread,
                       "turn": {"id": turn, "status": "completed"}}))
        per_agent.append(events)
    return [{"method": method, "params": params}
            for position in range(len(per_agent[0]))
            for events in per_agent for method, params in [events[position]]]


def run_case(active_count, rate, mode="notifications", inject=None):
    if active_count not in (5, 15) or not 1 <= rate <= 1000:
        raise ValueError("agents must be 5 or 15; rate must be 1 to 1000")
    if mode not in {"notifications", "wakes"}:
        raise ValueError("mode must be notifications or wakes")
    original_home = os.environ.get("CODEX_HOME")
    with tempfile.TemporaryDirectory(prefix="studio-parallel-bench-") as temporary:
        root = Path(temporary) / "state"
        home = Path(temporary) / "profile"
        root.mkdir()
        home.mkdir()
        os.environ["CODEX_HOME"] = str(home)
        try:
            runtime = Runtime(root, FakeServer)
        finally:
            if original_home is None:
                os.environ.pop("CODEX_HOME", None)
            else:
                os.environ["CODEX_HOME"] = original_home
        try:
            active = seed(runtime, root, active_count)
            server = runtime.connect()
            if not isinstance(server, FakeServer):
                raise AssertionError("native transport was used")
            monitors = [runtime.monitor(a["id"], {"command": "synthetic monitor",
                                                   "wake_on": "exit" if mode == "wakes" else "failure"},
                                        key=f"bench-monitor-{a['id']}") for a in active]
            events = workload(active)
            server.callbacks.join()
            server.latencies.clear()
            server.callback_methods.clear()
            server.peak = 0
            # Setup needs the scheduler to start real fake-native turns. A fixed
            # dispatch cadence makes the measured load repeatable after setup.
            runtime.closed = True
            runtime.changed.set()
            runtime.scheduler.join(5)
            if runtime.scheduler.is_alive():
                raise RuntimeError("fixture scheduler did not stop")
            runtime.closed = False
            server.measuring = True
            server.wake_cap = active_count if mode == "wakes" else 0
            lock = MeasuredLock(runtime.lock)
            runtime.lock = lock
            runtime.ui_condition = threading.Condition(lock)
            ticker_stop = threading.Event()
            tick_errors = []
            ticks = []
            def tick():
                next_due = time.monotonic() + 1
                while not ticker_stop.wait(max(0, next_due - time.monotonic())):
                    try:
                        runtime.dispatch_all()
                        ticks.append(time.monotonic())
                    except Exception as error:
                        tick_errors.append(f"{type(error).__name__}: {error}")
                        return
                    next_due += 1
            ticker = threading.Thread(target=tick, name="benchmark-scheduler", daemon=True)
            ticker.start()
            cpu_start = time.process_time()
            start = time.monotonic_ns()
            for index, event in enumerate(events):
                if inject == "drop_last" and index == len(events) - 1:
                    continue
                due = start + int(index * 1e9 / rate)
                delay = (due - time.monotonic_ns()) / 1e9
                if delay > 0:
                    time.sleep(delay)
                server.offer(event)
                if mode == "notifications" and index == len(events) // 2:
                    for monitor in monitors:
                        runtime.finish_monitor(monitor["id"], 0, None)
            server.callbacks.join()
            if mode == "wakes":
                for monitor in monitors:
                    runtime.finish_monitor(monitor["id"], 0, None)
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    server.callbacks.join()
                    if server.wake_starts == active_count and all(
                        runtime.agent(a["id"]).get("lastCompletedTurn") == server.wake_turns.get(a["threadId"])
                        for a in active):
                        break
                    time.sleep(.02)
                else:
                    raise TimeoutError(f"wake turns incomplete: {server.wake_starts} of {active_count}")
            ticker_stop.set()
            ticker.join(10)
            if ticker.is_alive() or tick_errors:
                raise RuntimeError("scheduler failed: " + str(tick_errors[:1]))
            if server.errors:
                raise RuntimeError("callback failed: " + server.errors[0])
            if getattr(runtime, "_stream_buffer", None):
                with runtime.lock, runtime.db() as db:
                    runtime._stream_buffer.flush_locked(db, force=True)
            end = time.monotonic_ns()
            cpu = time.process_time() - cpu_start
            with runtime.db() as db:
                usage = db.execute("SELECT COUNT(*) FROM analytics_usage WHERE agent IN (" +
                                   ",".join("?" for _ in active) + ")",
                                   [a["id"] for a in active]).fetchone()[0]
                monitor_events = db.execute("SELECT COUNT(*) FROM runtime_monitors WHERE "
                                            "json_extract(record,'$.status')='completed'").fetchone()[0]
                items = db.execute("SELECT COUNT(*) FROM runtime_items WHERE agent IN (" +
                                   ",".join("?" for _ in active) + ") AND json_extract(record,'$.role')='assistant'",
                                   [a["id"] for a in active]).fetchone()[0]
            if usage != active_count or monitor_events != active_count or items < active_count * ROUNDS:
                raise AssertionError(f"lost work: usage={usage}, monitors={monitor_events}, answers={items}")
            expected_callbacks = len(events) + (3 * active_count if mode == "wakes" else 0)
            if len(server.latencies) != expected_callbacks:
                raise AssertionError(f"lost callbacks: {len(server.latencies)} of {expected_callbacks}; "
                                     f"methods={server.callback_methods}; reused={server.wake_existing}")
            elapsed = (end - start) / 1e9
            rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
            return {"mode": mode, "agents": active_count, "agentRows": AGENT_ROWS,
                    "notifications": len(events), "wakeStarts": server.wake_starts,
                    "ratePerSecond": rate, "elapsedSeconds": round(elapsed, 3),
                    "cpuSeconds": round(cpu, 3), "peakRssBytes": rss if sys.platform == "darwin" else rss * 1024,
                    "lockHeldShare": round(lock.held_ns / (end - start), 4),
                    "lockHoldersMs": {name: round(ns / 1e6, 3) for name, ns in
                                      sorted(lock.holders.items(), key=lambda pair: -pair[1])},
                    "notificationLatencyMs": {"p50": percentile(server.latencies, .5),
                                              "p95": percentile(server.latencies, .95)},
                    "queuePeak": server.peak, "queueDrained": server.callbacks.qsize() == 0,
                    "schedulerTicks": len(ticks),
                    "receipts": {"usage": usage, "monitorCompleted": monitor_events,
                                 "answerItems": items}}
        finally:
            runtime.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agents", nargs="+", type=int, default=[5, 15])
    parser.add_argument("--modes", nargs="+", choices=["notifications", "wakes"],
                        default=["notifications", "wakes"])
    parser.add_argument("--mode", choices=["notifications", "wakes"], help=argparse.SUPPRESS)
    parser.add_argument("--rate", type=int, default=120)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--case", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.case is not None:
        print(json.dumps(run_case(args.case, args.rate, args.mode or "notifications")))
        return
    if not args.agents or any(n not in (5, 15) for n in args.agents):
        parser.error("agents must be 5 or 15")
    rows = []
    for count in args.agents:
        for mode in args.modes:
            command = [sys.executable, __file__, "--case", str(count), "--mode", mode,
                       "--rate", str(args.rate)]
            result = subprocess.run(command, capture_output=True, text=True, timeout=CASE_TIMEOUT)
            if result.returncode:
                raise RuntimeError(f"{count} agents, {mode} failed: {result.stderr[-3000:]}")
            rows.append(json.loads(result.stdout))
    report = {"sourceRevision": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
              "workload": {"rounds": ROUNDS, "textDeltasPerRound": DELTAS,
                           "commandDeltas": COMMAND_DELTAS, "archivedAgentRows": AGENT_ROWS},
              "cases": rows}
    encoded = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(encoded + "\n")
    print(encoded)


if __name__ == "__main__":
    main()
