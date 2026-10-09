"""Content transport for host_exec. Source state and merges belong to layr."""
from __future__ import annotations

import hashlib
import json
import os
import re
from pathlib import Path
import stat
import tempfile
import unicodedata
from typing import Any

PORT = 4051
FRAME_LIMIT = 2 * 1024 * 1024
CHUNK = 512 * 1024
MAX_ENTRIES = 200_000


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


def scan(root: Path) -> dict[str, dict[str, Any]]:
    result: dict[str, dict[str, Any]] = {}
    if not root.exists():
        return result
    for directory, folders, files in os.walk(root, followlinks=False):
        for name in sorted(folders + files):
            path = Path(directory) / name
            rel = relative(path.relative_to(root).as_posix())
            info = path.lstat()
            if stat.S_ISLNK(info.st_mode):
                result[rel] = {"kind": "link", "target": os.readlink(path)}
            elif stat.S_ISREG(info.st_mode):
                with path.open("rb") as stream:
                    checksum = hashlib.file_digest(stream, "sha256").hexdigest()
                result[rel] = {"kind": "file", "bytes": info.st_size,
                               "mode": stat.S_IMODE(info.st_mode), "sha256": checksum}
            elif stat.S_ISDIR(info.st_mode):
                result[rel] = {"kind": "directory"}
            else:
                raise HostExecError("unsupported_file", "The slot contains a special file: " + rel)
            require(len(result) <= MAX_ENTRIES, "The slot exceeds its file count limit", "disk_budget")
    return result


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
