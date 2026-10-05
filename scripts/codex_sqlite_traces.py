"""Keep bounded SQLite owner evidence without SQL text or connection ownership."""
import copy
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import weakref

_clock = time.monotonic
_wall_clock = time.time
_LOCK = threading.Lock()
_PERSIST_LOCK = threading.Lock()
_ACTIVE = {}
_RECENT = []
_LONGEST = []
_SEQUENCE = 0
_UNTRACKED_STARTS = 0
_REVISION = 0
_SAVED = None
_ARCHIVE_CHECKED = None
_STARTED_AT = _wall_clock()
_JOURNAL = {"status": "notWritten"}
SLOW_MS = 1000.0
ACTIVE_LIMIT = 256
HISTORY_LIMIT = 64
LONGEST_LIMIT = 8
# This also covers the worst JSON escaping of all bounded frame strings.
JOURNAL_LIMIT = 64 * 1024 * 1024


def database_kind(database):
    name = str(database).split("?", 1)[0].rsplit("/", 1)[-1]
    return name if name in {"canvas.sqlite3", "analytics.sqlite3"} else "other"


def _frames(frame):
    result = []
    try:
        for _ in range(12):
            if frame is None:
                break
            code = frame.f_code
            name = os.path.basename(code.co_filename)
            if name not in {"codex_sqlite.py", "codex_sqlite_traces.py", "contextlib.py"}:
                result.append({"file": name[-160:], "function": code.co_name[:120], "line": frame.f_lineno})
            frame = frame.f_back
            if len(result) >= 8:
                break
    finally:
        frame = None
    return result


def begin_transaction(db, started, site, statement):
    """Record the accepted transaction's caller without retaining stack frames."""
    global _SEQUENCE, _UNTRACKED_STARTS
    thread = threading.current_thread()
    frames = _frames(sys._getframe(1))
    database = getattr(db, "_codex_database_kind", None)
    if database is None:
        # Reused connections from before a live update have only their old site.
        database = {"Runtime.db": "canvas.sqlite3", "Runtime.analytics": "analytics.sqlite3"}.get(
            getattr(db, "_codex_site", None), "unknown")
    kind = statement.upper() if statement.upper() in {
        "BEGIN", "INSERT", "UPDATE", "DELETE", "REPLACE", "CREATE", "DROP", "ALTER", "WITH", "SAVEPOINT"
    } else "other"
    with _LOCK:
        _SEQUENCE += 1
        entry = {"id": f"{os.getpid()}:{_SEQUENCE}", "pid": os.getpid(), "site": site[:160],
                 "database": database,
                 "threadId": thread.ident, "nativeThreadId": threading.get_native_id(),
                 "threadName": thread.name[:120], "originFrames": frames,
                 "firstStatement": kind, "startedAtMonotonic": started,
                 "startedAt": _wall_clock(), "observations": [], "originRecorded": True}
        db._codex_transaction_trace = entry
        if len(_ACTIVE) < ACTIVE_LIMIT:
            def discarded(_reference, transaction_id=entry["id"]):
                with _LOCK:
                    _ACTIVE.pop(transaction_id, None)
            _ACTIVE[entry["id"]] = (weakref.ref(db, discarded), entry)
        else:
            _UNTRACKED_STARTS += 1


def _remember(entry):
    global _REVISION
    _RECENT.append(copy.deepcopy(entry))
    del _RECENT[:-HISTORY_LIMIT]
    _LONGEST.append(copy.deepcopy(entry))
    _LONGEST.sort(key=lambda row: row["durationMs"], reverse=True)
    del _LONGEST[LONGEST_LIMIT:]
    _REVISION += 1


def end_transaction(db, elapsed_ms, outcome):
    """Keep completed slow scopes, including commits and rollbacks."""
    entry = getattr(db, "_codex_transaction_trace", None)
    db._codex_transaction_trace = None
    if entry is None:
        return  # A transaction that predates this recorder has no start evidence.
    with _LOCK:
        _ACTIVE.pop(entry["id"], None)
        if elapsed_ms >= SLOW_MS:
            entry = {**entry, "durationMs": round(elapsed_ms, 3), "state": outcome,
                     "finishedAt": _wall_clock()}
            _remember(entry)


def history():
    with _LOCK:
        return {"coverageStartedAt": _STARTED_AT, "thresholdMs": SLOW_MS,
                "recent": copy.deepcopy(_RECENT), "longest": copy.deepcopy(_LONGEST),
                "journal": dict(_JOURNAL), "activeTracking": {
                    "tracked": len(_ACTIVE), "limit": ACTIVE_LIMIT,
                    "untrackedStarts": _UNTRACKED_STARTS}}


def active_transactions():
    """Copy owner evidence without inspecting another thread's SQLite connection."""
    with _LOCK:
        entries = [copy.deepcopy(entry) for _, entry in _ACTIVE.values()]
    now = _clock()
    frames = sys._current_frames()
    try:
        for entry in entries:
            entry["durationMs"] = round(max(0, now - entry["startedAtMonotonic"]) * 1000, 3)
            entry["frames"] = _frames(frames.get(entry["threadId"]))
    finally:
        frames.clear()
        frames = None
    with _LOCK:
        return [entry for entry in entries if entry["id"] in _ACTIVE]


