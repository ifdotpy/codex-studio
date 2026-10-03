#!/usr/bin/env python3
"""Isolated notification queue load and Runtime lock attribution. No model or user state."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import argparse
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
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
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


def measure(count=320, agents=24):
    with tempfile.TemporaryDirectory(prefix="studio-notification-load-") as temporary:
        root = Path(temporary)
        runtime = QuietRuntime(root / "state", server_factory=lambda *args: None)
        try:
            lead = runtime.create({"name": "Fixture", "cwd": temporary, "prompt": ""}, draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                original = runtime.agent(lead["id"], db)
                for index in range(agents):
                    agent = {**original, "id": str(uuid.uuid4()), "name": f"Agent {index}",
                             "threadId": f"fixture-thread-{index}", "turnId": f"fixture-turn-{index}",
                             "inFlight": True, "status": "running"}
                    runtime.put(db, "agents", agent)
            traced = MeasuredRLock(runtime.lock)
            runtime.lock = traced
            server = AppServer.__new__(AppServer)
            server.supervisor_mode = False
            server.callbacks = queue.Queue(maxsize=AppServer.CALLBACK_QUEUE_LIMIT)
            server.callback_lock = threading.RLock()
            server.dispatch_stopped = False
            server.reader_done = threading.Event()
            server.dispatcher_done = threading.Event()
            server.log = io.BytesIO()
            server.closed = True
            server.died = lambda: None
            delays, durations, connections = [], [], set()
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
                delays.append((message["_studioDispatchedAt"] - message["_studioReceivedAt"]) * 1000)

            server.notification = notify
            server.request = lambda message: None
            dispatch = threading.Thread(target=server.dispatch, daemon=True)
            dispatch.start()
            server.enqueue(notify, {"method": "fixture/block"})
            assert blocker.wait(2)
            for i in range(count):
                index = i % agents
                common = {"threadId": f"fixture-thread-{index}", "turnId": f"fixture-turn-{index}"}
                if i % 10 < 2:
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
                server.enqueue(notify, {"method": method, "params": params, "_studioReceivedAt": time.time()})
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
                    lambda: runtime.snapshot(include_work=False) for _ in range(12)])),
                threading.Thread(target=workload, args=("tool", [
                    lambda message=message: runtime.reserve_tool_request(message) for message in tools])),
                threading.Thread(target=workload, args=("scheduler", [
                    lambda: runtime.rules_tick() for _ in range(4)] +
                    [lambda: runtime.capacity_tick() for _ in range(4)] +
                    [lambda: runtime.usage_resume_tick() for _ in range(4)] +
                    [lambda: runtime.dispatch() for _ in range(2)])),
            ]
            release.set()
            for worker in workloads:
                worker.start()
            server.callbacks.join()
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
                    "dispatcherConnections": len(connections),
                    "queueDelayMs": {"p50": percentile(delays, .5), "p95": percentile(delays, .95),
                                     "max": percentile(delays, 1)},
                    "callbackDurationMs": {"p50": percentile(durations, .5), "p95": percentile(durations, .95)},
                    "lockWaitTotalMs": round(sum(row["totalWaitMs"] for row in metrics), 3),
                    "lockHoldTotalMs": round(sum(row["totalHoldMs"] for row in metrics), 3),
                    "lock": by_wait[:20]}
        finally:
            runtime.servers.clear()
            runtime.server = None
            runtime.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--baseline", type=Path, default=Path(__file__).resolve().parents[1] /
                        "docs/verification/2026-09-28-notification-baseline.json")
    parser.add_argument("--count", type=int, default=320)
    args = parser.parse_args()
    result = measure(args.count)
    if args.baseline:
        before = json.loads(args.baseline.read_text())
        result = {"before": before, "after": result}
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
