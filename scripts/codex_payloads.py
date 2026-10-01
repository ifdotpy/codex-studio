"""Content-addressed storage for exceptionally large SQLite JSON fields."""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile
import threading


EXTERNALIZE_THRESHOLD = 64 * 1024
TASK_RETENTION_SECONDS = 7 * 24 * 60 * 60
TASK_NEWEST_PER_AGENT = 100
TASK_PREVIEW_BYTES = 512
GC_GRACE_SECONDS = 7 * 24 * 60 * 60
FREE_SPACE_RESERVE = 64 * 1024 * 1024

_db_locks: dict[int, tuple[object, object]] = {}
_db_locks_guard = threading.Lock()


def _blob_root(state_dir: str | os.PathLike) -> Path:
    return Path(state_dir) / "blobs"


def state_root(runtime) -> Path:
    root = getattr(runtime, "root", None)
    if root is not None:
        return Path(root)
    database = getattr(runtime, "db_path", None) or getattr(runtime, "path", None)
    return Path(database).parent if database is not None else Path(".")


def _lock_db_writer(state_dir: str | os.PathLike, db) -> None:
    """Hold a shared filesystem lock until the owning SQLite transaction ends."""
    key = id(db)
    with _db_locks_guard:
        if key in _db_locks:
            return
        root = _blob_root(state_dir)
        _mkdir_durable(root)
        handle = (root / ".gc.lock").open("a+b")
        fcntl.flock(handle.fileno(), fcntl.LOCK_SH)
        _db_locks[key] = (db, handle)


def release_db_writer_lock(db) -> None:
    with _db_locks_guard:
        entry = _db_locks.pop(id(db), None)
    if entry:
        handle = entry[1]
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()


def _path(state_dir: str | os.PathLike, digest: str) -> Path:
    return _blob_root(state_dir) / digest[:2] / digest[2:4] / digest


def _check_free_space(directory: Path, required: int) -> None:
    free = shutil.disk_usage(directory).free
    if free < required + FREE_SPACE_RESERVE:
        raise OSError(f"Not enough free space for payload blob: need {required + FREE_SPACE_RESERVE} bytes")


