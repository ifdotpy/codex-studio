"""Bounded Runtime.lock wait and hold metrics."""

from array import array
import sys
import threading
import time
from types import FrameType


SAMPLE_LIMIT = 512
_SITE_CACHE = {}


class FixedSampleRing:
    """Bounded, named integer sample series shared by diagnostic recorders."""

    __slots__ = ("_limit", "_names", "_series", "count", "index", "samples")

    def __init__(self, limit: int, names: tuple[str, ...]) -> None:
        self._limit = limit
        self._names = names
        self._series = tuple(array("Q", [0]) * limit for _ in names)
        self.count = 0
        self.index = 0
        self.samples = 0

    def record(self, values: tuple[int, ...]) -> None:
        index = self.index
        for samples, value in zip(self._series, values):
            samples[index] = value
        self.index = (index + 1) % self._limit
        self.samples = min(self.samples + 1, self._limit)
        self.count += 1

    def snapshot(self) -> tuple[int, dict[str, tuple[int, ...]]]:
        return self.count, {name: tuple(samples[:self.samples])
                            for name, samples in zip(self._names, self._series)}


def percentile_summary(
    values: tuple[int, ...], *, include_p99: bool = False, divisor: int = 1_000_000
) -> dict[str, float | int]:
    """Summarize bounded samples in the requested unit."""
    ordered = sorted(values)
    summary: dict[str, float | int] = {"samples": len(ordered)}
    for key in (50, 95, 99)[:3 if include_p99 else 2]:
        index = min(len(ordered) - 1, (len(ordered) - 1) * key // 100)
        summary[f"p{key}"] = round(ordered[index] / divisor, 3) if ordered else 0.0
    summary["max"] = round(max(ordered, default=0) / divisor, 3)
    return summary


def _call_site(frame: FrameType) -> str:
    key = (frame.f_code, frame.f_lineno)
    site = _SITE_CACHE.get(key)
    if site is None:
        site = f"{frame.f_code.co_filename.rsplit('/', 1)[-1]}:{frame.f_code.co_name}:{frame.f_lineno}"
        _SITE_CACHE[key] = site
    return site


class _CallSiteMetrics:
    __slots__ = ("total_wait_ns", "total_hold_ns", "samples")

    def __init__(self):
        self.total_wait_ns = 0
        self.total_hold_ns = 0
        self.samples = FixedSampleRing(SAMPLE_LIMIT, ("wait", "hold"))

    def record(self, wait_ns: int, hold_ns: int) -> None:
        self.samples.record((wait_ns, hold_ns))
        self.total_wait_ns += wait_ns
        self.total_hold_ns += hold_ns


class MeasuredRLock:
    """Measure outer lock acquisitions by source call site."""

    def __init__(self, lock=None):
        self._lock = lock or threading.RLock()
        self._local = threading.local()
        self._metrics_lock = threading.Lock()
        self._metrics = {}

    def acquire(self, *args, **kwargs):
        depth = getattr(self._local, "depth", 0)
        site = None
        if depth == 0:
            caller = sys._getframe(1)
            if caller.f_code.co_name == "__enter__":
                caller = caller.f_back
            site = _call_site(caller)
        started = time.perf_counter_ns()
        acquired = self._lock.acquire(*args, **kwargs)
        if acquired:
            entered = time.perf_counter_ns()
            if depth == 0:
                self._local.site = site
                self._local.waitNs = entered - started
                self._local.holdStartedNs = entered
            self._local.depth = depth + 1
        return acquired

    def release(self):
        depth = getattr(self._local, "depth", 0)
        if depth < 1:
            self._lock.release()
            return
        if depth == 1:
            site = self._local.site
            wait_ns = self._local.waitNs
            hold_ns = time.perf_counter_ns() - self._local.holdStartedNs
            self._local.depth = 0
        else:
            self._local.depth = depth - 1
        self._lock.release()
        if depth == 1:
            with self._metrics_lock:
                row = self._metrics.get(site)
                if row is None:
                    row = self._metrics[site] = _CallSiteMetrics()
                row.record(wait_ns, hold_ns)

    def __enter__(self):
        self.acquire()
        return self

    def __exit__(self, *_exc):
        self.release()

    def _is_owned(self):
        return self._lock._is_owned()

    def _release_save(self):
        depth = getattr(self._local, "depth", 0)
        if depth:
            site = self._local.site
            wait_ns = self._local.waitNs
            hold_ns = time.perf_counter_ns() - self._local.holdStartedNs
            self._local.depth = 0
        state = self._lock._release_save()
        if depth:
            with self._metrics_lock:
                row = self._metrics.get(site)
                if row is None:
                    row = self._metrics[site] = _CallSiteMetrics()
                row.record(wait_ns, hold_ns)
        return state, depth

    def _acquire_restore(self, saved):
        state, depth = saved
        caller = sys._getframe(1)
        site = _call_site(caller)
        started = time.perf_counter_ns()
        self._lock._acquire_restore(state)
        entered = time.perf_counter_ns()
        self._local.depth = depth
        self._local.site = site
        self._local.waitNs = entered - started
        self._local.holdStartedNs = entered

    def __getattr__(self, name):
        return getattr(self._lock, name)

    def __repr__(self):
        return repr(self._lock)

    def runtime_lock_metrics(self):
        with self._metrics_lock:
            rows = [(site, row.total_wait_ns, row.total_hold_ns, row.samples.snapshot())
                    for site, row in self._metrics.items()]
        result = []
        for site, total_wait, total_hold, (count, samples) in sorted(rows):
            result.append({
                "callSite": site,
                "count": count,
                "totalWaitMs": round(total_wait / 1_000_000, 3),
                "totalHoldMs": round(total_hold / 1_000_000, 3),
                "waitMs": percentile_summary(samples["wait"]),
                "holdMs": percentile_summary(samples["hold"]),
            })
        return result


def runtime_lock():
    return MeasuredRLock(threading.RLock())
