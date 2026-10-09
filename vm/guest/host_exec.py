"""Guest caller for authenticated macOS commands and layr slot collection."""
from __future__ import annotations

import asyncio
import json
from pathlib import Path
import socket
from typing import Any
import uuid

from host_exec_protocol import FRAME_LIMIT, PORT, HostExecError, atomic, conflicts, key, relative, require


def request_key(operation: str, phase: str) -> str:
    return uuid.uuid5(uuid.NAMESPACE_URL, operation + ":" + phase).hex


class HostExec:
    def __init__(self, service: Any):
        self.service = service
        self.config = service.state / "host-exec.json"

    def configure(self, params: dict[str, Any]) -> dict[str, Any]:
        token = params.get("token")
        require(isinstance(token, str) and len(token) == 64 and all(char in "0123456789abcdef" for char in token),
                "Invalid host channel token")
        atomic(self.config, {"token": token})
        self.config.chmod(0o600)
        return {"configured": True}

    async def call(self, method: str, params: dict[str, Any], phase: str) -> dict[str, Any]:
        require(self.config.exists(), "The host command channel is not configured", "unavailable")
        identity = request_key(params["operationId"], phase)
        channel = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
        channel.setblocking(False)
        writer = None
        try:
            await asyncio.wait_for(asyncio.get_running_loop().sock_connect(channel, (socket.VMADDR_CID_HOST, PORT)), 15)
            reader, writer = await asyncio.open_connection(sock=channel, limit=FRAME_LIMIT + 1)
            data = json.dumps({"id": identity, "method": method, "params": params,
                               "token": json.loads(self.config.read_text())["token"]}).encode() + b"\n"
            require(len(data) <= FRAME_LIMIT, "The host request exceeds its frame limit")
            writer.write(data)
            await asyncio.wait_for(writer.drain(), 15)
            line = await asyncio.wait_for(reader.readline(), 300)
            require(line and len(line) <= FRAME_LIMIT, "The host response is missing or too large", "outcome_unknown")
            response = json.loads(line)
            require(response.get("id") == identity, "The host response ID differs", "outcome_unknown")
            if "error" in response:
                raise HostExecError(response["error"]["code"], response["error"]["message"])
            return dict(response["result"])
        except (OSError, asyncio.TimeoutError, ValueError) as exc:
            raise HostExecError("outcome_unknown", "The host response has no proven result; inspect operation " + params["operationId"]) from exc
        finally:
            if writer is not None:
                writer.close()
            else:
                channel.close()

    async def broker(self, method: str, params: dict[str, Any], phase: str) -> dict[str, Any]:
        from layr_admin_client import admin_request
        return await admin_request(request_key(params["operationId"], phase), method, params)

    async def manifest(self, method: str, params: dict[str, Any], phase: str, *, broker: bool = False) -> dict[str, Any]:
        result: dict[str, Any] = {}
        after = ""
        caller = self.broker if broker else self.call
        while True:
            page = await caller(method, {**params, "after": after}, phase + ":" + key(after))
            result.update(page["entries"])
            if not page["next"]:
                return result
            require(page["next"] > after, "The manifest cursor did not advance", "outcome_unknown")
            after = page["next"]

    async def handle(self, request_id: str, params: dict[str, Any], emit: Any) -> dict[str, Any]:
        identity = params.get("operationId", request_id)
        require(isinstance(identity, str) and 0 < len(identity) <= 128, "Invalid host operation ID")
        context = await self.service.host_context(params.get("agentId"))
        common = {"operationId": identity, "agentId": context["agentId"]}
        action = params.get("action", "execute")
        if action == "status":
            return await self.call("attach", {**common, "afterSeq": params.get("afterSeq", 0)}, "status:" + request_id)
        if action == "cancel":
            cancelled = await self.call("cancel", common, "cancel:" + request_id)
            if cancelled["state"] == "leased":
                slot = {**common, "slotId": cancelled["slotId"], "generation": cancelled["generation"]}
                try:
                    await self.broker("host.slot.forget", slot, "cancel-forget:" + request_id)
                except HostExecError as exc:
                    if exc.code != "lease_lost":
                        raise
                cancelled = await self.call("release", common, "cancel-release:" + request_id)
            return cancelled
        require(action == "execute", "Unknown host command action")
        require(not context.get("readOnly"), "A review line cannot accept host command changes", "forbidden")
        cwd = Path(params.get("cwd", context["cwd"]))
        line = Path(context["path"])
        require(cwd.is_absolute() and cwd.is_relative_to(line) and ".." not in cwd.parts,
                "The host command cwd is outside its line")
        op = await self.call("acquire", {**common, "projectId": context["projectId"], "linePath": str(line)}, "acquire")
        slot = {**common, "slotId": op["slotId"], "generation": op["generation"]}
        prepared = await self.broker("host.slot.prepare", slot, "prepare")
        if prepared.get("nameConflicts"):
            await self.broker("host.slot.forget", slot, "conflict-forget")
            await self.call("release", common, "conflict-release")
            return {**op, "state": "refused", "reason": "name_conflict", "nameConflicts": prepared["nameConflicts"]}
        target = await self.manifest("host.slot.manifest", slot, "target", broker=True)
        bad = conflicts(list(target))
        if bad:
            await self.broker("host.slot.forget", slot, "conflict-forget")
            await self.call("release", common, "conflict-release")
            return {**op, "state": "refused", "reason": "name_conflict", "nameConflicts": bad}
        held = await self.manifest("manifest", common, "held")
        deleted = [path for path in held if path not in target or held[path]["kind"] != target[path]["kind"]]
        for path in sorted(deleted, key=lambda value: (-value.count("/"), value)):
            await self.call("sync.delete", {**common, "path": path}, "delete:" + key(path))
        sent = 0
        for path in sorted(target, key=lambda value: (value.count("/"), value)):
            entry = target[path]
            if entry == held.get(path):
                continue
            await self.call("sync.entry", {**common, "path": path, "entry": entry}, "entry:" + key(path))
            if entry["kind"] == "file":
                offset = 0
                while offset < entry["bytes"]:
                    data = await self.broker("host.slot.read", {**slot, "path": path, "offset": offset}, "source:" + key(path) + ":" + str(offset))
                    require(data["bytes"] > 0, "The source ended before its declared size", "sync_conflict")
                    await self.call("sync.chunk", {**common, "path": path, "offset": offset, "data": data["data"]}, "chunk:" + key(path) + ":" + str(offset))
                    offset += data["bytes"]
                    sent += data["bytes"]
                await self.call("sync.commit", {**common, "path": path}, "commit:" + key(path))
        relative_cwd = cwd.relative_to(line).as_posix()
        await self.call("run", {**common, "argv": params["argv"], "cwd": "" if relative_cwd == "." else relative_cwd,
            "manifestSha256": key(json.dumps(target, sort_keys=True)), "timeoutSeconds": params.get("timeoutSeconds", 300)}, "run")
        after_seq = 0
        while True:
            attached = await self.call("attach", {**common, "afterSeq": after_seq, "waitSeconds": 10}, "attach:" + str(after_seq))
            for event in attached["events"]:
                after_seq = event["seq"]
                if "text" in event:
                    await emit("output", event)
            final = attached["status"]
            if final["state"] == "unknown":
                raise HostExecError("outcome_unknown", "The host command outcome is unknown; inspect operation " + identity)
            if final["state"] == "exited" and not attached["events"]:
                break
        changes = await self.manifest("result.manifest", {**common, "area": "changes"}, "changes")
        if final.get("nameConflicts"):
            return {**final, "state": "collection_refused", "transferredBytes": sent}
        for path in sorted(changes, key=lambda value: (-value.count("/"), value)):
            if changes[path] is None:
                await self.broker("host.slot.apply", {**slot, "action": "delete", "path": path}, "back-delete:" + key(path))
        for path in sorted(changes, key=lambda value: (value.count("/"), value)):
            entry = changes[path]
            if entry is None:
                continue
            await self.broker("host.slot.apply", {**slot, "action": "entry", "path": path, "entry": entry}, "back-entry:" + key(path))
            if entry["kind"] == "file":
                offset = 0
                while offset < entry["bytes"]:
                    data = await self.call("read", {**common, "area": "changes", "path": path, "offset": offset}, "back-read:" + key(path) + ":" + str(offset))
                    require(data["bytes"] > 0, "The result ended before its declared size", "outcome_unknown")
                    await self.broker("host.slot.apply", {**slot, "action": "chunk", "path": path, "offset": offset, "data": data["data"]}, "back-chunk:" + key(path) + ":" + str(offset))
                    offset += data["bytes"]
                await self.broker("host.slot.apply", {**slot, "action": "commit", "path": path}, "back-commit:" + key(path))
        paths = list(changes)
        # layr collect receives file paths, not the directory entries of the transport.
        paths = [path for path in paths if (changes[path] or target.get(path, {})).get("kind") != "directory"]
        artifacts = await self.manifest("result.manifest", {**common, "area": "artifacts"}, "artifacts")
        artifact_files = []
        for path, entry in artifacts.items():
            if entry["kind"] != "file":
                continue
            receipt = await self.broker("host.slot.artifact", {**slot, "action": "entry", "path": path, "entry": entry}, "artifact-entry:" + key(path))
            offset = 0
            while offset < entry["bytes"]:
                data = await self.call("read", {**common, "area": "artifacts", "path": path, "offset": offset}, "artifact-read:" + key(path) + ":" + str(offset))
                require(data["bytes"] > 0, "The artifact ended before its declared size", "outcome_unknown")
                await self.broker("host.slot.artifact", {**slot, "action": "chunk", "path": path, "offset": offset, "data": data["data"]}, "artifact-chunk:" + key(path) + ":" + str(offset))
                offset += data["bytes"]
            await self.broker("host.slot.artifact", {**slot, "action": "commit", "path": path}, "artifact-commit:" + key(path))
            artifact_files.append({"path": receipt["absolutePath"], "hostPath": str(Path(op["artifactsPath"]) / relative(path)), **entry})
        reports = []
        merge_conflicts = []
        for offset in range(0, max(1, len(paths)), 128):
            report = await self.broker("host.slot.collect", {**slot, "paths": paths[offset:offset + 128],
                "final": offset + 128 >= len(paths)}, "collect:" + str(offset))
            reports.append(report["output"])
            merge_conflicts.extend(report.get("conflicts", []))
        await self.call("release", {**common, "collected": True}, "release")
        return {**final, "state": "conflicted" if merge_conflicts else final["state"], "collected": True,
                "collectOutput": "".join(reports), "collectionConflicts": merge_conflicts, "transferredBytes": sent,
                "artifacts": artifact_files, "lastSeq": after_seq}
