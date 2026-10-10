"""Mac folder sync in the VM (docs/vm-mac-sync.md).

The Mac sends the files it changed, each with the layr state its content was based on.
apply() folds them into the project's `mac` line as the project user with a per-file
three-way merge, merges that line into the protected main as the same user, and lists
what the Mac must take from main. A conflict never becomes markers: a conflicting path
keeps the line's content and is reported.

Two lines belong to the project user: `mac` mirrors the Mac folder, and `mac-view` is a
checkout of the main state the Mac reads (exact content, modes and link kinds).
"""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
from typing import Any

from common import GuestError, atomic_json, require
from host_exec_protocol import inside, relative
from layr_share import command, project_id

METHODS = {"sync.mac.apply", "sync.mac.read", "sync.mac.hashes"}
READ_METHODS = {"sync.mac.read", "sync.mac.hashes"}
LINE, VIEW = "mac", "mac-view"
CHUNK = 512 * 1024
MAX_ENTRIES = 20_000
STATE_ID = re.compile(r"[0-9a-f]{7,64}")
OPERATION = re.compile(r"[A-Za-z0-9_-]{1,100}")
UPLOADS = Path("/var/lib/codex-studio/projects")
SKIPPED = {".git"}


def _state(value: Any) -> str:
    require(isinstance(value, str) and STATE_ID.fullmatch(value), "Invalid layr state ID")
    return value


def _entry(path: Path) -> tuple[str, bytes, int] | None:
    """(kind, bytes, mode) of a file or link without following links; None when absent."""
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
    raise GuestError("unsupported_file", "A special file cannot sync: " + path.name)


def _text(data: bytes | None) -> bool:
    return data is not None and b"\0" not in data[:8000]


