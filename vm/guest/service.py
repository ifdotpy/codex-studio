"""Guest JSON-lines service. No host or account state belongs in this process."""
from __future__ import annotations

import argparse
from contextlib import closing
import asyncio
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import uuid

from common import (GuestError, MAX_LINE, PORT, PROTOCOL, atomic_bytes,
                    atomic_json, bounded_lock, connect_db, decode, digest, identifier, integer,
                    number, private_dir, process_identity, receipt, require, save_receipt)
from native import Native
from upload import Uploads, sha, tree_space

READ_METHODS = {"health", "workspace.status", "provider.attach", "provider.list", "provider.rpc",
                "upload.begin", "upload.chunk", "upload.commit", "file.stat", "file.read"}
METHODS = READ_METHODS | {"exec", "sync.push", "workspace.startBase", "workspace.create",
                          "workspace.archive", "workspace.remove", "provider.start", "provider.write",
                          "provider.stop", "credentials.put"}
CLIENT_IDLE_SECONDS = 60


class Service:
    def __init__(self, state: Path, store: Path, projects: Path, home: Path):
        self.state, self.store, self.projects, self.home = map(Path.resolve, (state, store, projects, home))
        for path in (self.state, self.store, self.projects):
            private_dir(path)
        self.lease = (self.state / "service.lock").open("a+")
        try:
            fcntl.flock(self.lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as exc:
            raise RuntimeError("Another guest service owns this state directory") from exc
        self.providers = self.state / "providers"
        private_dir(self.providers)
        self.db = connect_db(self.state / "receipts.sqlite3")
        self.uploads = Uploads(self.state, self.projects, self.db)
        self.native = Native(self.state)
        self.active = {}
        self.operations = set()
        self.root_locks = {}
        self.agent_locks = {}
        self.clients = 0
        self.processes = set()
        self.environment = {key: value for key, value in os.environ.items()
                            if key in {"PATH", "LANG", "TZ", "TERM", "USER", "LOGNAME", "SHELL",
                                       "SSL_CERT_FILE", "SSL_CERT_DIR"} or key.startswith("LC_")
                            or key.startswith("CODEX_WORKSPACE_")}
        self.environment.update(HOME=str(self.home), CODEX_WORKSPACE_STORE=str(self.store))

    def close(self):
        self.db.close()
        self.lease.close()

    def path(self, value, *, projects_only=False):
        require(isinstance(value, str) and Path(value).is_absolute(), "The path must be absolute")
        path = Path(value).resolve()
        roots = (self.projects,) if projects_only else (self.projects, self.store, self.home)
        require(any(path.is_relative_to(root) for root in roots), "The path is outside the permitted roots")
        return path

    def directory(self, handle):
        require(isinstance(handle, str) and 0 < len(handle) <= 180, "The provider handle must be a bounded string")
        candidates = [self.providers / str(uuid.uuid5(uuid.NAMESPACE_URL, "native:" + handle))]
        try:
            if str(uuid.UUID(handle)) == handle:
                candidates.append(self.providers / handle)
        except ValueError:
            pass
        for directory in candidates:
            if directory.is_dir():
                if (directory / "native.json").exists():
                    require(json.loads((directory / "native.json").read_text())["handle"] == handle,
                            "The provider handle differs from its saved association")
                return directory
        raise GuestError("not_found", "The provider handle does not exist")

    def status(self, handle):
        directory = self.directory(handle)
        native = directory / "native.json"
        if native.exists():
            return {**self.native.info(handle), **json.loads(native.read_text()), "transport": "native"}
        try:
            meta = json.loads((directory / "status.json").read_text())
        except FileNotFoundError as exc:
            raise GuestError("outcome_unknown", "The provider startup has no proven result") from exc
        if meta["state"] in {"starting", "running"} and process_identity(meta["supervisorPid"]) != meta["supervisorStart"]:
            meta.update(state="lost", reason="supervisor_lost")
        database = directory / "journal.sqlite3"
        if database.exists():
            with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
                meta["lastSeq"] = db.execute("SELECT coalesce(max(seq),0) FROM events").fetchone()[0]
        return {**meta, "transport": "stdio"}

    def list_providers(self):
        result = []
        for directory in self.providers.iterdir():
            if directory.is_dir():
                try:
                    handle = json.loads((directory / "native.json").read_text())["handle"] if (directory / "native.json").exists() else directory.name
                    result.append(self.status(handle))
                except GuestError as exc:
                    association = directory / "association.json"
                    saved = json.loads(association.read_text()) if association.exists() else {"handle": directory.name}
                    result.append({**saved, "state": "lost", "reason": exc.code})
        return result

    async def worker(self, script, params, timeout, *, arguments=()):
        process = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("deadline.py")), str(timeout),
            sys.executable, str(Path(__file__).with_name(script)), *arguments,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            env=self.environment, start_new_session=True)
        try:
            stdout, _ = await asyncio.wait_for(process.communicate(json.dumps(params).encode()), timeout + 6)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            await asyncio.wait_for(process.wait(), 5)
            raise GuestError("timeout", "The guest operation exceeded its deadline")
        if process.returncode == 124:
            raise GuestError("timeout", "The guest operation exceeded its deadline")
        if process.returncode == 125:
            raise GuestError("output_limit", "The guest helper exceeded its output limit")
        return process.returncode, stdout

    async def workspace(self, method, params):
        if method in {"workspace.archive", "workspace.remove", "workspace.create"}:
            async with bounded_lock(self.agent_locks, params.get("agentId", "missing")):
                return await self.workspace_guarded(method, params)
        return await self.workspace_guarded(method, params)

    async def workspace_guarded(self, method, params):
        values = dict(params)
        if "root" in values:
            values["root"] = str(self.path(values["root"], projects_only=True))
        if "agentId" in values:
            agent = identifier(values["agentId"])
            require(all(char.isascii() and (char.isalnum() or char in "-_") for char in agent),
                    "agentId must contain ASCII letters, digits, hyphens, or underscores")
        if method in {"workspace.startBase", "workspace.create"}:
            require("root" in values, "The workspace root is required")
            timeout = number(values.get("timeoutSeconds"), 1800, 1, 1800)
            values["timeoutSeconds"] = timeout
        else:
            timeout = 15 if method == "workspace.status" else 120
        if method in {"workspace.create", "workspace.archive", "workspace.remove"}:
            require("agentId" in values, "The workspace agentId is required")
        if method in {"workspace.archive", "workspace.remove"}:
            require(not any(row.get("agentId") == values["agentId"] and row["state"] in {"starting", "running", "lost"}
                            for row in self.list_providers()), "A provider still uses this workspace")
        root = values.get("root", "agent:" + str(values.get("agentId", "status")))
        async with bounded_lock(self.root_locks, root):
            code, output = await self.worker("workspace.py", {"method": method, "params": values}, timeout)
        marker = b"GUEST_RESULT:"
        if marker not in output:
            raise GuestError("internal", "The workspace engine returned no result")
        result = json.loads(output.rsplit(marker, 1)[1])
        if code or "error" in result:
            raise GuestError("internal", "The workspace engine failed: " + str(result.get("error", "unknown error"))[:1000])
        return result["result"]

    async def launch_config(self, params):
        argv = params.get("argv")
        require(isinstance(argv, list) and 0 < len(argv) <= 1024
                and all(isinstance(value, str) and "\0" not in value for value in argv), "argv must contain nonempty executable arguments")
        require(bool(argv[0]), "The executable must not be empty")
        cwd = self.path(params.get("cwd"))
        env = params.get("env", {})
        require(isinstance(env, dict) and len(env) <= 1024 and all(
            isinstance(key, str) and isinstance(value, str) and key and "=" not in key and "\0" not in key + value
            for key, value in env.items()), "env must contain string names and values")
        require("HOME" not in env or env["HOME"] == str(self.home), "HOME must identify the service user's home")
        environment = {**self.environment, **env}
        agent = params.get("agentId")
        if agent:
            status = await self.workspace("workspace.status", {"agentId": agent})
            require(len(status["workspaces"]) == 1, "The workspace agentId does not exist")
            workspace_path = Path(status["workspaces"][0].get("path", ""))
            require(cwd.is_relative_to(workspace_path), "The command cwd is outside its workspace")

        else:
            require(cwd.is_dir(), "The command cwd does not exist")
        if agent:
            # The stable wrapper enters the current namespace only on a new
            # launch. A reattach preserves the original native launch signature.
            argv = [sys.executable, str(Path(__file__).with_name("namespace_exec.py")), agent, str(cwd), *argv]
            cwd = self.home
        return {"argv": argv, "cwd": str(cwd), "env": environment, "agentId": agent}

    async def start(self, request_id, params, *, execute=False):
        async with bounded_lock(self.agent_locks, params.get("agentId", "request:" + request_id)):
            return await self.start_guarded(request_id, params, execute=execute)

    async def start_guarded(self, request_id, params, *, execute=False):
        config = await self.launch_config(params)
        native = not execute and params.get("transport", "stdio") == "native"
        handle = params.get("handle") if native else None
        if handle is not None:
            require(isinstance(handle, str) and 0 < len(handle) <= 180, "The provider handle must be a bounded string")
        handle = handle or str(uuid.uuid5(uuid.NAMESPACE_URL, "studio-guest:" + request_id))
        directory = self.providers / (str(uuid.uuid5(uuid.NAMESPACE_URL, "native:" + handle)) if native else handle)
        if directory.exists() and not native:
            raise GuestError("outcome_unknown", "The provider launch directory already exists")
        sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
        import codex_workspace_images as images
        floor = images._minimum_free_bytes(images._AGENT_MIN_FREE_ENV, images._DEFAULT_AGENT_MIN_FREE_BYTES)
        if shutil.disk_usage(self.state).free < floor + 80 * 1024**2:
            raise GuestError("busy", "The data disk has insufficient free space for the provider journal")
        private_dir(directory)
        atomic_json(directory / "association.json", {"handle": handle, "agentId": config.get("agentId"), "transport": "native" if native else "stdio"})
        if native:
            return await self.native.start(handle, config, directory)
        require(params.get("transport", "stdio") == "stdio", "The provider transport is not supported")
        if execute:
            config.update(timeoutSeconds=number(params.get("timeoutSeconds"), 300, 0.01, 3600),
                          stdin=params.get("stdin", ""))
            decode(config["stdin"])
        config["outputLimitBytes"] = integer(params.get("outputLimitBytes"), 64 * 1024**2, 1, 64 * 1024**2)
        atomic_json(directory / "launch.json", config)
        process = subprocess.Popen([sys.executable, str(Path(__file__).with_name("supervisor.py")), str(directory)],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True, close_fds=True)
        self.processes.add(process)
        asyncio.create_task(self.reap(process))
        deadline = asyncio.get_running_loop().time() + 8
        while asyncio.get_running_loop().time() < deadline:
            if (directory / "status.json").exists():
                status = self.status(handle)
                if status["state"] == "exited" or (status["state"] == "running" and (directory / "control.sock").exists()):
                    return status
            if process.poll() is not None:
                raise GuestError("internal", "The process supervisor failed to start")
            await asyncio.sleep(0.025)
        raise GuestError("outcome_unknown", "The process startup has no proven result")

    async def reap(self, process):
        while process.poll() is None:
            await asyncio.sleep(0.1)
        self.processes.discard(process)

    async def control(self, request_id, handle, method, params):
        directory = self.directory(handle)
        status = self.status(handle)
        if status.get("transport") == "native":
            require(method == "stop", "Use provider.rpc for native provider input")
            return await self.native.stop(handle)
        if status["state"] == "exited":
            if method == "stop":
                return status
            # The detached supervisor can exit after it commits a write receipt.
            with closing(sqlite3.connect((directory / "journal.sqlite3").as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
                row = db.execute("SELECT digest,response FROM receipts WHERE id=?", (request_id,)).fetchone()
            if row and row[0] == digest(method, params) and row[1]:
                response = json.loads(row[1])
                if "result" in response:
                    return response["result"]
                raise GuestError(response["error"]["code"], response["error"]["message"])
            raise GuestError("not_found", "The provider process has exited")
        try:
            reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(str(directory / "control.sock"), limit=MAX_LINE + 1), 2)
            try:
                writer.write(json.dumps({"id": request_id, "method": method, "params": params}).encode() + b"\n")
                await asyncio.wait_for(writer.drain(), 2)
                response = json.loads(await asyncio.wait_for(reader.readline(), 8))
            finally:
                writer.close()
        except (OSError, asyncio.TimeoutError, ValueError) as exc:
            raise GuestError("outcome_unknown", "The provider operation has no proven result") from exc
        if "error" in response:
            raise GuestError(response["error"]["code"], response["error"]["message"])
        return response["result"]

    async def attach(self, params, emit):
        handle = params.get("handle")
        directory = self.directory(handle)
        require(self.status(handle).get("transport") == "stdio", "Use provider.rpc for native provider events")
        cursor = integer(params.get("afterSeq"), 0, 0, 2**63 - 1)
        count = integer(params.get("maxEvents"), 256, 1, 1000)
        wait = integer(params.get("waitMs"), 1000, 0, 30000) / 1000
        deadline = asyncio.get_running_loop().time() + wait
        sent = 0
        while True:
            status = self.status(handle)
            require(cursor <= status["lastSeq"], "The event cursor is ahead of the journal")
            with closing(sqlite3.connect((directory / "journal.sqlite3").as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
                rows = db.execute("SELECT seq,event,data FROM events WHERE seq>? ORDER BY seq LIMIT ?",
                                  (cursor, min(count - sent, 16))).fetchall()
            for sequence, event, data in rows:
                await emit(event, json.loads(data))
                cursor = sequence
                sent += 1
            if sent >= count or status["state"] in {"exited", "lost"} or asyncio.get_running_loop().time() >= deadline:
                return {"handle": handle, "state": status["state"], "nextSeq": cursor,
                        "hasMore": cursor < self.status(handle)["lastSeq"]}
            if not rows:
                await asyncio.sleep(min(0.05, max(0, deadline - asyncio.get_running_loop().time())))

    async def execute(self, request_id, params, emit):
        status = await self.start(request_id, params, execute=True)
        handle = status["handle"]
        cursor = 0
        while status["state"] not in {"exited", "lost"}:
            result = await self.attach({"handle": handle, "afterSeq": cursor, "waitMs": 1000}, emit)
            cursor = result["nextSeq"]
            status = self.status(handle)
        # Drain the final journal even if the child exited during startup.
        while cursor < status["lastSeq"]:
            result = await self.attach({"handle": handle, "afterSeq": cursor, "waitMs": 0}, emit)
            cursor = result["nextSeq"]
        return {key: status.get(key) for key in ("handle", "exitCode", "reason", "lastSeq")}

    async def apply_upload(self, identity, root, archive, expected):
        async with bounded_lock(self.root_locks, str(root)):
            staging = archive.parent / "expanded"
            code, output = await self.worker("upload.py", {}, 1800, arguments=(str(archive), str(staging), expected))
            result = json.loads(output)
            if code or "error" in result:
                if "error" in result:
                    raise GuestError(result["error"]["code"], result["error"]["message"])
                raise GuestError("invalid_params", "The upload extraction failed")
            require(root.parent.resolve() == self.projects and not root.is_symlink(), "The upload root is outside projects")
            try:
                # Rsync needs another complete copy after the archive has expanded.
                tree_space(self.projects, result["bytes"])
            except GuestError:
                await self.remove_temporary(staging)
                raise
            root.mkdir(mode=0o700, exist_ok=True)
            # This durable marker precedes the first change to the project tree.
            self.db.execute("UPDATE uploads SET state='applying' WHERE id=?", (identity,))
            self.db.commit()
            row = self.uploads.row(identity)
            mode, deletes = row[7], json.loads(row[8])
            if deletes:
                code, _ = await self.worker("upload_delete.py", {"root": str(root), "deletePaths": deletes}, 120)
                if code:
                    raise GuestError("outcome_unknown", "The delta deletion failed after application started")
            try:
                code, output = await self.worker("upload_apply.py", {"root": str(root), "staging": str(staging),
                                                "bytes": result["bytes"], "mode": mode}, 1800)
                applied = json.loads(output)
                if not isinstance(applied, dict):
                    raise ValueError("The worker response is not an object")
            except (GuestError, ValueError, TypeError) as exc:
                raise GuestError("outcome_unknown", "The project sync worker has no proven result; inspect the project root") from exc
            if "error" in applied:
                raise GuestError(applied["error"]["code"], applied["error"]["message"])
            if code or applied.get("applied") is not True:
                raise GuestError("outcome_unknown", "The project sync failed after application started")
            result = {"uploadId": identity, "root": str(root), "sha256": expected,
                      "bytes": result["bytes"], "state": "applied"}
            self.db.execute("UPDATE uploads SET state='applied',result=? WHERE id=?", (json.dumps(result), identity))
            self.db.commit()
            # The journal is the durable receipt. Temporary bytes are no longer needed.
            await self.remove_temporary(archive.parent)
            return result

    async def remove_temporary(self, path):
        process = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("deadline.py")), "60", "rm", "-rf", "--", str(path),
            stdin=asyncio.subprocess.DEVNULL,
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
        try:
            await asyncio.wait_for(process.wait(), 66)
        except asyncio.TimeoutError:
            process.kill()
            await asyncio.wait_for(process.wait(), 5)

    async def file(self, method, params):
        path = self.path(params.get("path"))
        root = next(root for root in (self.projects, self.store, self.home) if path.is_relative_to(root))
        values = {**params, "method": method, "path": str(path), "allowedRoot": str(root)}
        timeout = 300 if method == "file.stat" else 15
        if params.get("agentId"):
            status = await self.workspace("workspace.status", {"agentId": params["agentId"]})
            require(len(status["workspaces"]) == 1, "The workspace does not exist")
            root = Path(status["workspaces"][0]["path"])
            require(path.is_relative_to(root), "The file path is outside its workspace")
            values["allowedRoot"] = str(root)
            # The namespace wrapper executes a maintained file reader, not a shell.
            config = await self.launch_config({"argv": [sys.executable, str(Path(__file__).with_name("files.py"))],
                                               "cwd": str(root), "agentId": params["agentId"]})
            process = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("deadline.py")),
                str(timeout), *config["argv"], cwd=config["cwd"], env=config["env"],
                stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
            try:
                output, _ = await asyncio.wait_for(process.communicate(json.dumps(values).encode()), timeout + 6)
            except asyncio.TimeoutError as exc:
                process.kill()
                await asyncio.wait_for(process.wait(), 5)
                raise GuestError("timeout", "The file read exceeded its deadline") from exc
            if process.returncode in {124, 125}:
                raise GuestError("timeout", "The file reader exceeded its deadline or size limit")
        else:
            _, output = await self.worker("files.py", values, timeout)
        result = json.loads(output)
        if "error" in result:
            raise GuestError(result["error"]["code"], result["error"]["message"])
        return result["result"]

    @staticmethod
    def memory():
        try:
            values = {line.split(":", 1)[0]: int(line.split()[1]) * 1024
                      for line in Path("/proc/meminfo").read_text().splitlines()
                      if line.startswith(("MemTotal:", "MemAvailable:"))}
            return {"totalBytes": values["MemTotal"], "availableBytes": values["MemAvailable"]}
        except (OSError, ValueError, KeyError):
            return {"totalBytes": None, "availableBytes": None}

    async def dispatch(self, request_id, method, params, emit):
        if method == "health":
            process = await asyncio.create_subprocess_exec("stat", "-f", "-c", "%T", str(self.store), stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.DEVNULL)
            try:
                output, _ = await asyncio.wait_for(process.communicate(), 5)
            except asyncio.TimeoutError:
                process.kill()
                await asyncio.wait_for(process.wait(), 5)
                output = b"unknown"
            return {"protocol": PROTOCOL, "uid": os.getuid(), "home": str(self.home), "store": str(self.store),
                    "projects": str(self.projects), "state": str(self.state), "filesystem": output.decode().strip(),
                    "providers": len(self.list_providers()),
                    "disk": {"freeBytes": shutil.disk_usage(self.store).free, "totalBytes": shutil.disk_usage(self.store).total},
                    "memory": self.memory()}
        if method.startswith("file."):
            return await self.file(method, params)
        if method.startswith("workspace."):
            return await self.workspace(method, params)
        if method.startswith("upload."):
            return await self.uploads.run(method, params, self.apply_upload)
        if method == "sync.push":
            path = self.path(params.get("path"), projects_only=True)
            require(path.parent == self.projects and not path.exists(), "sync.push requires a new project root")
            data = decode(params.get("data"))
            expected = sha(params.get("sha256"))
            require(hashlib.sha256(data).hexdigest() == expected, "The archive checksum differs")
            identity = uuid.uuid5(uuid.NAMESPACE_URL, request_id).hex
            await self.uploads.run("upload.begin", {"uploadId": identity, "root": str(path), "totalBytes": len(data), "sha256": expected}, self.apply_upload)
            await self.uploads.run("upload.chunk", {"uploadId": identity, "seq": 0, "data": params["data"]}, self.apply_upload)
            result = await self.uploads.run("upload.commit", {"uploadId": identity}, self.apply_upload)
            return {"path": str(path), "sha256": expected, "bytes": result["bytes"]}
        if method == "provider.start":
            return await self.start(request_id, params)
        if method == "provider.list":
            return {"providers": self.list_providers()}
        if method == "provider.attach":
            return await self.attach(params, emit)
        if method == "provider.rpc":
            require(self.status(params.get("handle")).get("transport") == "native", "The provider transport is not native")
            return await self.native.rpc(params)
        if method == "provider.write":
            data = decode(params.get("data", ""))
            require(isinstance(params.get("close", False), bool), "close must be a boolean")
            return await self.control(request_id, params.get("handle"), "write", {"data": params.get("data", ""), "close": params.get("close", False)})
        if method == "provider.stop":
            return await self.control(request_id, params.get("handle"), "stop", {})
        if method == "exec":
            return await self.execute(request_id, params, emit)
        if method == "credentials.put":
            files = params.get("files")
            require(isinstance(files, list) and 0 < len(files) <= 16, "files must contain 1 to 16 entries")
            prepared = []
            for item in files:
                require(isinstance(item, dict), "Each credential entry must be an object")
                relative = item.get("path")
                require(isinstance(relative, str) and relative and not Path(relative).is_absolute()
                        and ".." not in Path(relative).parts, "The credential path must be relative to home")
                path = self.home / relative
                require(path != self.home, "The credential path must identify a file")
                for parent in (path, *path.parents):
                    if parent == self.home:
                        break
                    require(not parent.is_symlink(), "A credential path must not contain symlinks")
                data = decode(item.get("data"))
                prepared.append((path, data))
            for path, data in prepared:
                path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
                atomic_bytes(path, data)
            return {"files": [{"path": str(path.relative_to(self.home)), "bytes": len(data)} for path, data in prepared]}
        raise GuestError("invalid_request", "The method is not supported")

    async def replay_exec(self, result, emit):
        cursor = 0
        while cursor < result["lastSeq"]:
            replay = await self.attach({"handle": result["handle"], "afterSeq": cursor, "waitMs": 0}, emit)
            cursor = replay["nextSeq"]

    async def request(self, request, emit):
        request_id = request.get("id") if isinstance(request, dict) else None
        try:
            identifier(request_id)
            method, params = request.get("method"), request.get("params", {})
            require(isinstance(method, str) and method in METHODS, "The method is not supported")
            require(isinstance(params, dict), "The parameters must be an object")
            if method in READ_METHODS:
                return {"id": request_id, "result": await self.dispatch(request_id, method, params, emit)}
            request_digest = digest(method, params)
            if request_id in self.active:
                saved_digest, task = self.active[request_id]
                if saved_digest != request_digest:
                    raise GuestError("id_conflict", "The request ID has different content")
                response = await asyncio.shield(task)
                if method == "exec" and "result" in response:
                    await self.replay_exec(response["result"], emit)
                return response
            prior = receipt(self.db, request_id, request_digest)
            if prior is not None:
                if method == "exec" and "result" in prior:
                    await self.replay_exec(prior["result"], emit)
                return prior

            async def operation():
                try:
                    result = await self.dispatch(request_id, method, params, emit)
                    response = {"id": request_id, "result": result}
                except GuestError as exc:
                    response = {"id": request_id, "error": exc.object()}
                except Exception:
                    response = {"id": request_id, "error": {"code": "internal", "message": "The guest operation failed"}}
                save_receipt(self.db, request_id, response)
                return response

            task = asyncio.create_task(operation())
            self.active[request_id] = (request_digest, task)
            try:
                return await asyncio.shield(task)
            finally:
                self.active.pop(request_id, None)
        except GuestError as exc:
            return {"id": request_id, "error": exc.object()}
        except (ValueError, TypeError, KeyError):
            return {"id": request_id, "error": {"code": "invalid_params", "message": "The parameters are invalid"}}
        except Exception:
            return {"id": request_id, "error": {"code": "internal", "message": "The guest request failed"}}

    async def client(self, reader, writer):
        if self.clients >= 32:
            writer.close()
            return
        peer = writer.get_extra_info("peername")
        if writer.get_extra_info("socket").family == getattr(socket, "AF_VSOCK", -1) and peer[0] != socket.VMADDR_CID_HOST:
            writer.close()
            return
        self.clients += 1
        output_lock = asyncio.Lock()
        connection_tasks = set()
        read_task = None

        async def send(value):
            try:
                async with output_lock:
                    if writer.is_closing():
                        return
                    line = json.dumps(value, separators=(",", ":"), allow_nan=False).encode() + b"\n"
                    if len(line) > MAX_LINE:
                        value = {"id": value.get("id"), "error": {"code": "output_limit", "message": "The response exceeds the line limit"}}
                        line = json.dumps(value).encode() + b"\n"
                    writer.write(line)
                    await asyncio.wait_for(writer.drain(), 5)
            except (OSError, asyncio.TimeoutError):
                writer.close()

        async def handle(request):
            request_id = request.get("id") if isinstance(request, dict) else None

            async def emit(event, data):
                await send({"id": request_id, "event": event, "data": data})

            response = await self.request(request, emit)
            await send(response)

        try:
            while not writer.is_closing():
                if read_task is None:
                    read_task = asyncio.create_task(reader.readline())
                # Start the idle deadline only after every operation has sent its result.
                done, _ = await asyncio.wait({read_task, *connection_tasks},
                    timeout=None if connection_tasks else CLIENT_IDLE_SECONDS,
                    return_when=asyncio.FIRST_COMPLETED)
                if not done:
                    break
                if read_task not in done:
                    continue
                try:
                    line = read_task.result()
                except (ValueError, asyncio.LimitOverrunError):
                    break
                read_task = None
                if not line:
                    break
                if len(line) > MAX_LINE or not line.endswith(b"\n"):
                    break
                try:
                    request = json.loads(line, parse_constant=lambda value: (_ for _ in ()).throw(ValueError(value)))
                except (ValueError, UnicodeError):
                    await send({"id": None, "error": {"code": "invalid_request", "message": "The request is not valid JSON"}})
                    continue
                if len(self.operations) >= 16:
                    await send({"id": request.get("id") if isinstance(request, dict) else None,
                                "error": {"code": "busy", "message": "The guest operation limit is reached"}})
                    continue
                task = asyncio.create_task(handle(request))
                self.operations.add(task)
                connection_tasks.add(task)
                task.add_done_callback(self.operations.discard)
                task.add_done_callback(connection_tasks.discard)
        finally:
            self.clients -= 1
            if read_task is not None:
                read_task.cancel()
                await asyncio.gather(read_task, return_exceptions=True)
            # Do not cancel operations. They finish and save their receipts after disconnect.
            writer.close()


async def serve(args):
    if os.getuid() == 0:
        raise RuntimeError("The guest service must run as a non-root user")
    os.umask(0o077)
    service = Service(args.state, args.store, args.projects, Path.home())
    if args.unix_socket:
        # Explicit test transport. Production always uses vsock.
        args.unix_socket.unlink(missing_ok=True)
        server = await asyncio.start_unix_server(service.client, path=str(args.unix_socket), limit=MAX_LINE + 1)
        os.chmod(args.unix_socket, 0o600)
    else:
        listener = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        listener.setblocking(False)
        listener.bind((socket.VMADDR_CID_ANY, PORT))
        listener.listen(32)
        server = await asyncio.start_server(service.client, sock=listener, limit=MAX_LINE + 1)
    notify = os.environ.get("NOTIFY_SOCKET")
    if notify:
        address = "\0" + notify[1:] if notify.startswith("@") else notify
        with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as notification:
            notification.sendto(b"READY=1", address)
    try:
        async with server:
            await server.serve_forever()
    finally:
        service.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--state", type=Path, default=Path("/var/lib/codex-studio/guest"))
    parser.add_argument("--store", type=Path, default=Path("/var/lib/codex-studio/workspaces"))
    parser.add_argument("--projects", type=Path, default=Path("/var/lib/codex-studio/projects"))
    parser.add_argument("--unix-socket", type=Path, help="Use a private Unix socket for integration tests")
    asyncio.run(serve(parser.parse_args()))


if __name__ == "__main__":
    main()
