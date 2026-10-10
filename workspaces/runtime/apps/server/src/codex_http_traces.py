"""Record slow API reads without query values, tokens, or message bodies."""
import copy
import json
import logging
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlsplit

from codex_sqlite_traces import _frames
from codex_private_paths import ensure_private_dir, protect_temp_file

SLOW_MS = 1000
ACTIVE_LIMIT = 128
HISTORY_LIMIT = 32
JOURNAL_LIMIT = 4 * 1024 * 1024
_LOCK = threading.Lock()
_PERSIST_LOCK = threading.Lock()
_ACTIVE = {}
_RECENT = []
_SEQUENCE = 0
_REVISION = 0
_SAVED = None
_ARCHIVE_CHECKED = None
_STARTED_AT = time.time()
_UNTRACKED = 0
# Only these constant routes can identify the global connection error.
_ROUTES = frozenset(("/api/session", "/api/sync/identity",
                     "/api/sync/pull"))


def report_failure(error):
    try:
        logging.getLogger("codex.http").warning("HTTP request journal failed: %s", type(error).__name__)
    except Exception:
        pass  # A diagnostic logger must not change the API response.


def _archive_previous(path):
    global _ARCHIVE_CHECKED
    if _ARCHIVE_CHECKED == str(path):
        return
    try:
        with path.open("rb") as source:
            raw = source.read(JOURNAL_LIMIT + 1)
    except FileNotFoundError:
        _ARCHIVE_CHECKED = str(path)
        return
    if len(raw) > JOURNAL_LIMIT:
        raise OSError("The previous HTTP journal exceeds its size limit")
    previous = json.loads(raw)
    if not isinstance(previous, dict) or previous.get("version") != 1:
        raise OSError("The previous HTTP journal has an unknown version")
    for name, limit in (("active", ACTIVE_LIMIT), ("recent", HISTORY_LIMIT)):
        if not isinstance(previous.get(name), list) or len(previous[name]) > limit:
            raise OSError("The previous HTTP journal exceeds its record limit")
    same_session = previous.get("pid") == os.getpid() and previous.get("coverageStartedAt") == _STARTED_AT
    if not same_session and (previous["active"] or previous["recent"]):
        os.replace(path, path.with_name("http-requests.previous.json"))
    _ARCHIVE_CHECKED = str(path)


def begin(method, target):
    global _SEQUENCE, _UNTRACKED
    if method != "GET":
        return None
    try:
        parsed = urlsplit(target[:2048])
    except ValueError:
        return None
    if parsed.path not in _ROUTES:
        return None
    entry = {"endpoint": parsed.path, "method": "GET"}
    if parsed.path == "/api/sync/pull":
        try:
            scope = parse_qs(parsed.query, max_num_fields=32).get("scope", [""])[0]
        except ValueError:
            scope = ""
        entry["scope"] = (scope if scope in {"state:entities:v1", "drafts"}
                          else "transcript" if scope.startswith("transcript:") else "unknown")
    with _LOCK:
        if len(_ACTIVE) >= ACTIVE_LIMIT:
            _UNTRACKED += 1
            return None
        _SEQUENCE += 1
        key = f"{os.getpid()}:{_SEQUENCE}"
        _ACTIVE[key] = {**entry, "id": key, "threadId": threading.get_ident(),
                        "startedAt": time.time(), "startedAtMonotonic": time.monotonic()}
        return key


def finish(key, status, outcome):
    global _REVISION
    if key is None:
        return
    with _LOCK:
        entry = _ACTIVE.pop(key, None)
        if entry is None:
            return
        duration = (time.monotonic() - entry["startedAtMonotonic"]) * 1000
        if duration >= SLOW_MS:
            _RECENT.append({**entry, "durationMs": round(duration, 3),
                            "status": status, "outcome": outcome, "finishedAt": time.time()})
            del _RECENT[:-HISTORY_LIMIT]
            _REVISION += 1


def discard(key):
    """Retire a completed request when the diagnostic completion failed."""
    with _LOCK:
        _ACTIVE.pop(key, None)


def watchdog(root):
    """Save active waits on the existing update tick, outside Runtime.lock."""
    global _SAVED
    if not _PERSIST_LOCK.acquire(blocking=False):
        return
    temporary = None
    try:
        with _LOCK:
            now = time.monotonic()
            active = [copy.deepcopy(entry) for entry in _ACTIVE.values()
                      if (now - entry["startedAtMonotonic"]) * 1000 >= SLOW_MS]
            marker = (str(root), _REVISION, tuple(row["id"] for row in active))
            if marker == _SAVED and not active:
                return
            recent = copy.deepcopy(_RECENT)
            untracked = _UNTRACKED
        frames = sys._current_frames() if active else {}
        try:
            for entry in active:
                entry["durationMs"] = round((now - entry["startedAtMonotonic"]) * 1000, 3)
                entry["frames"] = _frames(frames.get(entry["threadId"]))
        finally:
            frames.clear()
        # Keep the last wait location after completion, without holding frames.
        with _LOCK:
            active = [entry for entry in active if entry["id"] in _ACTIVE]
            for entry in active:
                _ACTIVE[entry["id"]]["frames"] = entry["frames"]
        value = {"version": 1, "pid": os.getpid(), "coverageStartedAt": _STARTED_AT,
                 "at": time.time(), "thresholdMs": SLOW_MS, "active": active,
                 "recent": recent, "untrackedStarts": untracked}
        path = Path(root) / "diagnostics" / "http-requests.json"
        ensure_private_dir(path.parent)
        _archive_previous(path)
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, prefix=".http-requests-",
                                         delete=False) as output:
            temporary = output.name
            protect_temp_file(temporary)
            json.dump(value, output, separators=(",", ":"))
            output.write("\n")
        os.replace(temporary, path)
        temporary = None
        _SAVED = marker
    except Exception as error:
        report_failure(error)
    finally:
        if temporary is not None:
            try:
                os.unlink(temporary)
            except OSError:
                pass
        _PERSIST_LOCK.release()