def _describe(entry: tuple[str, bytes, int] | None) -> dict[str, Any]:
    if entry is None:
        return {"kind": "absent"}
    kind, data, mode = entry
    return {"kind": kind, "mode": mode, "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}


class MacSync:
    def __init__(self, state: Path, root: Path, projects: Any):
        self.state, self.root, self.projects = state, root, projects
        self.locks: dict[str, asyncio.Lock] = {}

    async def run(self, argv: list[str], *, cwd: Path, owner: str, timeout: float = 1800) -> tuple[int, str, str]:
        environment = {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "LAYR_ROOT": str(self.root),
                       "LAYR_SOCKET": "/run/layr/layr.sock", "LAYR_SESSION": "mac-sync"}
        process = await asyncio.create_subprocess_exec(
            "/usr/sbin/runuser", "-u", owner, "--", "/usr/local/bin/layr", *argv, cwd=str(cwd), env=environment,
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            output, errors = await asyncio.wait_for(process.communicate(), timeout)
        except asyncio.TimeoutError as exc:
            process.kill()
            await process.wait()
            raise GuestError("outcome_unknown", "The layr operation exceeded its deadline") from exc
        return process.returncode or 0, output.decode(errors="replace"), errors.decode(errors="replace")

    async def layr(self, argv: list[str], *, cwd: Path, owner: str) -> str:
        code, output, errors = await self.run(argv, cwd=cwd, owner=owner)
        if code:
            raise GuestError("operation_failed", "layr " + argv[0] + " failed: " + (errors or output).strip()[-300:])
        return output.strip()

    async def show(self, state: str, path: str, *, cwd: Path, owner: str) -> bytes | None:
        """The content of `path` in `state`, or None when the state has no such file."""
        environment = {"PATH": "/usr/local/bin:/usr/bin:/bin", "LAYR_ROOT": str(self.root),
                       "LAYR_SOCKET": "/run/layr/layr.sock"}
        process = await asyncio.create_subprocess_exec(
            "/usr/sbin/runuser", "-u", owner, "--", "/usr/local/bin/layr", "show", state + ":" + path, cwd=str(cwd),
            env=environment, stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE)
        output, _ = await asyncio.wait_for(process.communicate(), 300)
        if process.returncode or output.startswith(b"tree " + state.encode() + b":"):
            return None
        return output

    async def lines(self, project: str) -> tuple[str, Path, Path, Path]:
        owner = self.projects.owner(project)["owner"]
        folder = self.projects.folder(project)
        main, mac, view = folder / "lines/main", folder / "lines" / LINE, folder / "lines" / VIEW
        if not main.is_dir():
            raise GuestError("not_found", "The layr project does not exist")
        for line, path in ((LINE, mac), (VIEW, view)):
            if not path.is_dir():
                await self.layr(["branch", line, "main", "--owner", owner], cwd=main, owner=owner)
        return owner, main, mac, view

    async def head(self, cwd: Path, owner: str) -> str:
        return _state(await self.layr(["rev-parse", "HEAD"], cwd=cwd, owner=owner))

    async def clean(self, cwd: Path, owner: str, name: str) -> None:
        if await self.layr(["status", "--porcelain"], cwd=cwd, owner=owner):
            raise GuestError("operation_failed", "The " + name + " line has uncommitted changes")

    async def checkout(self, view: Path, owner: str, state: str) -> None:
        if await self.head(view, owner) != state:
            await self.layr(["reset", "-q", "--hard", state], cwd=view, owner=owner)
        await self.clean(view, owner, VIEW)

    def write(self, line: Path, rel: str, entry: tuple[str, bytes, int] | None, uid: int, gid: int) -> None:
        target = inside(line, rel, leaf=True)
        if target.is_symlink() or target.is_file():
            target.unlink()
        elif target.exists():
            shutil.rmtree(target)
        if entry is None:
            return
        parent = target.parent
        missing = []
        while not parent.exists():
            missing.append(parent)
            parent = parent.parent
        for folder in reversed(missing):
            folder.mkdir(mode=0o755)
            os.chown(folder, uid, gid)
        kind, data, mode = entry
        if kind == "link":
            os.symlink(data.decode(), target)
            os.lchown(target, uid, gid)
            return
        descriptor = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        with os.fdopen(descriptor, "wb") as stream:
            stream.write(data)
        os.chown(target, uid, gid)
        target.chmod(mode or 0o644)

    async def fold(self, project: str, owner: str, mac: Path, upload: Path,
                   entries: list[dict[str, Any]]) -> tuple[list[str], list[str]]:
        """Fold the Mac's files into the `mac` line; returns (applied, conflicts)."""
        user = self.projects.owner(project)
        applied, conflicts = [], []
        for item in entries:
            rel = relative(item.get("path"))
            require(rel.split("/")[0] not in SKIPPED and item.get("kind") in {"file", "link", "absent"},
                    "Invalid sync entry")
            base_state = item.get("base")
            base = None if base_state is None else await self.show(_state(base_state), rel, cwd=mac, owner=owner)
            theirs = None if item["kind"] == "absent" else _entry(inside(upload, rel, leaf=True))
            require(item["kind"] == "absent" or (theirs is not None and theirs[0] == item["kind"]),
                    "The upload does not hold " + rel)
            ours = _entry(inside(mac, rel, leaf=True))
            require(ours is None or ours[0] != "directory", "A folder in the line has the path " + rel)
            theirs_bytes, ours_bytes = (theirs[1] if theirs else None), (ours[1] if ours else None)
            if theirs_bytes == base and (theirs is None or ours is None or theirs[0] == ours[0]):
                continue
            if ours_bytes == base:
                self.write(mac, rel, theirs, user["uid"], user["gid"])
                applied.append(rel)
                continue
            if ours == theirs or (ours and theirs and ours[:2] == theirs[:2]):
                applied.append(rel)
                continue
            if (theirs and ours and theirs[0] == ours[0] == "file" and _text(theirs_bytes)
                    and _text(ours_bytes) and (base is None or _text(base))):
                with tempfile.TemporaryDirectory() as scratch:
                    files = []
                    for name, data in (("line", ours_bytes), ("base", base or b""), ("mac", theirs_bytes)):
                        Path(scratch, name).write_bytes(data or b"")
                        files.append(str(Path(scratch, name)))
                    merged = subprocess.run(["git", "merge-file", "-p", "--quiet", *files],
                                            capture_output=True, timeout=300)
                if merged.returncode == 0:
                    self.write(mac, rel, ("file", merged.stdout, theirs[2]), user["uid"], user["gid"])
                    applied.append(rel)
                    continue
            conflicts.append(rel)
        return applied, conflicts

    async def merge_conflicts(self, main: Path, owner: str, operation: str) -> list[str]:
        """Paths that conflict between mac and main, from a merge into a scratch line."""
        scratch = "mac-sync-probe-" + hashlib.sha256(operation.encode()).hexdigest()[:12]
        await self.layr(["branch", scratch, "main", "--owner", owner], cwd=main, owner=owner)
        try:
            folder = main.parent / scratch
            _code, output, errors = await self.run(["merge", LINE], cwd=folder, owner=owner)
            return sorted(set(re.findall(r"Merge conflict in (.+)", output + errors)))
        finally:
            await self.run(["branch", "-D", scratch], cwd=main, owner=owner)

    async def outbound(self, view: Path, owner: str, since: list[str], state: str) -> dict[str, list[dict[str, Any]]]:
        result: dict[str, list[dict[str, Any]]] = {}
        for base in since:
            changes = []
            output = await self.layr(["diff", "--name-status", "--no-renames", _state(base), state], cwd=view, owner=owner)
            for line in output.splitlines():
                status, _, rel = line.partition("\t")
                if not rel or rel.split("/")[0] in SKIPPED:
                    continue
                changes.append({"path": relative(rel), **_describe(_entry(inside(view, rel, leaf=True)))})
                require(len(changes) <= MAX_ENTRIES, "Too many changed files for one sync round")
            result[base] = changes
        return result

    async def apply(self, request_id: str, params: dict[str, Any]) -> dict[str, Any]:
        project = project_id(params.get("projectId"))
        operation = params.get("operationId")
        require(isinstance(operation, str) and OPERATION.fullmatch(operation), "Invalid sync operation ID")
        entries = params.get("entries")
        require(isinstance(entries, list) and len(entries) <= MAX_ENTRIES, "Invalid sync entries")
        since = params.get("since")
        require(isinstance(since, list) and len(since) <= 16, "Invalid sync bases")
        receipts = self.state / "mac-sync" / project
        receipt = receipts / (operation + ".json")
        if receipt.exists():
            return json.loads(receipt.read_text())
        async with self.locks.setdefault(project, asyncio.Lock()):
            owner, main, mac, view = await self.lines(project)
            await self.clean(mac, owner, LINE)
            main_head, mac_head = await self.head(main, owner), await self.head(mac, owner)
            # The line follows main between rounds; unmerged Mac work stays where it is.
            code, _, _ = await self.run(["merge-base", "--is-ancestor", mac_head, main_head], cwd=mac, owner=owner)
            if code == 0 and mac_head != main_head:
                await self.layr(["reset", "-q", "--hard", main_head], cwd=mac, owner=owner)
            applied, conflicts = [], []
            if entries:
                source = params.get("upload")
                require(isinstance(source, str) and Path(source).parent == UPLOADS
                        and Path(source).resolve() == Path(source) and Path(source).is_dir(),
                        "The sync upload must be a completed upload root")
                identity = "mac-sync:" + project + ":" + operation
                # A retry after a lost reply finds its own private snapshot of the upload.
                stale = self.state / "imports" / hashlib.sha256(identity.encode()).hexdigest()
                if stale.exists():
                    await command(["btrfs", "subvolume", "delete", stale])
                frozen = await self.projects.freeze_source(Path(source), identity)
                try:
                    applied, conflicts = await self.fold(project, owner, mac, frozen, entries)
                finally:
                    await command(["btrfs", "subvolume", "delete", frozen])
                await self.layr(["add", "-A"], cwd=mac, owner=owner)
                # Files Git ignores, such as .env, sync too: add them by name.
                present = [rel for rel in applied if os.path.lexists(mac / rel)]
                for start in range(0, len(present), 500):
                    await self.layr(["add", "-f", "--", *present[start:start + 500]], cwd=mac, owner=owner)
                if await self.layr(["status", "--porcelain"], cwd=mac, owner=owner):
                    await self.layr(["commit", "-q", "-m", "Mac sync"], cwd=mac, owner=owner)
            mac_state = await self.head(mac, owner)
            merge, merged, detail = "none", [], None
            if mac_state != main_head:
                code, output, errors = await self.run(["merge", "-q", LINE, "--expect", mac_state], cwd=main, owner=owner)
                if code:
                    # A conflict waits for a resolution in the VM. Without one, main was
                    # busy (for example uncommitted lead work); the next round tries again.
                    merged = await self.merge_conflicts(main, owner, operation)
                    merge, detail = ("conflict" if merged else "refused"), (errors or output).strip()[-300:]
                else:
                    merge = "merged"
            main_state = await self.head(main, owner)
            if merge in {"none", "merged"} and await self.head(mac, owner) != main_state:
                await self.layr(["reset", "-q", "--hard", main_state], cwd=mac, owner=owner)
            await self.checkout(view, owner, main_state)
            result = {"projectId": project, "mainStateId": main_state, "macStateId": await self.head(mac, owner),
                      "applied": applied, "conflicts": sorted(set(conflicts)), "merge": merge,
                      "mergeConflicts": merged, "mergeError": detail,
                      "outbound": await self.outbound(view, owner, [_state(base) for base in since], main_state)}
            receipts.mkdir(parents=True, mode=0o700, exist_ok=True)
            atomic_json(receipt, result)
            # The receipt answers a retry, so the upload is no longer needed.
            if entries:
                shutil.rmtree(params["upload"], ignore_errors=True)
            return result

    async def read(self, request_id: str, params: dict[str, Any]) -> dict[str, Any]:
        project = project_id(params.get("projectId"))
        state = _state(params.get("stateId"))
        offset = params.get("offset", 0)
        require(type(offset) is int and offset >= 0, "Invalid read offset")
        owner, _main, _mac, view = await self.lines(project)
        async with self.locks.setdefault(project, asyncio.Lock()):
            await self.checkout(view, owner, state)
            entry = _entry(inside(view, params.get("path"), leaf=True))
        if entry is None or entry[0] == "directory":
            return {"kind": "absent"}
        kind, data, mode = entry
        chunk = data[offset:offset + CHUNK]
        return {"kind": kind, "mode": mode, "size": len(data), "offset": offset,
                "data": base64.b64encode(chunk).decode(), "eof": offset + len(chunk) >= len(data)}

    async def hashes(self, request_id: str, params: dict[str, Any]) -> dict[str, Any]:
        """Every synced path of a state, for the first round of an existing project."""
        project = project_id(params.get("projectId"))
        state = _state(params.get("stateId"))
        after = params.get("after", "")
        require(isinstance(after, str), "Invalid page start")
        owner, _main, _mac, view = await self.lines(project)
        async with self.locks.setdefault(project, asyncio.Lock()):
            await self.checkout(view, owner, state)
            found = []
            for directory, folders, files in os.walk(view, followlinks=False):
                folders[:] = [name for name in folders if name not in SKIPPED]
                for name in files + [name for name in folders if (Path(directory) / name).is_symlink()]:
                    rel = (Path(directory) / name).relative_to(view).as_posix()
                    if rel > after:
                        found.append(rel)
            found.sort()
            page = found[:5000]
            items = {rel: _describe(_entry(view / rel)) for rel in page}
        return {"stateId": state, "items": items, "next": page[-1] if len(found) > len(page) else None}

    async def dispatch(self, request_id: str, method: str, params: dict[str, Any], emit: Any) -> dict[str, Any]:
        if method == "sync.mac.apply":
            return await self.apply(request_id, params)
        if method == "sync.mac.read":
            return await self.read(request_id, params)
        return await self.hashes(request_id, params)
