"""Opt-in, bounded Runtime.lock wait and hold metrics."""

from collections import defaultdict, deque
import os
import sys
import threading
import time


SAMPLE_LIMIT = 512


class MeasuredRLock:
    """Measure outer lock acquisitions by source call site."""

    def __init__(self, lock=None):
        self._lock = lock or threading.RLock()
        self._local = threading.local()
        self._metrics_lock = threading.Lock()
        self._metrics = defaultdict(lambda: {
            "count": 0, "totalWaitNs": 0, "totalHoldNs": 0,
            "wait": deque(maxlen=SAMPLE_LIMIT), "hold": deque(maxlen=SAMPLE_LIMIT),
        })

    @staticmethod
    def _site():
        caller = sys._getframe(1)
        while caller.f_code.co_name in {"acquire", "__enter__", "_acquire_restore"}:
            caller = caller.f_back
        return f"{caller.f_code.co_filename.rsplit('/', 1)[-1]}:{caller.f_code.co_name}:{caller.f_lineno}"

    def acquire(self, *args, **kwargs):
        depth = getattr(self._local, "depth", 0)
        started = time.perf_counter_ns()
        acquired = self._lock.acquire(*args, **kwargs)
        if acquired:
            entered = time.perf_counter_ns()
            if depth == 0:
                self._local.site = self._site()
                self._local.waitNs = entered - started
                self._local.holdStartedNs = entered
            self._local.depth = depth + 1
        return acquired

    def release(self):
        depth = getattr(self._local, "depth", 0)
        if depth < 1:
            self._lock.release()
            return
        sample = None
        if depth == 1:
            sample = (self._local.site, self._local.waitNs,
                      time.perf_counter_ns() - self._local.holdStartedNs)
            self._local.depth = 0
        else:
            self._local.depth = depth - 1
        self._lock.release()
        if sample:
            site, wait_ns, hold_ns = sample
            with self._metrics_lock:
                row = self._metrics[site]
                row["count"] += 1
                row["totalWaitNs"] += wait_ns
                row["totalHoldNs"] += hold_ns
                row["wait"].append(wait_ns)
                row["hold"].append(hold_ns)

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_exc):
        self.release()

    def _is_owned(self):
        return self._lock._is_owned()

    def _release_save(self):
        depth = getattr(self._local, "depth", 0)
        sample = None
        if depth:
            sample = (self._local.site, self._local.waitNs,
                      time.perf_counter_ns() - self._local.holdStartedNs)
            self._local.depth = 0
        state = self._lock._release_save()
        if sample:
            site, wait_ns, hold_ns = sample
            with self._metrics_lock:
                row = self._metrics[site]
                row["count"] += 1
                row["totalWaitNs"] += wait_ns
                row["totalHoldNs"] += hold_ns
                row["wait"].append(wait_ns)
                row["hold"].append(hold_ns)
        return state, depth

    def _acquire_restore(self, saved):
        state, depth = saved
        started = time.perf_counter_ns()
        self._lock._acquire_restore(state)
        entered = time.perf_counter_ns()
        self._local.depth = depth
        self._local.site = self._site()
        self._local.waitNs = entered - started
        self._local.holdStartedNs = entered

    def __getattr__(self, name):
        return getattr(self._lock, name)

    def __repr__(self):
        return repr(self._lock)

    @staticmethod
    def _percentile(values, fraction):
        ordered = sorted(values)
        if not ordered:
            return 0.0
        index = min(len(ordered) - 1, int((len(ordered) - 1) * fraction))
        return round(ordered[index] / 1_000_000, 3)

    def runtime_lock_metrics(self):
        with self._metrics_lock:
            rows = [(site, row["count"], row["totalWaitNs"], row["totalHoldNs"],
                     tuple(row["wait"]), tuple(row["hold"]))
                    for site, row in self._metrics.items()]
        return [{
            "callSite": site,
            "count": count,
            "totalWaitMs": round(total_wait / 1_000_000, 3),
            "totalHoldMs": round(total_hold / 1_000_000, 3),
            "waitMs": {"p50": self._percentile(wait, .50),
                       "p95": self._percentile(wait, .95),
                       "max": round(max(wait, default=0) / 1_000_000, 3),
                       "samples": len(wait)},
            "holdMs": {"p50": self._percentile(hold, .50),
                       "p95": self._percentile(hold, .95),
                       "max": round(max(hold, default=0) / 1_000_000, 3),
                       "samples": len(hold)},
        } for site, count, total_wait, total_hold, wait, hold in sorted(rows)]


def runtime_lock():
    lock = threading.RLock()
    if os.environ.get("CODEX_RUNTIME_LOCK_METRICS") == "1":
        return MeasuredRLock(lock)
    return lock
