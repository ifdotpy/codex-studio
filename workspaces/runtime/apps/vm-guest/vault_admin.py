"""Root broker methods of the project vault (docs/vault.md). The configuration, with the writer's
AWS key, stays in the private broker state; agents never read it."""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

from common import GuestError, atomic_json, require
from layr_share import project_id
from vault_s3 import S3
from vault_writer import Writer

READ_METHODS = {"vault.status"}
METHODS = READ_METHODS | {"vault.configure", "vault.push"}
LAYR = "/usr/local/bin/layr"


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
    return {"recipient": recipient, "bucket": bucket, "region": region, "prefix": prefix,
            "accessKeyId": key_id, "secretAccessKey": secret, "endpoint": endpoint}


def age_encrypt(recipient):
    def encrypt(data):
        process = subprocess.run(["age", "-r", recipient], input=data, capture_output=True, timeout=600)
        if process.returncode:
            raise GuestError("operation_failed", "age failed: " + process.stderr.decode(errors="replace")[:300])
        return process.stdout
    return encrypt


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

    def layr(self, project):
        environment = {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "HOME": "/root",
                       "LAYR_ROOT": str(self.root), "LAYR_SOCKET": "/run/layr/layr.sock"}

        def run(args):
            process = subprocess.run([LAYR, "--project", project, *args], capture_output=True, cwd="/",
                                     env=environment, timeout=1800)
            if process.returncode:
                raise GuestError("operation_failed", "layr failed: " + process.stderr.decode(errors="replace")[:300])
            return process.stdout.decode()
        return run

    async def dispatch(self, request_id, method, params, emit):
        project = project_id(params.get("projectId"))
        require((self.root / "projects" / project / "lines/main").is_dir(), "The layr project does not exist")
        if method == "vault.configure":
            config = check_config(params)
            self.folder(project).mkdir(mode=0o700, exist_ok=True)
            path = self.folder(project) / "config.json"
            old = json.loads(path.read_text()) if path.exists() else {}
            if any(old.get(k) != config[k] for k in ("bucket", "prefix", "recipient", "endpoint")):
                # Another vault: its first push is full; deltas never cross vaults or keys.
                shutil.rmtree(self.folder(project) / "writer", ignore_errors=True)
            atomic_json(self.folder(project) / "config.json", config)
            os.chmod(self.folder(project) / "config.json", 0o600)
            return {"configured": True, "bucket": config["bucket"], "prefix": config["prefix"]}
        if method == "vault.status":
            config = self.config(project)
            record = self.folder(project) / "writer" / "writer.json"
            return {"bucket": config["bucket"], "region": config["region"], "prefix": config["prefix"],
                    "recipient": config["recipient"],
                    "last": json.loads(record.read_text()) if record.exists() else None}
        lock = self.locks.setdefault(project, asyncio.Lock())
        if lock.locked():
            raise GuestError("busy", "A vault push of this project is running")
        async with lock:
            return await asyncio.to_thread(self.push, project)

    def push(self, project):
        config = self.config(project)
        s3 = S3(config["bucket"], config["region"], config["accessKeyId"], config["secretAccessKey"],
                endpoint=config.get("endpoint"))
        layr = self.layr(project)
        head = layr(["rev-parse", "main"]).strip()
        writer = Writer(self.folder(project) / "writer", config["prefix"], layr, age_encrypt(config["recipient"]), s3)
        return writer.push(head)
