"""Slot file operations run with the line owner's Linux credentials."""
from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path
import shutil
import sys

from host_exec_protocol import CHUNK, HostExecError, inside, require, scan


def run(params):
    root = Path(params["root"])
    action = params["action"]
    if action == "manifest":
        return {"entries": scan(root)}
    path = inside(root, params["path"], leaf=action == "read")
    if action == "read":
        offset = params.get("offset", 0)
        require(isinstance(offset, int) and offset >= 0, "Invalid source offset")
        with path.open("rb") as stream:
            stream.seek(offset)
            data = stream.read(CHUNK)
        return {"data": base64.b64encode(data).decode(), "bytes": len(data)}
    if action in {"delete", "entry"}:
        if path.is_symlink() or path.is_file():
            path.unlink()
        elif path.exists():
            shutil.rmtree(path)
        if action == "entry":
            entry = params["entry"]
            path.parent.mkdir(parents=True, exist_ok=True)
            if entry["kind"] == "directory":
                path.mkdir()
            elif entry["kind"] == "link":
                require(isinstance(entry["target"], str) and "\0" not in entry["target"], "Invalid source link")
                path.symlink_to(entry["target"])
            else:
                require(entry["kind"] == "file", "Invalid source entry")
                path.touch()
    elif action == "chunk":
        require(not path.is_symlink() and path.is_file(), "The source file is not open")
        data = base64.b64decode(params["data"], validate=True)
        require(len(data) <= CHUNK and path.stat().st_size == params["offset"]
                and path.stat().st_size + len(data) <= params["entry"]["bytes"], "Invalid source chunk")
        with path.open("ab") as stream:
            stream.write(data)
            stream.flush()
            import os
            os.fsync(stream.fileno())
    elif action == "commit":
        entry = params["entry"]
        require(not path.is_symlink(), "The source file is a symbolic link")
        with path.open("rb") as stream:
            checksum = hashlib.file_digest(stream, "sha256").hexdigest()
        require(checksum == entry["sha256"] and path.stat().st_size == entry["bytes"], "The source checksum differs")
        path.chmod(entry["mode"] & 0o777)
    else:
        raise HostExecError("invalid_params", "Invalid slot file action")
    return {"path": params["path"], "absolutePath": str(path)}


if __name__ == "__main__":
    try:
        value = run(json.load(sys.stdin))
        print(json.dumps({"result": value}))
    except Exception as exc:
        print(json.dumps({"error": {"code": getattr(exc, "code", "slot_io_failed"), "message": str(exc)}}))
        sys.exit(1)