def _fsync_directory(directory: Path) -> None:
    descriptor = os.open(directory, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def _mkdir_durable(directory: Path) -> None:
    missing = []
    current = directory
    while not current.exists():
        missing.append(current)
        current = current.parent
    for path in reversed(missing):
        try:
            path.mkdir()
        except FileExistsError:
            pass
        _fsync_directory(path)
        _fsync_directory(path.parent)


def _small_preview(value, depth: int = 0):
    """Keep a bounded, useful JSON-shaped preview beside an external blob."""
    if isinstance(value, str):
        raw = value.encode("utf-8")
        return raw[:512].decode("utf-8", errors="ignore") + ("…" if len(raw) > 512 else "")
    if isinstance(value, list):
        return [_small_preview(item, depth + 1) for item in value[:1]]
    if isinstance(value, dict):
        if depth >= 4:
            return "[preview omitted]"
        return {key: _small_preview(item, depth + 1)
                for key, item in list(value.items())[:8]}
    if value is None or isinstance(value, (bool, int, float)):
        return value
    return str(value)[:128]


def store_bytes(state_dir: str | os.PathLike, raw: bytes, *, db=None) -> dict:
    """Store bytes atomically, fsyncing each new file and its containing directory."""
    _check_free_space(Path(state_dir), len(raw))
    root = _blob_root(state_dir)
    if db is not None:
        _lock_db_writer(state_dir, db)
    else:
        _mkdir_durable(root)
    digest = hashlib.sha256(raw).hexdigest()
    target = _path(state_dir, digest)
    missing = []
    ancestor = target.parent
    while not ancestor.exists():
        missing.append(ancestor)
        ancestor = ancestor.parent
    target.parent.mkdir(parents=True, exist_ok=True)
    if missing:
        for directory in reversed(missing):
            _fsync_directory(directory)
            _fsync_directory(directory.parent)
    if not target.exists():
        fd, temporary = tempfile.mkstemp(prefix=".new-", dir=target.parent)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(raw)
                stream.flush()
                os.fsync(stream.fileno())
            try:
                os.link(temporary, target)
            except FileExistsError:
                pass
            os.unlink(temporary)
            _fsync_directory(target.parent)
        except BaseException:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass
            raise
    return {"sha256": digest, "size": len(raw)}


def load_bytes(state_dir: str | os.PathLike, ref: dict) -> bytes:
    digest = ref.get("sha256") if isinstance(ref, dict) else None
    size = ref.get("size") if isinstance(ref, dict) else None
    if (not isinstance(digest, str) or len(digest) != 64
            or any(char not in "0123456789abcdef" for char in digest)
            or not isinstance(size, int) or size < 0):
        raise ValueError("Invalid payload blob reference")
    raw = _path(state_dir, digest).read_bytes()
    if len(raw) != size or hashlib.sha256(raw).hexdigest() != digest:
        raise ValueError("Payload blob failed its size or digest check")
    return raw


def externalize_record(state_dir, db, table: str, record: dict) -> dict:
    """Move configured oversized fields out of a record and leave a preview."""
    record = dict(record)
    fields = {"checkpoints": ("items",), "tool_requests": ("result",)}.get(table, ())
    refs = dict(record.get("_payloadBlobs", {}))
    for field in fields:
        if field not in record:
            continue
        raw = json.dumps(record[field], ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if len(raw) <= EXTERNALIZE_THRESHOLD:
            refs.pop(field, None)
            continue
        value = record[field]
        ref = store_bytes(state_dir, raw, db=db)
        if isinstance(value, list):
            ref["preview"] = _small_preview(value[:1])
        elif isinstance(value, dict):
            ref["preview"] = _small_preview(
                {key: value[key] for key in ("success", "outcome", "stage") if key in value})
            if isinstance(value.get("contentItems"), list):
                ref["preview"]["contentItems"] = _small_preview(value["contentItems"][:1])
        else:
            ref["preview"] = None
        refs[field] = ref
        record[field] = ref["preview"]
    if refs:
        record["_payloadBlobs"] = refs
    return record


def externalize_result(state_dir, db, result: dict) -> dict:
    result = dict(result)
    contents = result.get("contentItems")
    if contents is None:
        return result
    raw = json.dumps(contents, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(raw) <= EXTERNALIZE_THRESHOLD:
        return result
    ref = store_bytes(state_dir, raw, db=db)
    ref["preview"] = _small_preview(contents[:1]) if isinstance(contents, list) else None
    result["contentItems"] = ref["preview"]
    result["_payloadBlobs"] = {**result.get("_payloadBlobs", {}), "contentItems": ref}
    return result


def resolve_record(state_dir, record: dict) -> dict:
    record = dict(record)
    refs = record.pop("_payloadBlobs", {})
    for field, ref in refs.items():
        record[field] = json.loads(load_bytes(state_dir, ref))
    return record


def resolve_result(state_dir, encoded: str | dict) -> dict:
    return resolve_record(state_dir, json.loads(encoded) if isinstance(encoded, str) else encoded)


def task_preview(text: str) -> str:
    raw = text.encode("utf-8")
    if len(raw) <= TASK_PREVIEW_BYTES:
        return text
    return raw[-TASK_PREVIEW_BYTES:].decode("utf-8", errors="ignore")


def trim_task(record: dict) -> dict:
    record = dict(record)
    tail = record.get("tail")
    if isinstance(tail, str) and len(tail.encode("utf-8")) > TASK_PREVIEW_BYTES:
        record["tail"] = task_preview(tail)
        record["outputTruncated"] = True
        record["outputPreviewOnly"] = True
    return record


def collect_unreferenced(state_dir, db, *, now: float, grace_seconds: int = GC_GRACE_SECONDS) -> dict:
    """Delete only aged unreferenced files while excluding active blob writers."""
    root = _blob_root(state_dir)
    if not root.exists():
        return {"deleted": 0, "bytes": 0}
    handle = (root / ".gc.lock").open("a+b")
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
        ensure_payload_schema(db)
        referenced = {row[0] for row in db.execute("SELECT DISTINCT sha256 FROM runtime_payload_refs")}
        cutoff = now - grace_seconds
        deleted = size = 0
        for path in root.glob("[0-9a-f][0-9a-f]/[0-9a-f][0-9a-f]/[0-9a-f]*"):
            digest = path.name
            try:
                stat = path.stat()
                if digest not in referenced and stat.st_mtime < cutoff:
                    path.unlink()
                    deleted += 1
                    size += stat.st_size
            except FileNotFoundError:
                pass
        return {"deleted": deleted, "bytes": size}
    finally:
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        handle.close()


def ensure_payload_schema(db) -> None:
    """Install reference bookkeeping triggers before payload writes begin."""
    db.executescript("""
        CREATE TABLE IF NOT EXISTS runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_tool_results (id TEXT PRIMARY KEY, result TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_payload_refs (
            table_name TEXT NOT NULL, record_id TEXT NOT NULL, field TEXT NOT NULL,
            sha256 TEXT NOT NULL, PRIMARY KEY(table_name,record_id,field));
        CREATE INDEX IF NOT EXISTS runtime_payload_refs_hash ON runtime_payload_refs(sha256);
        CREATE TRIGGER IF NOT EXISTS runtime_payload_checkpoints_insert AFTER INSERT ON runtime_checkpoints BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_checkpoints_update AFTER UPDATE OF record ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_checkpoints_delete AFTER DELETE ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_requests_insert AFTER INSERT ON runtime_tool_requests BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_requests_update AFTER UPDATE OF record ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_requests_delete AFTER DELETE ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_results_insert AFTER INSERT ON runtime_tool_results BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_results_update AFTER UPDATE OF result ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
        CREATE TRIGGER IF NOT EXISTS runtime_payload_results_delete AFTER DELETE ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
        END;
    """)
