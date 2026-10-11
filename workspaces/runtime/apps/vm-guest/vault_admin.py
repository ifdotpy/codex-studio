"""Root broker methods of the project vault (docs/vault.md). The configuration, with the writer's
AWS key, stays in the private broker state; agents never read it. `layr-desk push` is the writer."""
from __future__ import annotations

import asyncio
import base64
import binascii
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

from common import GuestError, atomic_json, require
from layr_share import project_id

READ_METHODS = {"vault.status"}
METHODS = READ_METHODS | {"vault.configure", "vault.push"}
LAYR = "/usr/local/bin/layr"
LAYR_DESK = "/usr/local/bin/layr-desk"
LAYR_SOCKET = "/run/layr/layr.sock"
# A first push of a large project on a slow line takes hours; a stuck one must still end.
PUSH_TIMEOUT = 6 * 3600
# A new value of one of these starts the writer again with a full push: deltas never cross
# vaults or keys. A new writer access key keeps the chain.
VAULT_FIELDS = ("bucket", "prefix", "recipient", "endpoint", "signingKey")


def check_config(params):
    recipient, bucket, region = params.get("recipient"), params.get("bucket"), params.get("region")
    require(isinstance(recipient, str) and re.fullmatch(r"age1[0-9a-z]{58}", recipient), "The vault recipient is invalid")
    require(isinstance(bucket, str) and re.fullmatch(r"[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]", bucket), "The bucket name is invalid")
    require(isinstance(region, str) and re.fullmatch(r"[a-z]{2}(-[a-z]+)+-\d", region), "The region is invalid")
    prefix = params.get("prefix")
    require(isinstance(prefix, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,99}", prefix), "The prefix is invalid")
    key_id, secret = params.get("accessKeyId"), params.get("secretAccessKey")
    require(isinstance(key_id, str) and re.fullmatch(r"[A-Z0-9]{16,128}", key_id), "The access key ID is invalid")
    require(isinstance(secret, str) and 16 <= len(secret) <= 128 and "\n" not in secret, "The secret access key is invalid")
    endpoint = params.get("endpoint")
    require(endpoint is None or (isinstance(endpoint, str) and endpoint.startswith(("http://127.0.0.1:", "http://localhost:"))),
            "Only a local test endpoint may replace AWS")
    config = {"recipient": recipient, "bucket": bucket, "region": region, "prefix": prefix,
              "accessKeyId": key_id, "secretAccessKey": secret, "endpoint": endpoint}
    # The optional key that signs each state: the base64 seed of an Ed25519 key.
    signing = params.get("signingKey")
    if signing is not None:
        try:
            seed = base64.b64decode(signing, validate=True) if isinstance(signing, str) else b""
        except binascii.Error:
            seed = b""
        require(len(seed) == 32, "The signing key is invalid")
        config["signingKey"] = signing
    return config


def failure(stderr):
    """The cause that layr-desk names, on one line: S3 replies and layr messages hold no secret."""
    text = " ".join(stderr.decode(errors="replace").split())
    return text.removeprefix("Error: ")[:400]


class VaultHandlers:
    def __init__(self, state, root):
        self.state, self.root = Path(state) / "vault", Path(root)
        self.state.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.locks = {}

    def folder(self, project):
        return self.state / project_id(project)

    def config(self, project):
        path = self.folder(project) / "config.json"
        if not path.exists():
            raise GuestError("not_found", "The project has no vault")
        return json.loads(path.read_text())

    async def dispatch(self, request_id, method, params, emit):
        project = project_id(params.get("projectId"))
        require((self.root / "projects" / project / "lines/main").is_dir(), "The layr project does not exist")
        if method == "vault.configure":
            config = check_config(params)
            # Wait for a push that runs with the old configuration: a new signing key must not
            # meet an unsigned push that lands after the configuration changed.
            async with self.locks.setdefault(project, asyncio.Lock()):
                return self.configure(project, config)
        if method == "vault.status":
            config = self.config(project)
            record = self.folder(project) / "writer" / "writer.json"
            status = self.folder(project) / "status.json"
            return {"bucket": config["bucket"], "region": config["region"], "prefix": config["prefix"],
                    "recipient": config["recipient"],
                    "last": json.loads(record.read_text()) if record.exists() else None,
                    **(json.loads(status.read_text()) if status.exists() else {})}
        return await self.run_push(project)

    def configure(self, project, config):
        """Store the writer configuration; the caller holds the project's push lock."""
        self.folder(project).mkdir(mode=0o700, exist_ok=True)
        path = self.folder(project) / "config.json"
        old = json.loads(path.read_text()) if path.exists() else {}
        if any(old.get(k) != config.get(k) for k in VAULT_FIELDS):
            # Another vault or signing key: its first push is full.
            shutil.rmtree(self.folder(project) / "writer", ignore_errors=True)
        atomic_json(path, config)
        os.chmod(path, 0o600)
        return {"configured": True, "bucket": config["bucket"], "prefix": config["prefix"]}

    async def run_push(self, project):
        """One push under the project lock; the outcome goes to status.json."""
        lock = self.locks.setdefault(project, asyncio.Lock())
        if lock.locked():
            raise GuestError("busy", "A vault push of this project is running")
        async with lock:
            status_path = self.folder(project) / "status.json"
            status = json.loads(status_path.read_text()) if status_path.exists() else {}
            status["lastAttempt"] = int(time.time())
            try:
                result = await asyncio.to_thread(self.push, project)
            except Exception as error:
                # The broker hides unexpected errors; a failed push names its cause.
                message = str(error) if isinstance(error, GuestError) else f"{type(error).__name__}: {str(error)[:400]}"
                status["lastError"] = message
                atomic_json(status_path, status)
                if isinstance(error, GuestError):
                    raise
                raise GuestError("operation_failed", "vault push: " + message) from error
            status["lastError"] = None
            if result.get("pushed"):
                status["lastPush"] = status["lastAttempt"]
            atomic_json(status_path, status)
            return result

    async def tick(self):
        """Push every configured project once; one failure does not stop the others."""
        for config in sorted(self.state.glob("*/config.json")):
            try:
                await self.run_push(config.parent.name)
            except GuestError:
                pass

    async def periodic(self, interval=600, first=60):
        """The vault keeps the latest state: push every `interval` seconds, from boot."""
        await asyncio.sleep(first)
        while True:
            await self.tick()
            await asyncio.sleep(interval)

    def push(self, project):
        """`layr-desk push` of the project's main head, as root. The configuration goes on its
        standard input, so the AWS key is in no argument list and in no other file."""
        config = {key: value for key, value in self.config(project).items() if value is not None}
        environment = {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": "/root",
                       "LAYR_ROOT": str(self.root), "LAYR_SOCKET": LAYR_SOCKET}
        command = [LAYR_DESK, "push", "--project", project, "--state-dir", str(self.folder(project) / "writer"),
                   "--layr", LAYR, "--config", "-"]
        try:
            process = subprocess.run(command, input=json.dumps(config).encode(), capture_output=True, cwd="/",
                                     env=environment, timeout=PUSH_TIMEOUT)
        except FileNotFoundError as error:
            raise GuestError("operation_failed", "layr-desk is not installed") from error
        except subprocess.TimeoutExpired as error:
            raise GuestError("operation_failed", f"layr-desk push ran longer than {PUSH_TIMEOUT} seconds") from error
        if process.returncode:
            cause = failure(process.stderr) or f"exit status {process.returncode}"
            raise GuestError("operation_failed", "layr-desk push: " + cause)
        lines = process.stdout.decode(errors="replace").strip().splitlines()
        try:
            summary = json.loads(lines[-1])
        except (IndexError, ValueError):
            summary = None
        if not isinstance(summary, dict) or not isinstance(summary.get("pushed"), bool):
            raise GuestError("operation_failed", "layr-desk push printed no summary")
        return summary
