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
import uuid

from common import GuestError, atomic_json, bounded_lock, decode, number, require, supervised
from layr_share import command, project_id
from project_env import wrap


def expect(condition, message, code):
    if not condition:
        raise GuestError(code, message)


class Projects:
    UPLOADS = Path("/var/lib/codex-studio/guest/uploads")

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

    # The layr rules of a Studio project (docs/btrfs-local-vcs.md, decisions 18 and 23). Version
    # 1: the lead (the owner of main) may also commit in main, and the agents' group may not
    # change roles, replicate, change remotes, import or export records, or make backups.
    POLICY_VERSION = 1

    async def apply_policy(self, project):
        marker = self.state / ("policy-" + project_id(project) + ".json")
        if marker.exists() and json.loads(marker.read_text()).get("version", 0) >= self.POLICY_VERSION:
            return
        from layr_agents import AGENTS_GROUP, ensure_agents_group
        ensure_agents_group()
        await self.layr(["--project", project, "protect", "main", "--direct"])
        await self.layr(["--project", project, "access", "deny", "@" + AGENTS_GROUP,
                         "access", "sync", "remote", "records", "backup"])
        atomic_json(marker, {"version": self.POLICY_VERSION})

    async def ensure(self, project):
        folder = self.folder(project)
        cwd = folder / "lines/main"
        expect(cwd.is_dir(), "The layr project does not exist", "not_found")
        await self.apply_policy(project)
        # Read identity from the owned main folder, not from caller input.
        user = pwd.getpwuid(cwd.stat().st_uid)
        state = await self.layr(["rev-parse", "HEAD"], cwd=cwd)
        return {"projectId": project, "mainLine": "main", "cwd": str(cwd),
                "owner": user.pw_name, "ownerUid": user.pw_uid, "home": user.pw_dir,
                "stateId": state, "share": await self.share.status()}

    async def exec(self, request_id, params):
        """Run a command as the owner of main, in main: the operator's way to set up a project
        (docs/vm-layr.md, "Operator commands"). Never root; the owner is read from main."""
        project = project_id(params.get("projectId"))
        main = self.folder(project) / "lines/main"
        expect(main.is_dir(), "The layr project does not exist", "not_found")
        argv = params.get("argv")
        require(isinstance(argv, list) and argv and all(isinstance(a, str) and a and "\0" not in a for a in argv),
                "The command argv is invalid")
        subpath = params.get("subpath", ".")
        require(isinstance(subpath, str) and "\0" not in subpath, "The command folder is invalid")
        cwd = (main / subpath).resolve()
        require(cwd.is_relative_to(main.resolve()) and cwd.is_dir(), "The command folder is outside main")
        timeout = number(params.get("timeoutSeconds"), 300, 1, 1800)
        stdin = params.get("stdin", "")
        decode(stdin)
        user = pwd.getpwuid(main.stat().st_uid)
        # Like the lead, the command gets the project's secrets (project_env.py).
        config = {"argv": wrap(self.state, project, user.pw_name, argv), "cwd": str(cwd),
                  "env": {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": user.pw_dir, "USER": user.pw_name,
                          "LOGNAME": user.pw_name, "LANG": "C.UTF-8", "LAYR_ROOT": str(self.root)},
                  "stdin": stdin, "timeoutSeconds": timeout, "outputLimitBytes": 512 * 1024}
        # The command folder keeps the argv and the output: the record of what the operator ran.
        # It shares `commands` with layr.exec; a short path keeps the control socket under 108 bytes.
        directory = self.state / "commands" / uuid.uuid5(uuid.NAMESPACE_URL, request_id).hex
        return await supervised(directory, config, {"projectId": project, "requestId": request_id}, timeout + 5)

    async def seed(self, request_id, params):
        """Give main a warm layer from the Mac (docs/vm-layr.md, "Seeded layers"): an uploaded
        folder becomes main/<path>, owned by the owner of main, who then saves caches, so each
        new line starts with a copy-on-write copy. An existing folder is never replaced."""
        project = project_id(params.get("projectId"))
        main = self.folder(project) / "lines/main"
        expect(main.is_dir(), "The layr project does not exist", "not_found")
        path = params.get("path")
        require(isinstance(path, str) and 0 < len(path) <= 512 and "\0" not in path and not path.startswith("/")
                and all(part not in {"", ".", ".."} for part in path.split("/")), "The layer path is invalid")
        marker = self.state / ("seed-" + hashlib.sha256(request_id.encode()).hexdigest() + ".json")
        if marker.exists():
            return json.loads(marker.read_text())
        source = params.get("source")
        require(isinstance(source, str) and Path(source).is_absolute(), "The seed source must be absolute")
        source = Path(source)
        # The guest service expands the upload in its private folder (root only, mode 0700);
        # no agent can change it, so it needs no frozen copy.
        require(source.parent.parent == self.UPLOADS and source.name == "expanded" and not source.is_symlink()
                and source.resolve() == source and source.is_dir(), "The seed source must be an expanded upload")
        async with bounded_lock(self.locks, project):
            user = pwd.getpwuid(main.stat().st_uid)
            target = main / path
            parent = main
            for part in path.split("/")[:-1]:
                parent = parent / part
                require(not parent.is_symlink(), "The layer path has a symbolic link parent")
                if not parent.exists():
                    parent.mkdir(mode=0o755)
                    os.chown(parent, user.pw_uid, user.pw_gid)
                require(parent.is_dir(), "The layer path has a file parent")
            seeded = not (target.exists() or target.is_symlink())
            if seeded:
                # Named by the path: a later request after a failed one removes the earlier partial copy.
                partial = parent / (".layr-seed-" + hashlib.sha256(path.encode()).hexdigest()[:16])
                try:
                    if partial.exists():
                        await remove_tree(partial)
                    # A subvolume is already a layr cache: `save --caches` keeps it and makes no
                    # second copy. Reflinks share the uploaded blocks; one sync for the tree.
                    await command(["btrfs", "subvolume", "create", partial], timeout=120)
                    await command(["cp", "-a", "--reflink=always", "--", str(source) + "/.", partial], timeout=1800)
                    # The archive's folder modes, before the owner gets the tree (upload.FOLDER_MODES).
                    modes = folder_modes(source.parent / "folder-modes.json")
                    restore_folder_modes(partial, modes)
                    await command(["chown", "-R", "--no-dereference", user.pw_name + ":" + user.pw_name, partial],
                                  timeout=1800)
                    partial.chmod(modes.get(".", 0o755))
                    await command(["sync", "-f", partial], timeout=600)
                    os.rename(partial, target)
                except BaseException:
                    await remove_tree(partial)
                    raise
            # Also after an earlier attempt that placed the folder but did not save. The layer
            # rules decide: the folder must be ignored and marked regenerable in main.
            await self.layr(["save", "--caches", "-m", "Seed layer " + path], cwd=main, owner=user.pw_name)
            result = {**await self.ensure(project), "path": path, "seeded": seeded}
            atomic_json(marker, result)
        return result

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
                # Studio chose the files (the code rule of docs/vm-layr.md): ignored files such as
                # .env and a nested repository that its parent ignores belong in main too.
                await self.layr(["init", project, "--from", str(private_source), "--include-ignored",
                                 "--owner", owner["owner"]])
            finally:
                await command(["btrfs", "subvolume", "delete", private_source])
            await self.apply_policy(project)
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
        # The used bytes of the store's file system. du counts each block once per line and
        # snapshot that shares it, and with hundreds of lines it passed the health deadline.
        return {"version": version, "state": "ready" if daemon else "unavailable", "root": str(self.root),
                "storeBytes": usage.total - usage.free, "freeBytes": usage.free,
                "totalBytes": usage.total, "share": await self.share.status()}


def folder_modes(path):
    """The folder modes of a seed archive: relative path -> permission bits. A missing file (an
    upload extracted before this record existed) gives no modes."""
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    except FileNotFoundError:
        return {}
    with os.fdopen(descriptor) as stream:
        value = json.loads(stream.read(64 * 1024 * 1024))
    require(isinstance(value, dict) and all(isinstance(mode, int) for mode in value.values()),
            "The seed folder modes are invalid")
    return {name: mode & 0o777 for name, mode in value.items()}


def restore_folder_modes(folder, modes):
    """Give each folder of the seed copy its archive mode, through descriptors and never through
    a link. Bottom up, so a folder without write or search permission comes last."""
    if not modes:
        return
    top = os.open(folder, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for directory, _, _, descriptor in os.fwalk(".", topdown=False, follow_symlinks=False, dir_fd=top):
            mode = modes.get(os.path.normpath(directory))
            if mode is not None:
                os.fchmod(descriptor, mode)
    finally:
        os.close(top)


async def remove_tree(path):
    if not (path.exists() or path.is_symlink()):
        return
    try:
        await command(["btrfs", "subvolume", "delete", path], timeout=1800)
    except GuestError:
        await command(["rm", "-rf", "--one-file-system", path], timeout=1800)


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
