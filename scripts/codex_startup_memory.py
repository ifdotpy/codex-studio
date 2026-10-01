"""Opt-in process memory checkpoints for backend startup diagnosis."""

import json
import os
import resource
import subprocess
import sys
import threading
import time


ENABLED = os.environ.get("CODEX_CANVAS_STARTUP_MEMORY") == "1"
TRACE = ENABLED and os.environ.get("CODEX_CANVAS_STARTUP_TRACEMALLOC") == "1"
_seen = set()
_last_at = {}
_lock = threading.Lock()
_tracemalloc = None

if TRACE:
    import tracemalloc as _tracemalloc
    _tracemalloc.start(10)


def _current_rss_bytes():
    try:
        value = subprocess.check_output(
            ["ps", "-o", "rss=", "-p", str(os.getpid())],
            stderr=subprocess.DEVNULL,
        ).decode("ascii").strip()
        return int(value) * 1024
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def mark(stage, *, once=True, interval_seconds=0):
    """Write one small checkpoint only when startup memory logging is enabled."""
    if not ENABLED:
        return
    with _lock:
        if once and stage in _seen:
            return
        now = time.time()
        if not once and interval_seconds and now - _last_at.get(stage, 0) < interval_seconds:
            return
        _seen.add(stage)
        _last_at[stage] = now
    maximum = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if sys.platform == "darwin":
        peak = int(maximum)
    else:
        peak = int(maximum * 1024)
    event = {
        "kind": "startupMemory",
        "stage": stage,
        "at": now,
        "rssBytes": _current_rss_bytes(),
        "maxRssBytes": peak,
    }
    if TRACE:
        snapshot = _tracemalloc.take_snapshot()
        event["topAllocations"] = [
            {"file": str(item.traceback[0]), "bytes": item.size, "count": item.count}
            for item in snapshot.statistics("lineno")[:8]
        ]
    print("STARTUP_MEMORY " + json.dumps(event, separators=(",", ":")),
          file=sys.stderr, flush=True)
