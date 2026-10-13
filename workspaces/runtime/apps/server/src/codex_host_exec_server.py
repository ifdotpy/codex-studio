"""User-session Mac command owner for the guest host_exec channel."""
from __future__ import annotations

import argparse
import asyncio
import base64
import codecs
import ctypes
import fcntl
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import shutil
import signal
import sqlite3
import stat
import sys
import time
from typing import Any, TYPE_CHECKING

from codex_layout import VM_GUEST_ROOT
from codex_host_exec import service_build_id

# The host side shares the frame protocol with the guest (mypy finds it through mypy_path).
sys.path.insert(0, str(VM_GUEST_ROOT))
from host_exec_protocol import (CHUNK, FRAME_LIMIT, HostExecError, aliases, atomic,  # noqa: E402
                                conflicts, file_hash, inside, key, load_hashes, mapped, page, relative, require,
                                scan, under)

# The limits of a project without a settings file.
DEFAULT_SLOTS = 4
DEFAULT_BUDGET = 20 * 1024**3
SETTINGS = "host-exec.json"
# The folder names of layr-projects/ (codex_linux_vm_share._id).
PROJECT_FOLDER = re.compile(r"[A-Za-z0-9][A-Za-z0-9_-]{0,99}")
# What this service accepts beyond the first protocol; the guest reads it from the lease.
FEATURES = ["prime", "batch", "mode", "clone"]
CLONE_NOFOLLOW = 0x0001


def clone_file(source: Path, copy: Path) -> None:
    """Clone one regular file with APFS clonefile(2). A failure raises OSError; it never copies bytes."""
    try:
        clonefile = ctypes.CDLL(None, use_errno=True).clonefile
    except AttributeError as exc:
        raise OSError(45, "clonefile is not available") from exc
    clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint32]
    clonefile.restype = ctypes.c_int
    if clonefile(os.fsencode(source), os.fsencode(copy), CLONE_NOFOLLOW) != 0:
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), str(source))


def clone_tree(origin: Path, target: Path) -> int:
    """Clone a folder file by file with APFS clonefile(2), never with a byte copy: a failed clone
    stops with an error. Links stay links; sockets and pipes stay out; a file that disappears
    during the walk stays out. Returns the bytes as `HostService.size` counts them (st_blocks),
    although the clones share their blocks with the origin."""
    total = 0
    target.mkdir(mode=0o700)
    for directory, folders, files in os.walk(origin, followlinks=False):
        destination = target / Path(directory).relative_to(origin)
        for name in folders + files:
            source, copy = Path(directory) / name, destination / name
            try:
                info = source.lstat()
                if stat.S_ISLNK(info.st_mode):
                    copy.symlink_to(os.readlink(source))
                elif stat.S_ISDIR(info.st_mode):
                    copy.mkdir()
                elif stat.S_ISREG(info.st_mode):
                    clone_file(source, copy)
                    total += info.st_blocks * 512
            except FileNotFoundError:
                continue
    return total


