"""In-place guest updates: new guest files without a VM reboot (docs/linux-vm-workspaces.md,
"Guest updates"). These root broker methods own the rules: which files an update may replace
in place, and which units then restart. Provider owners survive every one of these restarts
(KillMode=process, detached supervisors), so running agents keep their turns.

The studio user relays every request, so `update.apply` needs a signature with the update key
that provisioning wrote for root only. Without it the studio user could install root code."""
from __future__ import annotations

import asyncio
import hashlib
import hmac
import json
import os
from pathlib import Path
import re
import time

from common import (GuestError, atomic_bytes, atomic_json, decode, identifier, integer,
                    private_dir, require)
from shared_values_generated import (GUEST_PAYLOAD_RECORD_PATH, GUEST_UPDATE_KEY_PATH,
                                     GUEST_UPDATE_PART_BYTES)

METHODS = {"update.plan", "update.stage", "update.apply", "update.status"}
READ_METHODS = {"update.plan", "update.status"}

OPT = Path("/opt/codex-studio")
ADMIN_UNIT = "codex-studio-layr-admin.service"
GUEST_UNIT = "codex-studio-guest.service"
LAYR_UNIT = "codex-studio-layr.service"
# The guest Python modules run in the guest service, the root broker and the provider workers.
# The share bridge runs alone. Other guest files (install scripts) run only during an install.
SHARE_BRIDGE = "vm/guest/share_bridge.py"
# Files that an update replaces in place. Anything else (the provider CLIs, the Claude bridge
# and its packages, the process supervisor in scripts/, the provision script) needs the reboot
# path: running provider processes use them.
IN_PLACE = ("vm/guest/", "vm/layr/", "vm/layr-desk/", ".agents/skills/")
MAX_FILES = 4000
MAX_FILE_BYTES = 64 * 1024 * 1024
DRAIN_SECONDS = 600
# Stages of an operation that wrote files and did not finish its install or its restarts. A
# later plan and apply of any operation include its files, so their work is not lost.
UNFINISHED = {"writing", "install_failed", "restart_failed"}
INSTALL_SECONDS = 1500
_SHA = re.compile(r"[0-9a-f]{64}")


def relative(value):
    require(isinstance(value, str) and 0 < len(value) <= 512 and not value.startswith("/")
            and "\0" not in value and ".." not in Path(value).parts and Path(value).as_posix() == value,
            "The update path is invalid")
    return value


def sha(value):
    require(isinstance(value, str) and _SHA.fullmatch(value) is not None, "The SHA-256 value is invalid")
    return value


def manifest(value):
    require(isinstance(value, dict) and 0 < len(value) <= MAX_FILES, "The update manifest is invalid")
    return {relative(name): sha(digest) for name, digest in value.items()}


def signature(key: bytes, operation: str, files: dict) -> str:
    """The Mac signs exactly the operation and its manifest."""
    body = json.dumps({"operationId": operation, "files": files}, sort_keys=True, separators=(",", ":"))
    return hmac.new(key, body.encode(), hashlib.sha256).hexdigest()


def classify(changed):
    """The path an update takes for these changed files, and what it restarts after."""
    reboot = sorted(name for name in changed if not name.startswith(IN_PLACE))
    restarts = set()
    if any(name.startswith(("vm/layr/", "vm/layr-desk/")) for name in changed):
        # install-layr.sh rebuilds layr; the new daemon starts with a restart. The broker
        # requires the daemon, so systemd restarts the broker too.
        restarts.add(LAYR_UNIT)
    for name in changed:
        if not name.startswith("vm/guest/"):
            continue
        if name == SHARE_BRIDGE:
            restarts.add("codex-studio-share-bridge.service")
        elif name.endswith(".py"):
            restarts.update({ADMIN_UNIT, GUEST_UNIT})
        elif name.endswith(".service"):
            restarts.add(Path(name).name)
    restarts.discard("codex-studio-grow.service")  # it runs once at boot
    return {"reboot": reboot, "restarts": sorted(restarts),
            "install": any(name.startswith(("vm/guest/", "vm/layr/", "vm/layr-desk/")) for name in changed)}


def file_sha(path: Path):
    try:
        with path.open("rb") as stream:
            return hashlib.file_digest(stream, "sha256").hexdigest()
    except FileNotFoundError:
        return None


