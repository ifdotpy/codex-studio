"""Authenticated Samba exports of main and states on read-only bind mounts.

Samba listens only on the guest loopback; share_bridge.py carries the Mac's
connections to it over vsock (docs/vm-layr.md, Mac share)."""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import pwd
import re

from common import GuestError, atomic_bytes, atomic_json, require


# Finder shows a mounted share by its share name, so the Mac names it after the project
# folder. Plain ASCII keeps the smb:// source free of escapes; Samba compares case-blind.
SHARE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,47}")
RESERVED_SHARES = {"global", "homes", "printers", "print$", "ipc$"}


def project_id(value):
    require(isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,99}", value),
            "projectId must start with an ASCII letter or digit and contain letters, digits, hyphens, or underscores")
    return value


async def command(argv, *, data=None, timeout=30):
    process = await asyncio.create_subprocess_exec(*map(str, argv),
        stdin=asyncio.subprocess.PIPE if data is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        output, _ = await asyncio.wait_for(process.communicate(data), timeout)
    except asyncio.TimeoutError as exc:
        process.kill()
        await process.wait()
        raise GuestError("outcome_unknown", "The guest command exceeded its deadline") from exc
    if process.returncode:
        raise GuestError("operation_failed", "The guest command failed: " + str(argv[0]))
    return output.decode().strip()


class Share:
    def __init__(self, state: Path, root: Path):
        self.state, self.root = state, root
        self.export_root = state / "exports"
        self.configuration = state / "smb.conf"
        self.lock = asyncio.Lock()

    async def configure(self, params):
        password = params.get("password")
        require(isinstance(password, str) and re.fullmatch(r"[a-f0-9]{64}", password),
                "The SMB credential must contain 64 hexadecimal characters")
        async with self.lock:
            saved = self.state / "smb-password"
            if saved.exists():
                require(saved.read_text() == password, "The SMB credential differs from the saved installation credential")
            else:
                try:
                    pwd.getpwnam("studio-view")
                except KeyError:
                    await command(["useradd", "--system", "--no-create-home", "--shell", "/usr/sbin/nologin", "studio-view"])
                await command(["smbpasswd", "-s", "-a", "studio-view"], data=(password + "\n" + password + "\n").encode())
                atomic_bytes(saved, password.encode())
                saved.chmod(0o600)
            await self.refresh()
        return await self.status()

    async def export(self, project):
        project = project_id(project)
        destination = self.export_root / project
        destination.mkdir(parents=True, exist_ok=True, mode=0o700)
        for name, source in (("main", self.root / "projects" / project / "lines/main"),
                             ("states", self.root / "projects" / project / "states")):
            target = destination / name
            target.mkdir(exist_ok=True, mode=0o700)
            require(source.is_dir() and not source.is_symlink(), "The layr export source is not a real directory")
            mounted = os.path.ismount(target)
            if mounted and (target.stat().st_ino, target.stat().st_dev) != (source.stat().st_ino, source.stat().st_dev):
                await command(["umount", "-l", target])
                mounted = False
            if not mounted:
                await command(["mount", "--bind", source, target])
                await command(["mount", "-o", "remount,bind,ro,nosuid,nodev,noexec", target])
            options = await command(["findmnt", "-n", "-o", "OPTIONS", "--target", target])
            require("ro" in options.split(","), "The export bind mount is not read-only")
        atomic_json(self.state / ("export-" + project + ".json"), {"projectId": project})
        if (self.state / "smb-password").exists():
            await self.refresh()

    def share_name(self, record):
        return record.get("name") or record["projectId"]

    async def name(self, params):
        """Give a project's share the Mac's preferred name, unique among shares; keep it
        while it still follows that preference."""
        project = project_id(params.get("projectId"))
        preferred = params.get("name")
        require(isinstance(preferred, str) and SHARE_NAME.fullmatch(preferred)
                and preferred.lower() not in RESERVED_SHARES, "Invalid share name")
        async with self.lock:
            path = self.state / ("export-" + project + ".json")
            if not path.exists():
                raise GuestError("not_found", "The project has no share export")
            record = json.loads(path.read_text())
            taken = {self.share_name(json.loads(other.read_text())).lower()
                     for other in self.state.glob("export-*.json") if other != path}
            current = record.get("name")
            follows = current in {preferred} | {preferred + "-" + str(n) for n in range(2, 100)}
            if not (follows and current.lower() not in taken):
                current = next((candidate for candidate in [preferred] + [preferred + "-" + str(n) for n in range(2, 100)]
                                if candidate.lower() not in taken), None)
                require(current is not None, "No free share name for this project")
                record["name"] = current
                atomic_json(path, record)
                if (self.state / "smb-password").exists():
                    await self.refresh()
        return {"projectId": project, "name": current}

    async def restore(self):
        # No agent owns this directory. Names come from saved validated registrations.
        for record in sorted(self.state.glob("export-*.json")):
            await self.export(json.loads(record.read_text())["projectId"])
        if (self.state / "smb-password").exists():
            await self.refresh()

    async def refresh(self):
        text = "\n".join([
            "[global]", "server role = standalone server", "security = user",
            "map to guest = Never", "server min protocol = SMB3", "smb ports = 445",
            "interfaces = lo", "bind interfaces only = yes",
            "hosts allow = 127.0.0.1", "hosts deny = ALL",
            "disable netbios = yes", "load printers = no", "disable spoolss = yes",
            "printing = bsd", "printcap name = /dev/null", "unix extensions = no",
            "server signing = mandatory", "smb encrypt = required", "", ])
        for path in sorted(self.state.glob("export-*.json")):
            record = json.loads(path.read_text())
            project = project_id(record["projectId"])
            text += "\n".join(["[" + self.share_name(record) + "]", "path = " + str(self.export_root / project),
                "read only = yes", "guest ok = no", "valid users = studio-view",
                "force user = root", "browseable = yes", "follow symlinks = no",
                "wide links = no", "veto files = /.DS_Store/", "delete veto files = no", "", ""])
        atomic_bytes(self.configuration, text.encode())
        await command(["testparm", "-s", self.configuration])
        # A dedicated unit avoids the distribution's default public SMB listener.
        await command(["systemctl", "reload-or-restart", "codex-studio-share.service"])

    async def status(self):
        if not (self.state / "smb-password").exists():
            return {"state": "unconfigured", "protocol": "smb", "readOnly": True}
        try:
            for unit in ("codex-studio-share.service", "codex-studio-share-bridge.service"):
                await command(["systemctl", "is-active", "--quiet", unit])
            state = "ready"
        except GuestError:
            state = "failed"
        records = {}
        for path in self.state.glob("export-*.json"):
            record = json.loads(path.read_text())
            records[record["projectId"]] = self.share_name(record)
        return {"state": state, "protocol": "smb", "readOnly": True, "transport": "vsock",
                "user": "studio-view", "projects": sorted(records), "names": records}
