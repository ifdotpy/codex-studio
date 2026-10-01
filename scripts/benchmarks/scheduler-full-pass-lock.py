#!/usr/bin/env python3
"""Measure one scheduler pass against a disposable copied Studio state directory."""
import importlib.util
import json
from pathlib import Path
import sys
import threading
import time

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from codex_runtime import Runtime

spec = importlib.util.spec_from_file_location(
    "scheduler_benchmark_fixture", SCRIPTS.parent / "tests" / "runtime-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class PassiveRuntime(Runtime):
    def schedule(self):
        return

    def start(self, *args, **kwargs):
        return

    def run_native_action(self, *args, **kwargs):
        return


class MeasuredLock:
    def __init__(self, lock):
        self.lock = lock
        self.local = threading.local()
        self.holds = []

    def __getattr__(self, name):
        return getattr(self.lock, name)

    def __repr__(self):
        return repr(self.lock)

    def acquire(self, *args, **kwargs):
        acquired = self.lock.acquire(*args, **kwargs)
        if acquired:
            depth = getattr(self.local, "depth", 0)
            if depth == 0:
                caller = sys._getframe(1)
                if caller.f_code.co_name == "__enter__":
                    caller = caller.f_back
                self.local.caller = caller.f_code.co_name
                self.local.began = time.monotonic_ns()
            self.local.depth = depth + 1
        return acquired

    def release(self):
        self.local.depth -= 1
        if self.local.depth == 0:
            self.holds.append((self.local.caller,
                               (time.monotonic_ns() - self.local.began) / 1e6))
        self.lock.release()

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_):
        self.release()


def main():
    if len(sys.argv) not in {2, 3, 4}:
        raise SystemExit("usage: scheduler-full-pass-lock.py DISPOSABLE_STATE_DIR [baseline|optimized] [passes]")
    root = Path(sys.argv[1]).resolve()
    mode = sys.argv[2] if len(sys.argv) >= 3 else "optimized"
    passes = int(sys.argv[3]) if len(sys.argv) == 4 else 2
    if mode not in {"baseline", "optimized"}:
        raise SystemExit("mode must be baseline or optimized")
    if passes < 1:
        raise SystemExit("passes must be positive")
    database = root / "canvas.sqlite3"
    if not database.is_file():
        raise SystemExit(f"missing copied database: {database}")
    runtime = PassiveRuntime(root, fixture.FakeServer)
    try:
        runtime.accounts.get = lambda _key: {"disconnected": True, "provider": "codex"}
        if mode == "baseline":
            runtime.scheduler_agents = lambda db: runtime.records(db, "agents")
        runtime.closed = True
        runtime.changed.set()
        runtime.scheduler.join(timeout=5)
        if runtime.scheduler.is_alive():
            raise RuntimeError("passive scheduler did not stop")
        runtime.closed = False
        measured = MeasuredLock(runtime.lock)
        runtime.lock = measured
        pass_results = []
        for _ in range(passes):
            prior = len(measured.holds)
            began = time.monotonic_ns()
            runtime.dispatch_all()
            elapsed = (time.monotonic_ns() - began) / 1e6
            holds = {}
            for name, ms in measured.holds[prior:]:
                holds[name] = holds.get(name, 0.0) + ms
            pass_results.append({"passMs": round(elapsed, 3),
                                 "lockHoldsMs": {k: round(v, 3) for k, v in holds.items()},
                                 "measuredLockHoldMs": round(sum(holds.values()), 3)})
        with runtime.db() as db:
            count = db.execute("SELECT COUNT(*) FROM runtime_agents").fetchone()[0]
        print(json.dumps({"mode": mode, "agents": count, "passes": pass_results}, indent=2))
    finally:
        runtime.close()


if __name__ == "__main__":
    main()
