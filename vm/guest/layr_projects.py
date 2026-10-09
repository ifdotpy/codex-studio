"""Project import keeps the guest main authoritative after the first import."""
from __future__ import annotations

import asyncio
import hashlib
import json
import os
from pathlib import Path
import pwd
import shutil
import stat
import fcntl

from common import GuestError, atomic_json, bounded_lock, require
from layr_share import command, project_id


def expect(condition, message, code):
    if not condition:
        raise GuestError(code, message)


class Projects:
    def __init__(self, state, root, share, agents):
        self.state, self.root, self.share, self.agents = state, root, share, agents
        self.locks = {}

    def folder(self, project):
        return self.root / "projects" / project_id(project)

    def owner(self, project):
        # The VM-agents module uses the same project identity for subsequent leads.
        if self.agents is not None:
            return self.agents.ensure_project_owner(project)
        owner = "studio-p-" + hashlib.sha256(project.encode()).hexdigest()[:16]
        try:
            user = pwd.getpwnam(owner)
        except KeyError:
            import subprocess
            subprocess.run(["useradd", "--create-home", "--shell", "/bin/bash", owner], check=True, timeout=15)
            user = pwd.getpwnam(owner)
        return {"owner": owner, "uid": user.pw_uid, "gid": user.pw_gid, "home": user.pw_dir}

    async def layr(self, argv, *, cwd=None, owner=None):
        environment = {**os.environ, "LAYR_ROOT": str(self.root), "LAYR_SOCKET": "/run/layr/layr.sock"}
        executable = ["/usr/local/bin/layr", *argv]
        if owner:
            executable = ["runuser", "-u", owner, "--", *executable]
        process = await asyncio.create_subprocess_exec(*executable, cwd=cwd or "/", env=environment,
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            output, _ = await asyncio.wait_for(process.communicate(), 1800)
        except asyncio.TimeoutError as exc:
            process.kill()
            await process.wait()
            raise GuestError("outcome_unknown", "The layr operation exceeded its deadline") from exc
        expect(process.returncode == 0, "The layr operation failed", "operation_failed")
        return output.decode().strip()

    async def ensure(self, project):
        folder = self.folder(project)
        cwd = folder / "lines/main"
        expect(cwd.is_dir(), "The layr project does not exist", "not_found")
        # Read identity from the owned main folder, not from caller input.
        user = pwd.getpwuid(cwd.stat().st_uid)
        state = await self.layr(["rev-parse", "HEAD"], cwd=cwd)
        return {"projectId": project, "mainLine": "main", "cwd": str(cwd),
                "owner": user.pw_name, "ownerUid": user.pw_uid, "home": user.pw_dir,
                "stateId": state, "share": await self.share.status()}

    async def dispatch(self, request_id, method, params):
        project = project_id(params.get("projectId"))
        if method == "project.ensure":
            return await self.ensure(project)
        incremental = params.get("incremental", False)
        require(type(incremental) is bool, "incremental must be a boolean")
        async with bounded_lock(self.locks, project):
            exists = (self.folder(project) / "lines/main").is_dir()
            if exists and not incremental:
                # A new identity also cannot re-import or replace an existing main.
                return {**await self.ensure(project), "imported": False}
            source = params.get("source")
            require(isinstance(source, str) and Path(source).is_absolute(), "The import source must be absolute")
            source = Path(source)
            upload_root = Path("/var/lib/codex-studio/projects")
            require(source.parent == upload_root and not source.is_symlink()
                    and source.resolve() == source and source.is_dir(), "The import source must be a completed upload root")
            owner = self.owner(project)
            require(params.get("owner") in {None, owner["owner"]}, "The project owner differs from its installation identity")
            if incremental:
                expect(exists, "The layr project does not exist", "not_found")
                previous = await self.ensure(project)
                expect(params.get("expectedStateId") == previous["stateId"],
                        "The main state differs from the expected state", "stale_state")
                # The incoming source becomes a separate project, then a reviewed line.
                # A full tree replaces that line only, including removed paths.
                line = "import-" + hashlib.sha256(request_id.encode()).hexdigest()[:24]
                cwd = self.folder(project) / "lines" / line
                marker = self.state / ("import-" + hashlib.sha256(request_id.encode()).hexdigest() + ".json")
                expect(not cwd.exists(), "The import line already exists; inspect its saved state", "outcome_unknown")
                await self.layr(["branch", line, "--owner", owner["owner"]], cwd=previous["cwd"])
                # rsync works as the line owner and never changes the guest main.
                # Source is copied to a root-owned private snapshot before root reads it.
                private_source = await self.freeze_source(source, request_id)
                try:
                    await command(["rsync", "-a", "--delete", "--exclude=.git", private_source.as_posix() + "/", cwd.as_posix() + "/"], timeout=1800)
                    await command(["chown", "-R", owner["owner"] + ":" + owner["owner"], cwd], timeout=1800)
                    await self.layr(["add", "-A"], cwd=cwd, owner=owner["owner"])
                    await self.layr(["commit", "--allow-empty", "-m", "Explicit Mac re-import"], cwd=cwd, owner=owner["owner"])
                    state = await self.layr(["rev-parse", "HEAD"], cwd=cwd)
                    atomic_json(marker, {"line": line, "stateId": state})
                    return {**previous, "imported": True, "importLine": line,
                            "importCwd": str(cwd), "importStateId": state}
                finally:
                    await command(["btrfs", "subvolume", "delete", private_source])
            private_source = await self.freeze_source(source, request_id)
            try:
                await self.layr(["init", project, "--from", str(private_source), "--owner", owner["owner"]])
            finally:
                await command(["btrfs", "subvolume", "delete", private_source])
            await self.share.export(project)
            return {**await self.ensure(project), "imported": True}

    async def freeze_source(self, source, request_id):
        # Open source entries without following links. Uploaded roots are normal
        # directories, so first copy into a private subvolume, then make it read-only.
        staging = self.state / "imports"
        staging.mkdir(mode=0o700, exist_ok=True)
        target = staging / hashlib.sha256(request_id.encode()).hexdigest()
        expect(not target.exists(), "The import snapshot exists without a proven result", "outcome_unknown")
        await command(["btrfs", "subvolume", "create", target], timeout=120)
        try:
            await asyncio.to_thread(copy_source, source, target)
            await command(["btrfs", "property", "set", "-ts", target, "ro", "true"])
        except Exception:
            await command(["btrfs", "subvolume", "delete", target])
            raise
        return target

    async def health(self):
        try:
            version = await self.layr(["--version"])
            await self.layr(["projects"], owner="studio")
            daemon = True
        except (GuestError, OSError):
            version, daemon = None, False
        usage = shutil.disk_usage(self.root)
        used = await command(["du", "-s", "-B1", self.root], timeout=30)
        return {"version": version, "state": "ready" if daemon else "unavailable", "root": str(self.root),
                "storeBytes": int(used.split()[0]), "freeBytes": usage.free,
                "totalBytes": usage.total, "share": await self.share.status()}


def copy_source(source, target):
    descriptor = os.open(source, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    count = 0

    def copy(directory, destination):
        nonlocal count
        for name in os.listdir(directory):
            count += 1
            require(count <= 1_000_000, "The import tree exceeds the entry limit")
            meta = os.stat(name, dir_fd=directory, follow_symlinks=False)
            path = destination / name
            if stat.S_ISLNK(meta.st_mode):
                os.symlink(os.readlink(name, dir_fd=directory), path)
                continue
            if stat.S_ISDIR(meta.st_mode):
                child = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=directory)
                try:
                    path.mkdir(mode=0o700)
                    copy(child, path)
                    path.chmod(stat.S_IMODE(meta.st_mode) & 0o777)
                finally:
                    os.close(child)
            else:
                require(stat.S_ISREG(meta.st_mode), "The import tree contains an unsupported file type")
                child = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
                with os.fdopen(child, "rb") as stream, path.open("xb") as output:
                    current = os.fstat(stream.fileno())
                    expect(stat.S_ISREG(current.st_mode) and current.st_ino == meta.st_ino,
                            "The import source changed during its copy", "outcome_unknown")
                    try:
                        fcntl.ioctl(output.fileno(), 0x40049409, stream.fileno())
                    except OSError:
                        shutil.copyfileobj(stream, output, 1024 * 1024)
                    output.flush()
                    os.fsync(output.fileno())
                path.chmod(stat.S_IMODE(meta.st_mode) & 0o777)
            os.utime(path, ns=(meta.st_atime_ns, meta.st_mtime_ns), follow_symlinks=False)

    try:
        copy(descriptor, target)
    finally:
        os.close(descriptor)
