"""Content transport for host_exec. Source state and merges belong to layr."""
from __future__ import annotations

import bisect
import hashlib
import json
import os
import re
from pathlib import Path
import stat
import tempfile
import time
import unicodedata
from typing import Any, Iterable

PORT = 4051
FRAME_LIMIT = 2 * 1024 * 1024
CHUNK = 512 * 1024
# A Chromium checkout with its build output has about a million entries.
MAX_ENTRIES = 2_000_000
# A manifest page stays well below the frame limit.
PAGE_ENTRIES = 4096
PAGE_BYTES = 1024 * 1024
# The hash cache does not keep a file whose times are this close to the scan: a second write
# in the same clock step would keep all key fields.
RACY_NS = 2_000_000_000


class HostExecError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code

    def object(self) -> dict[str, str]:
        return {"code": self.code, "message": str(self)}


def require(condition: Any, message: str, code: str = "invalid_params") -> None:
    if not condition:
        raise HostExecError(code, message)


def key(value: str) -> str:
    return hashlib.sha256(value.encode()).hexdigest()


def atomic(path: Path, value: Any) -> None:
    fd, temporary = tempfile.mkstemp(dir=path.parent, prefix=".host-exec-")
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        Path(temporary).unlink(missing_ok=True)


def relative(value: Any) -> str:
    require(isinstance(value, str) and value and len(value.encode()) <= 4096
            and "\0" not in value and "\n" not in value and "\r" not in value, "Invalid relative file path")
    path = Path(value)
    require(not path.is_absolute() and all(part not in {"", ".", ".."} for part in value.split("/")),
            "The file path must be relative without dot components")
    return str(value)


def inside(root: Path, value: Any, *, leaf: bool = False) -> Path:
    path = root / relative(value)
    for parent in (path, *path.parents) if leaf else path.parents:
        if parent == root:
            break
        require(not parent.is_symlink(), "The file path crosses a symbolic link")
    return path


def under(path: str, roots: Iterable[str]) -> bool:
    """Whether `path` is one of `roots` or inside one of them."""
    return any(path == root or path.startswith(root + "/") for root in roots)


def load_hashes(path: Path) -> dict[str, Any]:
    """A saved hash cache; a missing or damaged file is an empty cache."""
    try:
        value = json.loads(path.read_text())
    except (OSError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def file_hash(path: Path, info: os.stat_result, rel: str, hashes: dict[str, Any] | None,
              started: int) -> tuple[str, bool]:
    """The SHA-256 of a regular file and whether the cache `hashes` (see `scan`) holds it. The
    cache keeps only a hash of a file that did not change during the read, and whose times are
    older than RACY_NS before `started`."""
    stamp = [info.st_size, info.st_mtime_ns, info.st_ino, info.st_ctime_ns]
    cached = hashes.get(rel) if hashes is not None else None
    if isinstance(cached, list) and len(cached) == 5 and cached[:4] == stamp and isinstance(cached[4], str):
        return cached[4], True
    with path.open("rb") as stream:
        checksum = hashlib.file_digest(stream, "sha256").hexdigest()
        done = os.fstat(stream.fileno())
    if hashes is None:
        return checksum, False
    steady = [done.st_size, done.st_mtime_ns, done.st_ino, done.st_ctime_ns] == stamp
    if steady and started - max(info.st_mtime_ns, info.st_ctime_ns) > RACY_NS:
        hashes[rel] = [*stamp, checksum]
        return checksum, True
    hashes.pop(rel, None)
    return checksum, False


def scan(root: Path, skip: Iterable[str] = (), hashes: dict[str, Any] | None = None) -> dict[str, dict[str, Any]]:
    """The entries below `root` with content hashes. The relative paths in `skip` stay out
    with everything inside them.

    `hashes` is a cache: path -> [size, mtime_ns, inode, ctime_ns, sha256]. A file whose four
    stat fields are unchanged is not read again; a change in any of them reads the file. The
    scan updates the cache in place and removes the files that it no longer finds, except
    those below a skipped path."""
    skipped = set(skip)
    result: dict[str, dict[str, Any]] = {}
    started = time.time_ns()
    seen: set[str] = set()
    if not root.exists():
        if hashes is not None:
            hashes.clear()
        return result
    for directory, folders, files in os.walk(root, followlinks=False):
        if skipped:
            base = Path(directory).relative_to(root).as_posix()
            folders[:] = [name for name in folders if (name if base == "." else base + "/" + name) not in skipped]
        for name in sorted(folders + files):
            path = Path(directory) / name
            rel = relative(path.relative_to(root).as_posix())
            if rel in skipped:
                continue
            info = path.lstat()
            if stat.S_ISLNK(info.st_mode):
                result[rel] = {"kind": "link", "target": os.readlink(path)}
            elif stat.S_ISREG(info.st_mode):
                checksum, _ = file_hash(path, info, rel, hashes, started)
                seen.add(rel)
                result[rel] = {"kind": "file", "bytes": info.st_size,
                               "mode": stat.S_IMODE(info.st_mode) & 0o777, "sha256": checksum}
            elif stat.S_ISDIR(info.st_mode):
                result[rel] = {"kind": "directory"}
            else:
                raise HostExecError("unsupported_file", "The slot contains a special file: " + rel)
            require(len(result) <= MAX_ENTRIES, "The slot exceeds its file count limit", "disk_budget")
    if hashes is not None:
        for gone in [path for path in hashes if path not in seen and not under(path, skipped)]:
            del hashes[gone]
    return result


def page(items: list[tuple[str, Any]], after: Any) -> dict[str, Any]:
    """The entries of a sorted manifest after the path `after` ("" for the first page), and the
    cursor of the next page (None after the last page)."""
    require(isinstance(after, str), "Invalid manifest cursor")
    start = bisect.bisect_right(items, after, key=lambda item: item[0])
    entries: dict[str, Any] = {}
    size = 0
    for path, entry in items[start:start + PAGE_ENTRIES]:
        size += len(path.encode()) + len(json.dumps(entry)) + 8
        if entries and size > PAGE_BYTES:
            break
        entries[path] = entry
    end = start + len(entries)
    return {"entries": entries, "next": items[end - 1][0] if entries and end < len(items) else None}


def conflicts(paths: list[str]) -> list[list[str]]:
    names: dict[str, set[str]] = {}
    for path in paths:
        parts = path.split("/")
        for size in range(1, len(parts) + 1):
            name = "/".join(parts[:size])
            normalized = unicodedata.normalize("NFD", name).casefold()
            names.setdefault(normalized, set()).add(name)
    return [sorted(values) for values in names.values() if len(values) > 1]


def aliases(path: str) -> list[str]:
    values = {path, os.path.realpath(path)}
    for value in list(values):
        for short in ("/tmp", "/var"):
            long = "/private" + short
            if value == long or value.startswith(long + "/"):
                values.add(value[len("/private"):])
            if value == short or value.startswith(short + "/"):
                values.add("/private" + value)
    return sorted(values, key=len, reverse=True)


def mapped(text: str, slot: str, line: str) -> str:
    for path in aliases(slot):
        text = re.sub(re.escape(path) + r'(?=$|[/\s\"\'():,;\]])', lambda match: line, text)
    return text
