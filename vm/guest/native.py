"""Relay the maintained native supervisor protocol without changing RPC identity."""
from __future__ import annotations

import asyncio
from contextlib import closing
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import uuid

from common import GuestError, MAX_LINE, atomic_json, integer, private_dir, require


class Native:
    def __init__(self, state: Path):
        self.root = state / "native"
        private_dir(self.root)
        self.socket = self.root / "supervisor.sock"
        self.backend = "linux-guest-service"
        self.start_lock = asyncio.Lock()
        self.process = None

    async def call(self, action, *, operator=False, **params):
        reader, writer = await asyncio.wait_for(asyncio.open_unix_connection(str(self.socket), limit=MAX_LINE + 1), 2)
        try:
            writer.write(json.dumps({"protocol": 1, "stateDir": str(self.root),
                                     "backendId": self.backend, "operator": operator}).encode() + b"\n")
            await asyncio.wait_for(writer.drain(), 2)
            hello = json.loads(await asyncio.wait_for(reader.readline(), 2))
            if hello.get("error"):
                raise GuestError("outcome_unknown", "The native supervisor refused the connection")
            request_id = uuid.uuid4().hex
            writer.write(json.dumps({"requestId": request_id, "action": action, **params}).encode() + b"\n")
            await asyncio.wait_for(writer.drain(), 2)
            line = await asyncio.wait_for(reader.readline(), 8)
            if not line:
                raise GuestError("outcome_unknown", "The native supervisor disconnected")
            response = json.loads(line)
            if response.get("requestId") != request_id:
                raise GuestError("outcome_unknown", "The native supervisor response identity differs")
            if response.get("error"):
                # Do not expose executable arguments or native message contents in errors.
                raise GuestError("outcome_unknown", "The native supervisor rejected the operation; inspect its state")
            return response["result"]
        except (OSError, asyncio.TimeoutError, ValueError) as exc:
            raise GuestError("outcome_unknown", "The native supervisor operation has no proven result") from exc
        finally:
            writer.close()

    async def ensure(self):
        async with self.start_lock:
            if self.socket.exists():
                try:
                    await self.call("health")
                    return
                except GuestError:
                    # A socket with an unproven owner must not start another supervisor.
                    raise GuestError("outcome_unknown", "The existing native supervisor is unavailable")
            script = Path(__file__).resolve().parents[2] / "scripts" / "codex_process_supervisor.py"
            self.process = subprocess.Popen(
                [sys.executable, str(script), "--state", str(self.root)],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL, start_new_session=True, close_fds=True)
            asyncio.create_task(self.reap(self.process))
            deadline = asyncio.get_running_loop().time() + 8
            while asyncio.get_running_loop().time() < deadline:
                if self.process.poll() is not None:
                    raise GuestError("internal", "The native supervisor failed to start")
                if self.socket.exists():
                    await self.call("health")
                    return
                await asyncio.sleep(0.025)
            raise GuestError("timeout", "The native supervisor did not become ready")

    async def reap(self, process):
        while process.poll() is None:
            await asyncio.sleep(0.1)

    def info(self, handle):
        path = self.root / "supervisor.sqlite3"
        require(path.exists(), "The native supervisor journal does not exist")
        with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
            row = db.execute("SELECT pid,sequence,acknowledged,generation,init_result,closed_at,closed_reason "
                             "FROM handles WHERE id=?", (handle,)).fetchone()
        if not row:
            raise GuestError("not_found", "The native provider handle does not exist")
        state = "running" if row[5] is None else "exited"
        exit_code = None
        if state == "running":
            with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
                identity = db.execute("SELECT start_time FROM child_identities WHERE handle=? AND pid=?", (handle, row[0])).fetchone()
                event = db.execute("SELECT payload FROM events WHERE handle=? AND generation=? AND kind='exit' ORDER BY sequence DESC LIMIT 1",
                                   (handle, row[3])).fetchone()
            sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
            from codex_process_supervisor import process_start_time
            if not identity or process_start_time(row[0]) != identity[0]:
                state = "lost"
                proof_path = self.exit_path(handle)
                proof = json.loads(proof_path.read_text()) if proof_path.exists() else {}
                if event:
                    state, exit_code = "exited", json.loads(event[0]).get("returnCode")
                elif proof.get("generation") == row[3]:
                    state, exit_code = "exited", proof["returnCode"]
        return {"handle": handle, "pid": row[0], "sequence": row[1], "lastSeq": row[1],
                "acknowledged": row[2], "generation": row[3], "exitCode": exit_code,
                "initResult": json.loads(row[4]) if row[4] else None,
                "state": state, "reason": "native_process_lost" if state == "lost" else row[6]}

    def exit_path(self, handle):
        return self.root / ("exit-" + uuid.uuid5(uuid.NAMESPACE_URL, handle).hex + ".json")

    def save_exit(self, handle, code):
        info = self.info(handle)
        atomic_json(self.exit_path(handle), {"generation": info["generation"], "returnCode": code})

    async def start(self, handle, config, directory):
        await self.ensure()
        # Save only the provider association. The existing supervisor owns launch identity.
        atomic_json(directory / "native.json", {"handle": handle, "agentId": config.get("agentId")})
        result = await self.call("open", handle=handle, command=config["argv"], env=config["env"], cwd=config["cwd"])
        return {**self.info(handle), **result, "transport": "native"}

    async def rpc(self, params):
        action = params.get("action")
        require(action in {"write", "operationStatus", "ack", "replay", "status", "next", "detach", "info", "responseStatus"},
                "The native supervisor action is not supported")
        handle = params["handle"]
        if action == "info":
            return self.info(handle)
        if action == "responseStatus":
            native_id = params.get("nativeId")
            require(type(native_id) in {str, int}, "nativeId must be a string or integer")
            generation = integer(params.get("generation"), None, 1, 2**63 - 1)
            info = self.info(handle)
            accepted = False
            if generation == info["generation"] and info["state"] == "running":
                path = self.root / "supervisor.sqlite3"
                with closing(sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
                    operation = db.execute("SELECT 1 FROM operations WHERE handle=? AND native_id=? AND generation=? LIMIT 1",
                                           (handle, native_id, generation)).fetchone()
                    identity = db.execute("SELECT start_time FROM child_identities WHERE handle=? AND pid=?",
                                          (handle, info["pid"])).fetchone()
                from codex_process_supervisor import process_start_time
                accepted = bool(operation and identity and process_start_time(info["pid"]) == identity[0])
            return {"accepted": accepted, "generation": info["generation"], "state": info["state"]}
        if action == "write":
            require(isinstance(params.get("message"), dict), "The native message must be an object")
            require(isinstance(params.get("operationId"), str) and 0 < len(params["operationId"]) <= 256,
                    "The native operationId must be a bounded string")
        fields = {"write": ("operationId", "nativeId", "message"), "operationStatus": ("operationId",),
                  "ack": ("sequence",), "replay": ("cursor",), "next": ("cursor",), "status": (), "detach": ()}
        values = {key: params.get(key) for key in fields[action]}
        if action == "ack":
            info = self.info(handle)
            if info["state"] == "exited" and info.get("exitCode") is not None:
                self.save_exit(handle, info["exitCode"])
            if isinstance(values["sequence"], int) and 0 <= values["sequence"] <= info["acknowledged"]:
                return {"acknowledged": info["acknowledged"]}
        if action == "replay":
            info = self.info(handle)
            cursor = integer(values["cursor"], None, info["acknowledged"], info["sequence"])
            limit = integer(params.get("limit"), 128, 1, 128)
            with closing(sqlite3.connect((self.root / "supervisor.sqlite3").as_uri() + "?mode=ro", uri=True, timeout=2)) as db:
                rows = db.execute("SELECT sequence,kind,payload,generation FROM events WHERE handle=? AND sequence>? ORDER BY sequence LIMIT ?",
                                  (handle, cursor, limit)).fetchall()
            events = []
            size = 0
            for sequence, kind, payload, generation in rows:
                event = {"sequence": sequence, "kind": kind, "payload": payload, "generation": generation}
                size += len(json.dumps(event).encode())
                if size > MAX_LINE - 4096:
                    break
                events.append(event)
            return {"events": events, "sequence": info["sequence"], "acknowledged": info["acknowledged"],
                    "backpressure": bool((await self.call("status", handle=handle)).get("backpressure")),
                    "hasMore": bool(rows) and (not events or events[-1]["sequence"] < info["sequence"])}
        result = await self.call(action, handle=handle, **values)
        if result.get("returnCode") is not None:
            self.save_exit(handle, result["returnCode"])
        return result

    async def stop(self, handle):
        status = await self.call("status", handle=handle)
        if status.get("returnCode") is not None:
            self.save_exit(handle, status["returnCode"])
            return self.info(handle)
        health = await self.call("health")
        row = next((row for row in health["handles"] if row["id"] == handle), None)
        if row is None:
            return self.info(handle)
        await self.call("adminCloseHandle", operator=True, handle=handle, expectedPid=row["pid"],
                        expectedStartTime=row["startTime"], expectedSignature=row["signature"])
        return self.info(handle)