class UpdateHandlers:
    def __init__(self, state: Path, root: Path, *, opt=OPT, key_path=Path(GUEST_UPDATE_KEY_PATH),
                 record_path=Path(GUEST_PAYLOAD_RECORD_PATH), busy=lambda: 0, runner=None,
                 monotonic=time.monotonic, wall_clock=time.time, sleep=asyncio.sleep):
        self.state, self.root = state, root
        self.opt, self.key_path, self.record_path = opt, key_path, record_path
        self.updates = state / "updates"
        # The number of other broker requests in flight: a restart waits until it is 0.
        self.busy = busy
        self.runner = runner or self.run
        self.monotonic, self.wall_clock, self.sleep = monotonic, wall_clock, sleep
        self.tasks = set()

    async def dispatch(self, request_id, method, params, emit):
        if method == "update.plan":
            return self.plan(params)
        if method == "update.status":
            return self.status(params)
        if method == "update.stage":
            return self.stage(params)
        return await self.apply(params)

    def key(self):
        try:
            value = self.key_path.read_text().strip()
        except FileNotFoundError:
            return None
        return value.encode() if re.fullmatch(r"[0-9a-f]{64}", value) else None

    def plan(self, params):
        files = manifest(params.get("files"))
        payload = params.get("payload")
        require(isinstance(payload, dict) and len(json.dumps(payload)) <= 4096, "The payload record is invalid")
        changed = sorted({name for name, digest in files.items() if file_sha(self.opt / name) != digest}
                         | self.unfinished_files(self.unfinished(), files))
        result = {"changed": changed, **classify(changed), "reasons": []}
        try:
            saved = json.loads(self.record_path.read_text())
        except (OSError, ValueError):
            saved = None
        if saved != payload:
            result["reasons"].append("The provider versions or the provision script changed.")
        if self.key() is None:
            result["reasons"].append("The guest has no update key: it was provisioned before in-place updates.")
        if result["reboot"]:
            result["reasons"].append("These files need a reboot: " + ", ".join(result["reboot"][:20]))
        result["path"] = "reboot" if result["reasons"] else "in_place" if changed else "current"
        return result

    def unfinished(self):
        """The status of each operation in an UNFINISHED stage, by operation ID."""
        result = {}
        for path in sorted(self.updates.glob("*/status.json")) if self.updates.is_dir() else ():
            try:
                status = json.loads(path.read_text())
            except (OSError, ValueError):
                continue
            if isinstance(status, dict) and status.get("stage") in UNFINISHED:
                result[path.parent.name] = status
        return result

    @staticmethod
    def unfinished_files(unfinished, files):
        return {name for status in unfinished.values() for name in status.get("changed") or []
                if name in files}

    def operation_dir(self, operation):
        require(re.fullmatch(r"[A-Za-z0-9_-]{1,100}", identifier(operation)) is not None,
                "The operation ID is invalid")
        return self.updates / operation

    def stage(self, params):
        """Write one part of a file into the operation's private folder. Apply checks each file."""
        directory = self.operation_dir(params.get("operationId"))
        digest = sha(params.get("sha256"))
        offset = integer(params.get("offset"), 0, 0, MAX_FILE_BYTES)
        data = decode(params.get("data"), GUEST_UPDATE_PART_BYTES)
        require(offset + len(data) <= MAX_FILE_BYTES, "The update file is too large")
        private_dir(self.updates)
        private_dir(directory)
        part = directory / (digest + ".part")
        size = part.stat().st_size if part.exists() else 0
        require(offset <= size, "The update part leaves a gap")
        with part.open("r+b" if part.exists() else "wb") as stream:
            stream.seek(offset)
            stream.write(data)
            stream.truncate(offset + len(data))
            stream.flush()
            os.fsync(stream.fileno())
        return {"sha256": digest, "bytes": offset + len(data)}

    def status(self, params):
        try:
            return json.loads((self.operation_dir(params.get("operationId")) / "status.json").read_text())
        except FileNotFoundError:
            return {"stage": "unknown"}

    async def apply(self, params):
        operation = params.get("operationId")
        directory = self.operation_dir(operation)
        files = manifest(params.get("files"))
        key = self.key()
        if key is None:
            raise GuestError("update_unsupported", "The guest has no update key; use the reboot path")
        if not hmac.compare_digest(signature(key, operation, files), str(params.get("signature"))):
            raise GuestError("forbidden", "The update signature is invalid")
        writes = sorted(name for name, digest in files.items() if file_sha(self.opt / name) != digest)
        # An earlier attempt (this operation or another one) that wrote files but did not
        # finish its install or its restarts: those files count as changed, so this apply
        # finishes the same work.
        earlier = self.unfinished()
        changed = sorted(set(writes) | self.unfinished_files(earlier, files))
        plan = classify(changed)
        if plan["reboot"]:
            raise GuestError("reboot_required", "These files need a reboot: " + ", ".join(plan["reboot"][:20]))
        for name in writes:
            part = directory / (files[name] + ".part")
            require(file_sha(part) == files[name], "A staged update file is missing or differs: " + name)
        status = {"operationId": operation, "stage": "writing", "changed": changed,
                  "restarts": plan["restarts"], "at": time.time()}
        private_dir(self.updates)
        private_dir(directory)
        atomic_json(directory / "status.json", status)
        for other, previous in earlier.items():
            if other != operation:
                previous.update(stage="superseded", by=operation)
                atomic_json(self.updates / other / "status.json", previous)
        for name in writes:
            target = self.opt / name
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
            atomic_bytes(target, (directory / (files[name] + ".part")).read_bytes())
            os.chmod(target, 0o644)
        if plan["install"]:
            # The same install as provisioning: owners and modes, units, a layr rebuild when its
            # source changed. It restarts nothing: the restarts below wait for idle.
            code, output = await self.runner(["/bin/sh", str(self.opt / "vm/guest/install.sh")],
                                             {"STUDIO_DEFER_RESTARTS": "1"}, INSTALL_SECONDS)
            if code != 0:
                status.update(stage="install_failed", error=output[-2000:])
                atomic_json(directory / "status.json", status)
                raise GuestError("install_failed", "The guest install failed: " + output[-500:])
        status["stage"] = "restart_pending" if plan["restarts"] else "complete"
        atomic_json(directory / "status.json", status)
        if plan["restarts"]:
            task = asyncio.create_task(self.restart_when_idle(directory, status))
            self.tasks.add(task)
            task.add_done_callback(self.tasks.discard)
        return status

    async def restart_when_idle(self, directory, status):
        """Restart the changed units once no other broker request runs (or after DRAIN_SECONDS).
        A broker restart needs a separate systemd unit: it would stop this process first."""
        deadline = self.monotonic() + DRAIN_SECONDS
        while self.busy() > 0 and self.monotonic() < deadline:
            await self.sleep(1)
        units = status["restarts"]
        if ADMIN_UNIT in units or LAYR_UNIT in units:
            status.update(stage="restart_scheduled", scheduledAt=self.wall_clock())
            atomic_json(directory / "status.json", status)
            command = ["/usr/bin/systemd-run", "--collect", "--quiet", "--on-active=1",
                       "--unit", "codex-studio-update-" + status["operationId"][:40],
                       "/bin/systemctl", "restart", *units]
        else:
            command = ["/bin/systemctl", "restart", *units]
        code, output = await self.runner(command, {}, 120)
        if code != 0:
            status.update(stage="restart_failed", error=output[-2000:])
        elif status["stage"] != "restart_scheduled":
            status.update(stage="complete", restartedAt=self.wall_clock())
        atomic_json(directory / "status.json", status)

    def finish_restarts(self, started_at):
        """At broker start: a restart that this new process proves complete."""
        if not self.updates.is_dir():
            return
        for path in self.updates.glob("*/status.json"):
            try:
                status = json.loads(path.read_text())
            except (OSError, ValueError):
                continue
            if status.get("stage") == "restart_scheduled" and status.get("scheduledAt", started_at) < started_at:
                status.update(stage="complete", restartedAt=started_at)
                atomic_json(path, status)

    @staticmethod
    async def run(argv, env, timeout):
        process = await asyncio.create_subprocess_exec(
            *argv, env={**os.environ, **env}, stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT, start_new_session=True)
        try:
            output, _ = await asyncio.wait_for(process.communicate(), timeout)
        except asyncio.TimeoutError:
            process.kill()
            await process.wait()
            return 124, "timed out"
        return process.returncode, output.decode(errors="replace")
