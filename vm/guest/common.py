"""Private files and limits shared by the guest and its process supervisors."""
from __future__ import annotations

import asyncio
import base64
from contextlib import asynccontextmanager
import hashlib
import json
import math
import os
from pathlib import Path
import sqlite3
import tempfile

PROTOCOL = 1
PORT = 4050
MAX_LINE = 2 * 1024 * 1024
MAX_INPUT = 1024 * 1024
MAX_OUTPUT = 64 * 1024 * 1024


class GuestError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code

    def object(self):
        return {"code": self.code, "message": str(self)}


def require(condition, message):
    if not condition:
        raise GuestError("invalid_params", message)


def identifier(value):
    require(isinstance(value, str) and 0 < len(value) <= 128, "The identifier must be a string of 1 to 128 characters")
    return value


def number(value, default, minimum, maximum):
    value = default if value is None else value
    require(isinstance(value, (int, float)) and not isinstance(value, bool)
            and math.isfinite(value) and minimum <= value <= maximum,
            f"The number must be between {minimum} and {maximum}")
    return value


def integer(value, default, minimum, maximum):
    result = number(value, default, minimum, maximum)
    require(isinstance(result, int), "The value must be an integer")
    return result


def decode(value, limit=MAX_INPUT):
    require(isinstance(value, str) and len(value) <= ((limit + 2) // 3) * 4,
            "The base64 data exceeds the limit")
    try:
        result = base64.b64decode(value, validate=True)
    except (ValueError, UnicodeError) as exc:
        raise GuestError("invalid_params", "The data is not valid base64") from exc
    require(len(result) <= limit, "The decoded data exceeds the limit")
    return result


def private_dir(path: Path):
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(path, 0o700)


def atomic_bytes(path: Path, data: bytes):
    fd, temporary = tempfile.mkstemp(prefix=".guest-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        Path(temporary).unlink(missing_ok=True)


def atomic_json(path: Path, value):
    atomic_bytes(path, json.dumps(value, separators=(",", ":")).encode())


def connect_db(path: Path):
    database = sqlite3.connect(path, timeout=2)
    os.chmod(path, 0o600)
    database.execute("PRAGMA journal_mode=WAL")
    database.execute("PRAGMA synchronous=FULL")
    database.execute("CREATE TABLE IF NOT EXISTS receipts (id TEXT PRIMARY KEY, digest TEXT NOT NULL, response TEXT)")
    database.commit()
    return database


def digest(method, params):
    return hashlib.sha256(json.dumps([method, params], sort_keys=True, separators=(",", ":"),
                                    allow_nan=False).encode()).hexdigest()


def receipt(database, request_id, request_digest):
    row = database.execute("SELECT digest, response FROM receipts WHERE id=?", (request_id,)).fetchone()
    if row is not None:
        if row[0] != request_digest:
            raise GuestError("id_conflict", "The request ID has different content")
        if row[1] is None:
            raise GuestError("outcome_unknown", "The request has no proven result; inspect the saved state")
        return json.loads(row[1])
    database.execute("INSERT INTO receipts VALUES (?, ?, NULL)", (request_id, request_digest))
    database.commit()
    return None


def save_receipt(database, request_id, response):
    database.execute("UPDATE receipts SET response=? WHERE id=?", (json.dumps(response), request_id))
    database.commit()


def process_identity(pid):
    if os.uname().sysname == "Linux":
        try:
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
            return None if fields[0] in {"Z", "X"} else fields[19]
        except (OSError, IndexError):
            return None
    try:
        os.kill(pid, 0)
        return "test-host"
    except (ProcessLookupError, PermissionError):
        return None


@asynccontextmanager
async def bounded_lock(locks, key):
    entry = locks.setdefault(key, [asyncio.Lock(), 0])
    entry[1] += 1
    acquired = False
    try:
        try:
            await asyncio.wait_for(entry[0].acquire(), 30)
            acquired = True
        except asyncio.TimeoutError as exc:
            raise GuestError("busy", "The resource lock exceeded its deadline") from exc
        yield
    finally:
        if acquired:
            entry[0].release()
        entry[1] -= 1
        if entry[1] == 0:
            locks.pop(key, None)