class HostService:
    def __init__(self, state: Path, *, slots: int = DEFAULT_SLOTS, budget: int = DEFAULT_BUDGET):
        self.state = state.resolve()
        self.state.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.pool = self.state / "host-slots"
        self.pool.mkdir(exist_ok=True, mode=0o700)
        self.slots, self.budget = slots, budget
        self.token = (self.state / "host-exec-token").read_text()
        self.lease = (self.state / "host-exec.lock").open("a+")
        fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.db = sqlite3.connect(self.state / "host-exec.sqlite3")
        self.db.execute("PRAGMA journal_mode=WAL")
        self.db.execute("PRAGMA synchronous=FULL")
        self.db.execute("CREATE TABLE IF NOT EXISTS receipts (id TEXT PRIMARY KEY, digest TEXT, response TEXT)")
        self.db.execute("CREATE TABLE IF NOT EXISTS events (operation TEXT, seq INTEGER, value TEXT, PRIMARY KEY(operation,seq))")
        self.db.commit()
        self.active: dict[str, asyncio.Task[dict[str, Any]]] = {}
        self.commands: dict[str, asyncio.Task[None]] = {}
        self.children: dict[str, asyncio.subprocess.Process] = {}
        self.locks: dict[str, asyncio.Lock] = {}
        # Per project folder: the bytes of the last walk plus the files written since then.
        self.usage: dict[str, int] = {}
        # Sorted manifests during paging, by operation and area.
        self.pages: dict[str, list[tuple[str, Any]]] = {}
        # Hashes of files cloned into a slot, for its next scan, by slot folder.
        self.cloned: dict[str, dict[str, Any]] = {}
        self.clients = 0
        # The code build at start. Studio replaces an idle service of another build (codex_host_exec.py).
        self.build_id = service_build_id()
        self.reading = 0
        self.retiring = False
        self.stopped = asyncio.Event()

    def close(self) -> None:
        self.db.close()
        self.lease.close()

    def opdir(self, identity: str) -> Path:
        return self.state / "host-operations" / key(identity)

    def load(self, identity: str) -> dict[str, Any]:
        path = self.opdir(identity) / "status.json"
        require(path.is_file(), "The host command does not exist", "not_found")
        value: dict[str, Any] = json.loads(path.read_text())
        if value["state"] == "running" and identity not in self.commands:
            value.update(state="unknown", reason="host_owner_lost")
        return value

    def save(self, identity: str, value: dict[str, Any]) -> None:
        atomic(self.opdir(identity) / "status.json", value)

    def event(self, identity: str, value: dict[str, Any]) -> None:
        seq = self.db.execute("SELECT coalesce(max(seq),0)+1 FROM events WHERE operation=?", (identity,)).fetchone()[0]
        self.db.execute("INSERT INTO events VALUES (?,?,?)", (identity, seq, json.dumps({"seq": seq, **value})))
        self.db.commit()

    def settings(self, project_id: str) -> dict[str, Any]:
        """The slot count, disk budget and Mac-only paths of a project. The operator sets them in
        layr-projects/<project>/host-exec.json of this state folder; the VM cannot write it."""
        path = self.state / "layr-projects" / project_id / SETTINGS
        value: Any = {}
        if PROJECT_FOLDER.fullmatch(project_id):
            try:
                value = json.loads(path.read_text())
            except FileNotFoundError:
                value = {}
            except (OSError, ValueError) as exc:
                raise HostExecError("invalid_config", f"{path} is not readable JSON: {exc}") from exc
        require(isinstance(value, dict) and set(value) <= {"slots", "diskBudgetGiB", "macOnly"},
                f"{path} accepts only slots, diskBudgetGiB and macOnly", "invalid_config")
        slots = value.get("slots", self.slots)
        require(type(slots) is int and 1 <= slots <= 64, f"{path}: slots must be an integer from 1 to 64", "invalid_config")
        budget = self.budget
        if "diskBudgetGiB" in value:
            size = value["diskBudgetGiB"]
            require(type(size) in (int, float) and 0 < size <= 1024**2,
                    f"{path}: diskBudgetGiB must be a number above 0", "invalid_config")
            budget = int(size * 1024**3)
        mac_only = value.get("macOnly", [])
        require(isinstance(mac_only, list) and len(mac_only) <= 256, f"{path}: macOnly must be a list of paths", "invalid_config")
        for item in mac_only:
            try:
                relative(item)
            except HostExecError as exc:
                raise HostExecError("invalid_config", f"{path}: macOnly {item!r}: {exc}") from exc
        return {"slots": slots, "diskBudget": budget, "macOnly": list(mac_only)}

    def budget_of(self, op: dict[str, Any]) -> int:
        return int(op.get("diskBudget", self.budget))

    @staticmethod
    def used_at(slot: Path) -> float:
        return float(json.loads((slot / "slot.json").read_text()).get("usedAt", 0))

    @staticmethod
    def size(path: Path) -> int:
        total = 0
        for directory, _, files in os.walk(path, followlinks=False):
            for name in files:
                try:
                    total += (Path(directory) / name).lstat().st_blocks * 512
                except FileNotFoundError:
                    pass
        return total

    def evict(self, project: Path, budget: int, *, reserve: int = 0, exclude: Path | None = None) -> int:
        """Remove idle slots, least recently used first, until `reserve` more bytes fit in the
        budget. Returns the bytes in use."""
        require(0 <= reserve <= budget, "The sources exceed the project disk budget", "disk_budget")
        candidates = [path for path in project.iterdir() if path.is_dir() and path != exclude and not (path / "lease.json").exists()]
        candidates.sort(key=self.used_at)
        used = self.size(project)
        while used + reserve > budget and candidates:
            victim = candidates.pop(0)
            used -= self.size(victim)
            shutil.rmtree(victim)
        self.usage[str(project)] = used
        require(used + reserve <= budget, "The project disk budget has no free slot capacity", "disk_budget")
        return used

    def reserve(self, project: Path, budget: int, length: int, exclude: Path) -> None:
        """Room for a new file. The estimate grows with each file and a full walk of the project
        folder happens only when the estimate reaches the budget."""
        used = self.usage.get(str(project))
        if used is None or used + length > budget:
            used = self.evict(project, budget, reserve=length, exclude=exclude)
        self.usage[str(project)] = used + length

    def listing(self, name: str, path: Path, after: Any, entries: dict[str, Any] | None = None) -> dict[str, Any]:
        """A page of a saved manifest. The sorted entries stay in memory until the last page."""
        items = self.pages.get(name)
        if entries is not None or items is None:
            items = sorted((json.loads(path.read_text()) if entries is None else entries).items())
            self.pages[name] = items
        result = page(items, after)
        if result["next"] is None:
            self.pages.pop(name, None)
        return result

    def scan_slot(self, slot: Path, skip: list[str]) -> dict[str, dict[str, Any]]:
        """The entries of the slot's source folder. The slot's hash cache (removed with the slot)
        keeps the hash of each unchanged file, and of each file cloned since the last scan."""
        cache = slot / "hash-cache.json"
        hashes = load_hashes(cache)
        hashes.update(self.cloned.pop(str(slot), {}))
        entries = scan(slot / "source", skip, hashes)
        atomic(cache, hashes)
        return entries

    @staticmethod
    def cache_list(value: Any) -> list[str]:
        require(isinstance(value, list) and len(value) <= 256, "Invalid cache folders")
        return [relative(item) for item in value]

    def project_folder(self, project_id: str) -> Path | None:
        """The project's folder on this Mac (layr-projects/<project>/project.json "source")."""
        if not PROJECT_FOLDER.fullmatch(project_id):
            return None
        try:
            value = json.loads((self.state / "layr-projects" / project_id / "project.json").read_text())
        except (OSError, ValueError):
            return None
        source = value.get("source") if isinstance(value, dict) else None
        if not isinstance(source, str) or not source.startswith("/"):
            return None
        path = Path(source)
        return path if path.is_dir() and not path.is_symlink() else None

    @staticmethod
    def bare(target: Path, cache: str, mac_only: list[str]) -> bool:
        """Whether the slot's cache folder holds only Mac-only paths and the folders above them."""
        for directory, folders, files in os.walk(target, followlinks=False):
            here = Path(directory).relative_to(target).as_posix()
            prefix = cache if here == "." else cache + "/" + here
            keep = []
            for name in folders + files:
                rel = prefix + "/" + name
                if under(rel, mac_only):
                    continue
                if name in folders and not (Path(directory) / name).is_symlink() and any(item.startswith(rel + "/") for item in mac_only):
                    keep.append(name)
                    continue
                return False
            folders[:] = keep
        return True

    async def prime(self, op: dict[str, Any], slot: Path, cache: str) -> dict[str, Any]:
        """Clone a cache folder that the slot lacks from the project's Mac folder, so the transfer
        sends only the files that differ. Each reason not to clone leaves the normal transfer."""
        mac_only: list[str] = op.get("macOnly", [])
        target = inside(slot / "source", cache)

        def skipped(reason: str) -> dict[str, Any]:
            return {"path": cache, "primed": False, "reason": reason}

        if under(cache, mac_only):
            return skipped("Mac-only path")
        if target.is_symlink() or (target.exists() and not self.bare(target, cache, mac_only)):
            return skipped("the slot has the folder")
        folder = self.project_folder(op["projectId"])
        if folder is None:
            return skipped("no project folder on this Mac")
        try:
            origin = inside(folder, cache, leaf=True)
        except HostExecError:
            return skipped("the Mac path crosses a symbolic link")
        if not origin.is_dir():
            return skipped("the project folder on this Mac has no such folder")
        temporary = slot / "prime"
        await asyncio.to_thread(shutil.rmtree, temporary, True)
        try:
            cloned = await asyncio.to_thread(clone_tree, origin, temporary)
        except OSError as exc:
            await asyncio.to_thread(shutil.rmtree, temporary, True)
            return skipped("clone failed: " + str(exc)[:300])
        try:
            # A clone counts at full size, as the walk of `size` counts it.
            self.reserve(slot.parent, self.budget_of(op), cloned, slot)
        except HostExecError:
            await asyncio.to_thread(shutil.rmtree, temporary, True)
            raise
        if target.exists():
            # The slot keeps its own Mac-only output in place of the copy from the project folder.
            for item in mac_only:
                if not item.startswith(cache + "/"):
                    continue
                own = target / item[len(cache) + 1:]
                if not (own.exists() or own.is_symlink()):
                    continue
                try:
                    copy = inside(temporary, item[len(cache) + 1:])
                except HostExecError:
                    continue
                if copy.is_symlink() or copy.is_file():
                    copy.unlink()
                elif copy.exists():
                    await asyncio.to_thread(shutil.rmtree, copy)
                copy.parent.mkdir(parents=True, exist_ok=True)
                os.rename(own, copy)
            shutil.rmtree(target)
        target.parent.mkdir(parents=True, exist_ok=True)
        os.rename(temporary, target)
        return {"path": cache, "primed": True, "bytes": cloned}

    def clone_source(self, op: dict[str, Any], slot: Path, name: Any, entry: Any) -> dict[str, Any]:
        """Clone one file from the same path in the project folder on this Mac when that file has
        the size and SHA-256 of `entry`, so the guest does not send its content. Each reason not
        to clone leaves the transfer. The project's hash cache keeps the hashes of the files it
        checked; a clone counts at its st_blocks size, as the walk of the slot counts it."""
        require(isinstance(entry, dict) and entry.get("kind") == "file" and isinstance(entry.get("bytes"), int)
                and isinstance(entry.get("sha256"), str), "Invalid source entry")
        mode = entry.get("mode")
        require(isinstance(mode, int) and 0 <= mode <= 0o777, "Invalid source file mode")
        require(not under(name, op.get("macOnly", [])), "The path stays on the Mac (macOnly)")
        path = inside(slot / "source", name)

        def skipped(reason: str) -> dict[str, Any]:
            return {"cloned": False, "reason": reason}

        folder = self.project_folder(op["projectId"])
        if folder is None:
            return skipped("no project folder on this Mac")
        try:
            origin = inside(folder, name, leaf=True)
            info = origin.lstat()
        except HostExecError:
            return skipped("the Mac path crosses a symbolic link")
        except OSError:
            return skipped("the project folder has no such file")
        if not stat.S_ISREG(info.st_mode) or info.st_size != entry["bytes"]:
            return skipped("the project file differs")
        cache = self.state / "layr-projects" / op["projectId"] / "host-exec-hashes.json"
        hashes = load_hashes(cache)
        if len(hashes) > 100_000:
            hashes = {}
        try:
            checksum, known = file_hash(origin, info, name, hashes, time.time_ns())
        except OSError:
            return skipped("the project file cannot be read")
        atomic(cache, hashes)
        if checksum != entry["sha256"]:
            return skipped("the project file differs")
        self.reserve(slot.parent, self.budget_of(op), info.st_blocks * 512, slot)
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_symlink() or path.is_file():
            path.unlink()
        elif path.exists():
            shutil.rmtree(path)
        try:
            clone_file(origin, path)
        except OSError as exc:
            return skipped("clone failed: " + str(exc)[:300])
        path.chmod(mode)
        after = origin.lstat()
        if known and [after.st_size, after.st_mtime_ns, after.st_ino, after.st_ctime_ns] == \
                [info.st_size, info.st_mtime_ns, info.st_ino, info.st_ctime_ns]:
            # The origin did not change since its known hash: the run check need not read the clone.
            mine = path.lstat()
            self.cloned.setdefault(str(slot), {})[name] = [mine.st_size, mine.st_mtime_ns, mine.st_ino,
                                                           mine.st_ctime_ns, checksum]
        return {"cloned": True, "bytes": entry["bytes"]}

    @staticmethod
    def set_modes(slot: Path, mac_only: list[str], items: list[Any]) -> None:
        """Change only the mode of slot files whose content the guest's entry already has. The slot's
        hash cache keeps each known hash with the new change time, so the run check does not read
        the file again; a chmod changes no content, and a later write would change the mtime."""
        cache = slot / "hash-cache.json"
        hashes = load_hashes(cache)
        for item in items:
            require(isinstance(item, dict) and isinstance(item.get("entry"), dict), "Invalid mode change")
            name, entry = item.get("path"), item["entry"]
            mode = entry.get("mode")
            require(entry.get("kind") == "file" and isinstance(mode, int) and 0 <= mode <= 0o777, "Invalid mode change")
            require(not under(name, mac_only), "The path stays on the Mac (macOnly)")
            path = inside(slot / "source", name, leaf=True)
            try:
                info = path.lstat()
            except FileNotFoundError as exc:
                raise HostExecError("sync_conflict", "The slot file differs: " + name) from exc
            require(stat.S_ISREG(info.st_mode) and info.st_size == entry.get("bytes"), "The slot file differs", "sync_conflict")
            stamp = [info.st_size, info.st_mtime_ns, info.st_ino, info.st_ctime_ns]
            cached: Any = hashes.get(name)
            known = isinstance(cached, list) and len(cached) == 5 and cached[:4] == stamp
            if known:
                checksum = cached[4]
            else:
                with path.open("rb") as stream:
                    checksum = hashlib.file_digest(stream, "sha256").hexdigest()
            require(checksum == entry.get("sha256"), "The slot file differs", "sync_conflict")
            path.chmod(mode)
            after = path.lstat()
            if known and [after.st_size, after.st_mtime_ns, after.st_ino] == stamp[:3]:
                hashes[name] = [after.st_size, after.st_mtime_ns, after.st_ino, after.st_ctime_ns, checksum]
            else:
                hashes.pop(name, None)
        atomic(cache, hashes)

    def place(self, op: dict[str, Any], slot: Path, name: Any, entry: Any, data: bytes | None = None) -> None:
        """Write one source entry: a folder, a link or a file. A file with `data` is complete; a
        file without it starts empty and receives its content in chunks."""
        path = inside(slot / "source", name)
        require(not under(name, op.get("macOnly", [])), "The path stays on the Mac (macOnly)")
        require(isinstance(entry, dict) and entry.get("kind") in {"directory", "link", "file"}, "Invalid source entry")
        path.parent.mkdir(parents=True, exist_ok=True)
        if path.is_symlink() or path.is_file():
            path.unlink()
        elif path.exists() and entry["kind"] != "directory":
            shutil.rmtree(path)
        if entry["kind"] == "directory":
            path.mkdir(exist_ok=True)
            return
        if entry["kind"] == "link":
            require(isinstance(entry.get("target"), str) and "\0" not in entry["target"], "Invalid link target")
            path.symlink_to(entry["target"])
            return
        length = entry.get("bytes")
        require(isinstance(length, int) and 0 <= length <= self.budget_of(op), "Invalid file size")
        self.reserve(slot.parent, self.budget_of(op), length, slot)
        if data is None:
            path.touch()
            entries = self.opdir(op["operationId"]) / "entries"
            entries.mkdir(exist_ok=True)
            atomic(entries / (key(name) + ".json"), entry)
            return
        require(len(data) == length and hashlib.sha256(data).hexdigest() == entry.get("sha256"), "The source checksum differs")
        mode = entry.get("mode")
        require(isinstance(mode, int) and 0 <= mode <= 0o777, "Invalid source file mode")
        # No fsync: the next command hashes the slot again and sends a file that a crash broke.
        path.write_bytes(data)
        path.chmod(mode)

    def slot(self, op: dict[str, Any]) -> Path:
        path = self.pool / key(op["projectId"]) / str(op["slotId"])
        lease = path / "lease.json"
        require(lease.is_file() and json.loads(lease.read_text())["operationId"] == op["operationId"],
                "The operation no longer owns its slot", "lease_lost")
        return path

    async def acquire(self, params: dict[str, Any]) -> dict[str, Any]:
        identity = params["operationId"]
        require(isinstance(identity, str) and 0 < len(identity) <= 128, "Invalid operation ID")
        for field in ("projectId", "agentId", "linePath"):
            require(isinstance(params.get(field), str) and 0 < len(params[field]) <= 4096, "Missing host command context: " + field)
        settings = self.settings(params["projectId"])
        project = self.pool / key(params["projectId"])
        async with self.locks.setdefault(str(project), asyncio.Lock()):
            project.mkdir(exist_ok=True, mode=0o700)
            require(not self.opdir(identity).exists(), "The operation already exists", "outcome_unknown")
            self.evict(project, settings["diskBudget"])
            existing = [path for path in project.iterdir() if path.is_dir()]
            free = sorted((path for path in existing if not (path / "lease.json").exists()), key=self.used_at)
            # A smaller slot count removes idle slots, the least recently used first.
            while len(existing) > settings["slots"] and free:
                victim = free.pop(0)
                shutil.rmtree(victim)
                existing.remove(victim)
            if free:
                slot = free[-1]
            else:
                require(len(existing) < settings["slots"], "All project slots are leased", "busy")
                slot = next(project / str(index) for index in range(settings["slots"]) if not (project / str(index)).exists())
                slot.mkdir(mode=0o700)
                atomic(slot / "slot.json", {"generation": os.urandom(16).hex(), "usedAt": 0})
            for name in ("source", "DerivedData", "packages"):
                (slot / name).mkdir(exist_ok=True, mode=0o700)
            directory = self.opdir(identity)
            directory.mkdir(parents=True, mode=0o700)
            artifact_dir = slot / "artifacts" / key(identity)
            artifact_dir.mkdir(parents=True, mode=0o700)
            (directory / "artifacts").symlink_to(artifact_dir, target_is_directory=True)
            meta = json.loads((slot / "slot.json").read_text())
            op = {"operationId": identity, **{field: params[field] for field in ("projectId", "agentId", "linePath")},
                  "slotId": slot.name, "generation": meta["generation"], "state": "leased", "createdAt": time.time(),
                  "slotPath": str(slot / "source"), "derivedData": str(slot / "DerivedData"),
                  "packageCache": str(slot / "packages"), "artifactsPath": str(directory / "artifacts"),
                  "diskBudget": settings["diskBudget"], "macOnly": settings["macOnly"], "features": FEATURES}
            self.save(identity, op)
            atomic(slot / "lease.json", {"operationId": identity})
            return op

    async def dispatch(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        if method == "acquire":
            return await self.acquire(params)
        identity = params["operationId"]
        op = self.load(identity)
        # The broker sets these fields; the host never accepts a slot path from the caller.
        require(params.get("agentId") == op["agentId"], "The command belongs to another agent", "forbidden")
        if method == "status":
            return op
        if method == "attach":
            after = params.get("afterSeq", 0)
            require(isinstance(after, int) and after >= 0, "Invalid event cursor")
            deadline = time.monotonic() + min(max(params.get("waitSeconds", 0), 0), 10)
            while time.monotonic() < deadline and identity in self.commands:
                if self.db.execute("SELECT 1 FROM events WHERE operation=? AND seq>?", (identity, after)).fetchone():
                    break
                await asyncio.sleep(0.025)
            events = [json.loads(row[0]) for row in self.db.execute("SELECT value FROM events WHERE operation=? AND seq>? ORDER BY seq LIMIT 16", (identity, after))]
            return {"events": events, "status": self.load(identity)}
        if method == "result.manifest":
            area: Any = params.get("area")
            require(area in {"changes", "artifacts"}, "Invalid result file area")
            require(op["state"] in {"exited", "released"}, "The command has no final files", "outcome_unknown")
            return self.listing(identity + "\0" + area, self.opdir(identity) / (area + ".json"), params.get("after", ""))
        if method == "read":
            require(op["state"] in {"exited", "released"}, "The command has no final files", "outcome_unknown")
            area = params.get("area")
            require(area in {"changes", "artifacts"}, "Invalid result file area")
            path = inside(self.opdir(identity) / area, params["path"], leaf=True)
            offset = params.get("offset", 0)
            require(isinstance(offset, int) and offset >= 0, "Invalid file offset")
            with path.open("rb") as stream:
                stream.seek(offset)
                data = stream.read(CHUNK)
            return {"data": base64.b64encode(data).decode(), "bytes": len(data)}
        slot = self.slot(op)
        mac_only: list[str] = op.get("macOnly", [])
        if method == "manifest":
            manifest_path = self.opdir(identity) / "held-manifest.json"
            if params.get("after"):
                return self.listing(identity + "\0held", manifest_path, params["after"])
            # Hashing a large slot takes minutes; the event loop keeps serving other requests.
            held = await asyncio.to_thread(self.scan_slot, slot, mac_only)
            atomic(manifest_path, held)
            return self.listing(identity + "\0held", manifest_path, "", held)
        if method == "prime":
            require(op["state"] == "leased", "The slot is no longer open for source transfer")
            caches = self.cache_list(params.get("caches", []))
            async with self.locks.setdefault("run\0" + identity, asyncio.Lock()):
                return {"primed": [await self.prime(op, slot, cache) for cache in caches]}
        if method == "sync.clone":
            require(op["state"] == "leased", "The slot is no longer open for source transfer")
            return await asyncio.to_thread(self.clone_source, op, slot, params.get("path"), params.get("entry"))
        if method == "sync.mode":
            require(op["state"] == "leased", "The slot is no longer open for source transfer")
            changes: Any = params.get("items")
            require(isinstance(changes, list) and 0 < len(changes) <= 4096, "Invalid mode changes")
            await asyncio.to_thread(self.set_modes, slot, mac_only, changes)
            return {"entries": len(changes)}
        if method == "sync.batch":
            require(op["state"] == "leased", "The slot is no longer open for source transfer")
            items: Any = params.get("items")
            require(isinstance(items, list) and 0 < len(items) <= 1024, "Invalid source batch")
            for item in items:
                require(isinstance(item, dict) and isinstance(item.get("data", ""), str), "Invalid source batch")
                content = base64.b64decode(item["data"], validate=True) if "data" in item else None
                self.place(op, slot, item.get("path"), item.get("entry"), content)
            return {"entries": len(items)}
        if method.startswith("sync."):
            require(op["state"] == "leased", "The slot is no longer open for source transfer")
            path = inside(slot / "source", params["path"])
            require(not under(params["path"], mac_only), "The path stays on the Mac (macOnly)")
            entry_dir = self.opdir(identity) / "entries"
            if method == "sync.delete":
                if path.is_symlink() or path.is_file():
                    path.unlink()
                elif path.exists() and not any(item.startswith(params["path"] + "/") for item in mac_only):
                    # A folder above a Mac-only path stays: the guest deletes its other entries one by one.
                    shutil.rmtree(path)
            elif method == "sync.entry":
                self.place(op, slot, params["path"], params["entry"])
            elif method == "sync.chunk":
                require(path.is_file() and not path.is_symlink(), "The source file is not open")
                data = base64.b64decode(params["data"], validate=True)
                require(len(data) <= CHUNK, "The source chunk exceeds its limit")
                require(path.stat().st_size == params["offset"], "The source chunk offset differs")
                entry = json.loads((entry_dir / (key(params["path"]) + ".json")).read_text())
                require(path.stat().st_size + len(data) <= entry["bytes"], "The source exceeds its declared size")
                with path.open("ab") as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
            elif method == "sync.commit":
                require(not path.is_symlink(), "The source file is a symbolic link")
                entry = json.loads((entry_dir / (key(params["path"]) + ".json")).read_text())
                with path.open("rb") as stream:
                    checksum = hashlib.file_digest(stream, "sha256").hexdigest()
                require(path.stat().st_size == entry["bytes"] and checksum == entry["sha256"], "The source checksum differs")
                require(isinstance(entry["mode"], int) and 0 <= entry["mode"] <= 0o777, "Invalid source file mode")
                path.chmod(entry["mode"])
            else:
                raise HostExecError("invalid_params", "Unknown source transfer method")
            return {"path": params["path"]}
        if method == "run":
            require(op["state"] == "leased", "The command already started", "outcome_unknown")
            argv: Any = params.get("argv")
            require(isinstance(argv, list) and 0 < len(argv) <= 1024 and all(isinstance(item, str) and "\0" not in item for item in argv), "Invalid command arguments")
            timeout = params.get("timeoutSeconds", 300)
            require(isinstance(timeout, (int, float)) and not isinstance(timeout, bool) and 0 < timeout <= 3600, "Invalid command timeout")
            # Cache folders of the line: the Mac receives them, and its changes there never return.
            caches = self.cache_list(params.get("caches", []))
            async with self.locks.setdefault("run\0" + identity, asyncio.Lock()):
                source = await asyncio.to_thread(self.scan_slot, slot, mac_only)
                # The scan let other requests run: the operation must still be leased.
                op = self.load(identity)
                require(op["state"] == "leased", "The command already started", "outcome_unknown")
                slot = self.slot(op)
                require(not conflicts(list(source)), "The source names conflict on macOS", "name_conflict")
                require(key(json.dumps(source, sort_keys=True)) == params.get("manifestSha256"), "The Mac sources differ from the layr slot", "sync_conflict")
                cwd = slot / "source"
                if params.get("cwd"):
                    cwd = inside(cwd, params["cwd"], leaf=True)
                    require(cwd.is_dir(), "The command cwd is not a directory")
                self.evict(slot.parent, self.budget_of(op), exclude=slot)
                op.update(state="running", startedAt=time.time(), caches=caches)
                self.save(identity, op)
                self.commands[identity] = asyncio.create_task(self.command(identity, dict(op), argv, cwd, timeout, source))
                return op
        if method == "cancel":
            child = self.children.get(identity)
            if child is not None:
                op["cancelRequested"] = True
                self.save(identity, op)
                await self.terminate(child)
            elif op["state"] == "unknown":
                raise HostExecError("outcome_unknown", "The old host owner is lost; inspect the slot before recovery")
            else:
                op["cancelRequested"] = True
                self.save(identity, op)
            return self.load(identity)
        if method == "release":
            require(op["state"] in {"leased", "exited"}, "A running or unknown command retains its lease")
            failure = params.get("preRunError")
            if failure is not None:
                require(op["state"] == "leased" and isinstance(failure, dict),
                        "Only a failed transfer can release an unstarted command")
                op.update(reason="sync_failed", preRunError=failure)
            op.update(state="released", collected=bool(params.get("collected")), finishedAt=time.time())
            self.save(identity, op)
            # Failed guest staging can stay active after a lost broker response. Never reuse it.
            generation = os.urandom(16).hex() if failure is not None else op["generation"]
            atomic(slot / "slot.json", {"generation": generation, "usedAt": time.time()})
            (slot / "lease.json").unlink()
            if op["collected"]:
                shutil.rmtree(self.opdir(identity) / "changes", ignore_errors=True)
            # A large slot has a large manifest and one entry file per transferred file.
            (self.opdir(identity) / "held-manifest.json").unlink(missing_ok=True)
            shutil.rmtree(self.opdir(identity) / "entries", ignore_errors=True)
            for name in ("held", "changes", "artifacts"):
                self.pages.pop(identity + "\0" + name, None)
            self.locks.pop("run\0" + identity, None)
            pending = self.cloned.pop(str(slot), None)
            if pending:
                # Clones of a command that did not start keep their hashes for the next scan.
                hashes = load_hashes(slot / "hash-cache.json")
                hashes.update(pending)
                atomic(slot / "hash-cache.json", hashes)
            self.evict(slot.parent, self.budget_of(op))
            return op
        raise HostExecError("invalid_params", "Unknown host method")

    @staticmethod
    async def terminate(child: asyncio.subprocess.Process) -> None:
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            await asyncio.wait_for(child.wait(), 3)
        except asyncio.TimeoutError:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            await child.wait()

    async def command(self, identity: str, op: dict[str, Any], argv: list[str], cwd: Path,
                      timeout: float, before: dict[str, dict[str, Any]]) -> None:
        reason, code = "exited", None
        output_bytes = 0
        child = None
        slot = self.slot(op)
        environment = {**os.environ, "HOST_EXEC_DERIVED_DATA": op["derivedData"],
                       "HOST_EXEC_PACKAGE_CACHE": op["packageCache"], "HOST_EXEC_ARTIFACTS": op["artifactsPath"]}
        async def drain(pipe: asyncio.StreamReader, stream: str) -> None:
            nonlocal output_bytes, reason
            decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
            tail = ""
            keep = max(map(len, aliases(op["slotPath"])))
            while block := await pipe.read(32768):
                output_bytes += len(block)
                if output_bytes > 64 * 1024**2:
                    reason = "output_limit"
                    assert child is not None
                    await self.terminate(child)
                    break
                tail += decoder.decode(block)
                # Preserve any path prefix split across pipe reads.
                end = max(0, len(tail) - keep)
                for path in aliases(op["slotPath"]):
                    start = tail.rfind(path, 0, end + len(path))
                    if 0 <= start < end < start + len(path):
                        end = start
                if end:
                    self.event(identity, {"stream": stream, "text": mapped(tail[:end], op["slotPath"], op["linePath"])})
                    tail = tail[end:]
            self.event(identity, {"stream": stream, "text": mapped(tail + decoder.decode(b"", final=True), op["slotPath"], op["linePath"])})
        try:
            if self.load(identity).get("cancelRequested"):
                reason = "cancelled"
            else:
                child = await asyncio.create_subprocess_exec(*argv, cwd=str(cwd), env=environment,
                    stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True)
                self.children[identity] = child
                op["pid"] = child.pid
                saved = self.load(identity)
                op.update(saved, pid=child.pid)
                self.save(identity, op)
                if op.get("cancelRequested"):
                    await self.terminate(child)
                assert child.stdout is not None and child.stderr is not None
                readers = [asyncio.create_task(drain(child.stdout, "stdout")), asyncio.create_task(drain(child.stderr, "stderr"))]
                async def budget_watch() -> None:
                    nonlocal reason
                    budget = self.budget_of(op)
                    while child is not None and child.returncode is None:
                        started = time.monotonic()
                        # The walk of a large slot takes seconds: it runs off the event loop,
                        # and the pause grows with it so that the walk takes at most a fifth of the time.
                        used = await asyncio.to_thread(self.size, slot.parent)
                        self.usage[str(slot.parent)] = used
                        if used > budget:
                            try:
                                self.evict(slot.parent, budget, exclude=slot)
                            except HostExecError:
                                reason = "disk_budget"
                                await self.terminate(child)
                                return
                        await asyncio.sleep(max(1.0, 4 * (time.monotonic() - started)))
                watchdog = asyncio.create_task(budget_watch())
                try:
                    await asyncio.wait_for(child.wait(), timeout)
                except asyncio.TimeoutError:
                    reason = "timeout"
                    await self.terminate(child)
                finally:
                    watchdog.cancel()
                    await asyncio.gather(watchdog, return_exceptions=True)
                    try:
                        os.killpg(child.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                await asyncio.wait_for(asyncio.gather(*readers), 6)
                code = child.returncode
                if self.load(identity).get("cancelRequested"):
                    reason = "cancelled"
            # Mac-only paths and cache folders never return to the line, so they are not compared.
            caches: list[str] = op.get("caches", [])
            after = await asyncio.to_thread(self.scan_slot, slot, [*op.get("macOnly", []), *caches])
            before = {path: entry for path, entry in before.items() if not under(path, caches)}
            changed = {path: after.get(path) for path in before.keys() | after.keys() if before.get(path) != after.get(path)}
            result_dir = self.opdir(identity) / "changes"
            result_dir.mkdir()
            for path, entry in changed.items():
                if entry is None or entry["kind"] == "directory":
                    continue
                source, target = inside(slot / "source", path), inside(result_dir, path)
                target.parent.mkdir(parents=True, exist_ok=True)
                if entry["kind"] == "link":
                    target.symlink_to(entry["target"])
                else:
                    shutil.copyfile(source, target, follow_symlinks=False)
                    target.chmod(entry["mode"])
            artifact_manifest = scan(self.opdir(identity) / "artifacts")
            atomic(self.opdir(identity) / "changes.json", changed)
            atomic(self.opdir(identity) / "artifacts.json", artifact_manifest)
            op.update(state="exited", exitCode=code, reason=reason, exitedAt=time.time(), changedPaths=len(changed),
                      artifactFiles=len(artifact_manifest), nameConflicts=conflicts(list(after)))
            self.save(identity, op)
            self.event(identity, {"state": "exited", "exitCode": code, "reason": reason})
        except Exception as exc:
            if child is not None:
                await self.terminate(child)
            op.update(state="unknown", reason="host_command_failed", error=str(exc)[:1000])
            self.save(identity, op)
        finally:
            self.children.pop(identity, None)
            self.commands.pop(identity, None)

    def busy(self) -> bool:
        """Work of this process that a stop would cut. Older unknown outcomes and pending receipts
        stay in the state folder, and the next service reports them the same way."""
        return self.retiring or bool(self.commands or self.active or self.children or self.reading)

    async def request(self, request: dict[str, Any]) -> dict[str, Any]:
        identity: Any = request.get("id")
        try:
            require(isinstance(identity, str) and 0 < len(identity) <= 128, "Invalid request ID")
            token = request.get("token", "")
            require(isinstance(token, str) and hmac.compare_digest(token, self.token), "The guest host token differs", "forbidden")
            method, params = request["method"], request.get("params", {})
            require(isinstance(method, str) and isinstance(params, dict), "Invalid request")
            digest = hashlib.sha256(json.dumps([method, params], sort_keys=True, allow_nan=False).encode()).hexdigest()
            if method == "health":
                return {"id": identity, "result": {"protocol": 1, "buildId": self.build_id, "pid": os.getpid(),
                    "activeCommands": len(self.commands), "activeRequests": len(self.active), "busy": self.busy()}}
            require(not self.retiring, "The host_exec service is retiring", "busy")
            if method == "service.retire":
                require(params.get("buildId") == self.build_id and params.get("pid") == os.getpid(),
                        "The service build or process differs", "id_conflict")
                require(not self.busy(), "The host_exec service has a running command or request", "busy")
                self.retiring = True
                return {"id": identity, "result": {"state": "retiring", "buildId": self.build_id, "pid": os.getpid()}}
            readonly = method in {"status", "manifest", "attach", "read", "result.manifest"}
            if readonly:
                self.reading += 1
                try:
                    return {"id": identity, "result": await self.dispatch(method, params)}
                finally:
                    self.reading -= 1
            saved = self.db.execute("SELECT digest,response FROM receipts WHERE id=?", (identity,)).fetchone()
            if saved:
                require(saved[0] == digest, "The request ID has different content", "id_conflict")
                if identity in self.active:
                    return await asyncio.shield(self.active[identity])
                require(saved[1] is not None, "The request has no proven result; inspect its operation", "outcome_unknown")
                value: dict[str, Any] = json.loads(saved[1])
                return value
            self.db.execute("INSERT INTO receipts VALUES (?,?,NULL)", (identity, digest))
            self.db.commit()
            async def operation() -> dict[str, Any]:
                try:
                    result = {"id": identity, "result": await self.dispatch(method, params)}
                except HostExecError as exc:
                    result = {"id": identity, "error": exc.object()}
                except Exception as exc:
                    result = {"id": identity, "error": {"code": "outcome_unknown", "message": str(exc)[:1000]}}
                self.db.execute("UPDATE receipts SET response=? WHERE id=?", (json.dumps(result), identity))
                self.db.commit()
                return result
            task = asyncio.create_task(operation())
            self.active[identity] = task
            try:
                return await asyncio.shield(task)
            finally:
                self.active.pop(identity, None)
        except HostExecError as exc:
            return {"id": identity, "error": exc.object()}
        except Exception:
            return {"id": identity, "error": {"code": "invalid_request", "message": "Invalid host request"}}

    async def client(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        if self.clients >= 64:
            writer.close()
            return
        self.clients += 1
        try:
            line = await asyncio.wait_for(reader.readline(), 30)
            require(len(line) <= FRAME_LIMIT, "The request frame exceeds its limit")
            response = await self.request(json.loads(line))
            data = json.dumps(response).encode() + b"\n"
            require(len(data) <= FRAME_LIMIT, "The host result exceeds its frame limit", "output_limit")
            writer.write(data)
            await asyncio.wait_for(writer.drain(), 5)
        except Exception:
            pass
        finally:
            self.clients -= 1
            writer.close()
            if self.retiring:
                # The service stops after the retire answer; the server waits for open connections.
                self.stopped.set()


async def serve(state: Path) -> None:
    service = HostService(state)
    path = state / "host-exec.sock"
    path.unlink(missing_ok=True)
    server = await asyncio.start_unix_server(service.client, path=str(path), limit=FRAME_LIMIT + 1)
    path.chmod(0o600)
    try:
        async with server:
            # A retire request ends this service; Studio then starts the current build.
            await service.stopped.wait()
    finally:
        for child in list(service.children.values()):
            await service.terminate(child)
        service.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("state", type=Path)
    options = parser.parse_args()
    os.umask(0o077)
    asyncio.run(serve(options.state))