def _archive_previous(path):
    """Retain three past snapshots and the compatible previous-session file."""
    from hashlib import sha256

    global _ARCHIVE_CHECKED
    if _ARCHIVE_CHECKED == str(path):
        return

    def read_snapshot(source_path):
        try:
            with source_path.open("rb") as source:
                raw = source.read(JOURNAL_LIMIT + 1)
        except FileNotFoundError:
            return None
        if len(raw) > JOURNAL_LIMIT:
            raise OSError("The previous SQLite journal exceeds its size limit")
        try:
            snapshot = json.loads(raw)
        except (ValueError, UnicodeError) as error:
            raise OSError("The previous SQLite journal is invalid") from error
        if not isinstance(snapshot, dict) or snapshot.get("version") != 1:
            raise OSError("The previous SQLite journal has an unknown version")
        fields = {"active": ACTIVE_LIMIT, "recent": HISTORY_LIMIT, "longest": LONGEST_LIMIT}
        for name, limit in fields.items():
            if not isinstance(snapshot.get(name), list) or len(snapshot[name]) > limit:
                raise OSError("The previous SQLite journal exceeds its record limit")
        return raw, snapshot

    current = read_snapshot(path)
    if current is None:
        _ARCHIVE_CHECKED = str(path)
        return
    raw, previous = current
    same_session = previous.get("pid") == os.getpid() and previous.get("coverageStartedAt") == _STARTED_AT
    if not same_session and any(previous[name] for name in ("active", "recent", "longest")):
        previous_path = path.with_name("sqlite-transactions.previous.json")
        older = read_snapshot(previous_path)
        snapshots = ([older] if older is not None else []) + [current]
        # Validate both originals before changing any file. Copy exact bytes so
        # owner IDs, process times and frame evidence remain unchanged.
        for saved_raw, snapshot in snapshots:
            if not any(snapshot[name] for name in ("active", "recent", "longest")):
                continue
            archive = path.with_name("sqlite-transactions.history." + sha256(saved_raw).hexdigest() + ".json")
            try:
                with archive.open("rb") as source:
                    existing = source.read(JOURNAL_LIMIT + 1)
            except FileNotFoundError:
                existing = None
            if existing is not None:
                if existing != saved_raw:
                    raise OSError("The SQLite history archive identity differs")
                continue
            temporary = None
            try:
                with tempfile.NamedTemporaryFile(mode="wb", dir=path.parent,
                                                 prefix=".sqlite-transactions-archive-", delete=False) as output:
                    temporary = output.name
                    os.fchmod(output.fileno(), 0o600)
                    output.write(saved_raw)
                    output.flush()
                    os.fsync(output.fileno())
                os.replace(temporary, archive)
                temporary = None
            finally:
                if temporary is not None:
                    try:
                        os.unlink(temporary)
                    except OSError:
                        pass
        archives = []
        prefix = "sqlite-transactions.history."
        for archive in path.parent.glob(prefix + "*.json"):
            identity = archive.name[len(prefix):-5]
            if len(identity) == 64 and all(character in "0123456789abcdef" for character in identity):
                archives.append(archive)
        archives.sort(key=lambda item: (item.stat().st_mtime_ns, item.name), reverse=True)
        for archive in archives[3:]:
            archive.unlink()
        os.replace(path, previous_path)
    _ARCHIVE_CHECKED = str(path)


def transaction_watchdog(root):
    """Use the existing update tick to save held and completed slow transactions."""
    global _REVISION, _SAVED, _JOURNAL
    try:
        from codex_http_traces import watchdog as http_watchdog
        http_watchdog(root)
    except Exception as error:
        import logging
        logging.getLogger("codex.http").warning("HTTP request journal tick failed: %s", type(error).__name__)
    if not _PERSIST_LOCK.acquire(blocking=False):
        return
    try:
        with _LOCK:
            candidates = list(_ACTIVE.values())
        active = []
        thread_frames = None
        try:
            for _, entry in candidates:
                duration = max(0, _clock() - entry["startedAtMonotonic"]) * 1000
                if duration < SLOW_MS:
                    continue
                if thread_frames is None:
                    thread_frames = sys._current_frames()
                observed = {"at": _wall_clock(), "durationMs": round(duration, 3),
                            "frames": _frames(thread_frames.get(entry["threadId"]))}
                # Completion can race the frame scan. Never resurrect that scope.
                with _LOCK:
                    if entry["id"] not in _ACTIVE:
                        continue
                    entry["observations"].append(observed)
                    del entry["observations"][:-4]
                    active.append({**copy.deepcopy(entry), "durationMs": round(duration, 3), "state": "active"})
                    _REVISION += 1
        finally:
            if thread_frames is not None:
                thread_frames.clear()
            thread_frames = None
        root = Path(root)
        with _LOCK:
            marker = (str(root), _REVISION, tuple(row["id"] for row in active))
            if marker == _SAVED:
                return
            value = {"version": 1, "pid": os.getpid(), "coverageStartedAt": _STARTED_AT,
                     "at": _wall_clock(), "thresholdMs": SLOW_MS, "active": active,
                     "recent": copy.deepcopy(_RECENT), "longest": copy.deepcopy(_LONGEST)}
        path = root / "diagnostics" / "sqlite-transactions.json"
        temporary = None
        try:
            path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            _archive_previous(path)
            with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, prefix=".sqlite-transactions-",
                                             delete=False) as output:
                temporary = output.name
                os.fchmod(output.fileno(), 0o600)
                json.dump(value, output, separators=(",", ":"))
                output.write("\n")
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, path)
            temporary = None
            previous_available = path.with_name("sqlite-transactions.previous.json").exists()
            with _LOCK:
                _SAVED = marker
                _JOURNAL = {"status": "written", "at": value["at"],
                            "previousSnapshotAvailable": previous_available}
        except OSError as error:
            with _LOCK:
                _JOURNAL = {"status": "error", "errorType": type(error).__name__}
        finally:
            if temporary is not None:
                try:
                    os.unlink(temporary)
                except OSError:
                    pass
    finally:
        _PERSIST_LOCK.release()
