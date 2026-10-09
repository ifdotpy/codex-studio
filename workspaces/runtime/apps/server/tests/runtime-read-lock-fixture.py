#!/usr/bin/env python3
"""Measure read-path runtime lock holds with 625 agents and CPU load."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

sys.dont_write_bytecode = True
repo = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(repo / "tests"))
from test_isolation import isolate_api_schema_cache
isolate_api_schema_cache()
sys.path.insert(0, str(repo / "scripts"))
from studio_api.testing import read_runtime_state
from codex_canvas import Canvas, make_server
from codex_native_sweep import _account_busy, _protected
from codex_runtime import Runtime
from codex_sync import SyncStore

spec = importlib.util.spec_from_file_location(
    "read_fixture_server", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def percentile(values, fraction):
    values = sorted(values)
    return round(values[min(len(values) - 1, int((len(values) - 1) * fraction))], 3)


class MeasuredLock:
    def __init__(self, lock):
        self.lock = lock
        self.local = threading.local()
        self.holds = []

    def __repr__(self):
        return repr(self.lock)

    def acquire(self, *args, **kwargs):
        acquired = self.lock.acquire(*args, **kwargs)
        if acquired:
            depth = getattr(self.local, "depth", 0)
            self.local.depth = depth + 1
            if depth == 0:
                self.local.began = time.monotonic_ns()
        return acquired

    def release(self):
        depth = self.local.depth - 1
        self.local.depth = depth
        if depth == 0:
            self.holds.append((time.monotonic_ns() - self.local.began) / 1e6)
        self.lock.release()

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_):
        self.release()


def measure():
    with tempfile.TemporaryDirectory(prefix="studio-read-lock-") as temporary:
        root = Path(temporary)
        runtime = Runtime(root, fixture.FakeServer)
        runtime.closed = True
        runtime.changed.set()
        runtime.scheduler.join(timeout=5)
        assert not runtime.scheduler.is_alive()
        runtime.closed = False
        burners = []
        server = None
        try:
            canvas = Canvas(root)
            canvas.runtime = runtime
            server = make_server(canvas, 0)
            http_snapshot = next(cell.cell_contents for cell in server.RequestHandlerClass.do_GET.__closure__
                                 if callable(cell.cell_contents) and
                                 getattr(cell.cell_contents, "__name__", None) == "snapshot")
            lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                  draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                for index in range(625):
                    agent = dict(lead)
                    agent.update(id=f"fixture-{index:04d}", parentId=lead["id"],
                                 rootId=lead["id"], isLead=False, role="implementer",
                                 name=f"Worker {index}", status="completed", inFlight=False,
                                 threadId=f"thread-{index}", fixtureLargeRecord="x" * 8192)
                    runtime.put(db, "agents", agent)
                room_id = "broadcast:" + lead["id"]
                runtime.put(db, "rooms", {"id": room_id, "kind": "broadcast",
                                           "rootId": lead["id"], "updated": time.time()})
                for index in range(20):
                    db.execute("INSERT INTO runtime_chat_messages "
                               "(id,room,sender,text,created,deliveries) VALUES (?,?,?,?,?,?)",
                               (f"message-{index}", room_id, lead["id"], "message", time.time(), "{}"))
            SyncStore(runtime.db, lambda _key: {})
            traced = MeasuredLock(runtime.lock)
            runtime.lock = traced
            canvas.lock = traced
            # Four independent processes keep the fixture under CPU pressure.
            for _ in range(4):
                burners.append(subprocess.Popen([sys.executable, "-c",
                    "import time; end=time.monotonic()+90; "
                    "exec('while time.monotonic()<end: pass')"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
            operations = {
                "httpSnapshot": lambda: http_snapshot(False),
                "runtimeSnapshot": lambda: read_runtime_state(runtime, include_work=False),
                "chatRead": lambda: runtime.chat_read(room_id),
                "peers": lambda: runtime.peers(lead["id"]),
                "nativeSweepChecks": lambda: native_checks(runtime, lead["id"]),
                "records": lambda: agent_records(runtime),
            }
            result = {}
            for name, operation in operations.items():
                elapsed, holds = [], []
                for _ in range(10):
                    begin = time.monotonic_ns()
                    prior = len(traced.holds)
                    operation()
                    elapsed.append((time.monotonic_ns() - begin) / 1e6)
                    holds.append(sum(traced.holds[prior:]))
                result[name] = {"elapsedP50Ms": percentile(elapsed, .5),
                                "elapsedP90Ms": percentile(elapsed, .9),
                                "lockHoldP50Ms": percentile(holds, .5),
                                "lockHoldP90Ms": percentile(holds, .9)}
            stop = threading.Event()
            polled = [0]
            def poll_http():
                while not stop.is_set():
                    http_snapshot(False)
                    polled[0] += 1
            poller = threading.Thread(target=poll_http)
            poller.start()
            try:
                waits = []
                for index in range(40):
                    event_id = f"lock-probe-{index}"
                    entered = time.monotonic_ns()
                    runtime.dispatch_candidates(lead["id"], fast_event_ids=[event_id],
                                                fast_scheduled_at=entered, fast_entered_at=entered)
                    with runtime.db() as db:
                        row = db.execute("SELECT record FROM runtime_event_meta WHERE id=?",
                                         (event_id,)).fetchone()
                    mark = json.loads(row[0])["timing"]
                    waits.append((mark["fastLockedAt"] - mark["fastEnteredAt"]) / 1e6)
                result["fastLockWaitDuringHttpPoll"] = {
                    "p50Ms": percentile(waits, .5), "p90Ms": percentile(waits, .9),
                    "httpPolls": polled[0]}
            finally:
                stop.set()
                poller.join(timeout=10)
                assert not poller.is_alive()
            return {"agents": 626, "cpuBurners": 4, "operations": result}
        finally:
            for burner in burners:
                burner.terminate()
                burner.wait(timeout=5)
            if server is not None:
                server.server_close()
            runtime.close()


def native_checks(runtime, agent_id):
    with runtime.lock, runtime.db() as db:
        _account_busy(runtime, db, "default")
        _protected(runtime, db, "default", "thread-0001", time.time())


def agent_records(runtime):
    with runtime.lock, runtime.db() as db:
        return len(runtime.records(db, "agents", shared=True) if not os.environ.get("STUDIO_BASELINE")
                   else runtime.records(db, "agents"))


if __name__ == "__main__":
    print(json.dumps(measure()), flush=True)
