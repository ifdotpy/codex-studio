"""Slot file operations run with the line owner's Linux credentials."""
from __future__ import annotations

import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile

from host_exec_protocol import CHUNK, HostExecError, inside, relative, require, scan

EXPORT_MARKER = ".layr-export.json"


def _entry(path: Path):
    """(kind, bytes, mode) of a file or a link; None when absent; ("directory", ...) for a folder."""
    try:
        info = path.lstat()
    except FileNotFoundError:
        return None
    if stat.S_ISLNK(info.st_mode):
        return ("link", os.readlink(path).encode(), 0)
    if stat.S_ISREG(info.st_mode):
        return ("file", path.read_bytes(), stat.S_IMODE(info.st_mode) & 0o777)
    if stat.S_ISDIR(info.st_mode):
        return ("directory", b"", 0)
    raise HostExecError("unsupported_file", "The slot contains a special file: " + path.name)


def _base(env, state, rel):
    """The bytes of `rel` in the held state (a file's content, a link's target), or None."""
    proc = subprocess.run(["layr", "show", state + ":" + rel], env=env, capture_output=True, timeout=300)
    if proc.returncode != 0 or proc.stdout.startswith(b"tree " + state.encode() + b":"):
        return None
    return proc.stdout


def _put(path: Path, entry):
    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.exists():
        shutil.rmtree(path)
    if entry is None:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    kind, data, mode = entry
    if kind == "link":
        path.symlink_to(data.decode())
    else:
        path.write_bytes(data)
        path.chmod(mode or 0o644)


def _text(data):
    return data is not None and b"\0" not in data[:8000]


def collect(params):
    """Bring the paths that the Mac command changed into the line. Per path three versions: the
    held state (base), the slot (theirs) and the line now (ours). A path that only the slot
    changed is copied; one that both changed is merged as text (markers) or keeps the line's
    version with the slot's next to it as `<path>.slot-conflict`."""
    source, line = Path(params["root"]), Path(params["line"])
    held, env = params["held"], {k: str(v) for k, v in params["env"].items()}
    copied, conflicts = [], []
    for value in sorted(set(params["paths"])):
        rel = relative(value)
        if rel == EXPORT_MARKER or rel == ".git" or rel.startswith(".git/"):
            continue
        theirs = _entry(inside(source, rel, leaf=True))
        target = inside(line, rel, leaf=True)
        ours = _entry(target)
        if (theirs and theirs[0] == "directory") or (ours and ours[0] == "directory"):
            continue
        base = _base(env, held, rel)
        theirs_b, ours_b = theirs[1] if theirs else None, ours[1] if ours else None
        if theirs_b == base and (theirs is None or ours is None or theirs[2] == ours[2]):
            continue
        if ours_b == base:
            _put(target, theirs)
            copied.append(rel)
            continue
        if ours_b == theirs_b:
            continue
        if theirs and ours and theirs[0] == ours[0] == "file" and _text(theirs_b) and _text(ours_b) and (base is None or _text(base)):
            with tempfile.TemporaryDirectory() as tmp:
                files = []
                for name, data in (("ours", ours_b), ("base", base or b""), ("theirs", theirs_b)):
                    Path(tmp, name).write_bytes(data)
                    files.append(str(Path(tmp, name)))
                merged = subprocess.run(["git", "merge-file", "-p", "-L", "line", "-L", "slot base", "-L", "slot", *files],
                                        capture_output=True, timeout=300)
                require(merged.returncode >= 0, "git merge-file failed", "operation_failed")
                target.write_bytes(merged.stdout)
            if merged.returncode == 0:
                copied.append(rel)
            else:
                conflicts.append(rel + " (content: markers written)")
            continue
        if theirs is None:
            conflicts.append(rel + " (deleted in the slot, changed in the line: kept the line version)")
            continue
        side = target.with_name(target.name + ".slot-conflict")
        _put(side, theirs)
        conflicts.append(rel + " (kept the line version; the slot version is " + rel + ".slot-conflict)")
    return {"copied": copied, "conflicts": conflicts}


def run(params):
    root = Path(params["root"])
    action = params["action"]
    if action == "manifest":
        return {"entries": scan(root)}
    if action == "collect":
        return collect(params)
    if action == "clear":
        # Everything in the agent's slot folder goes (the folder itself stays).
        for child in root.iterdir():
            if child.is_symlink() or child.is_file():
                child.unlink()
            else:
                shutil.rmtree(child)
        return {"path": str(root)}
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
