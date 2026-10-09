"""Root broker slot adapter. layr owns state capture and source collection."""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import signal
import sys
from typing import Any

from host_exec_protocol import HostExecError, atomic, conflicts, inside, key, require

METHODS = {"host.slot.prepare", "host.slot.manifest", "host.slot.read", "host.slot.apply", "host.slot.collect", "host.slot.forget", "host.slot.artifact"}
READ_METHODS = {"host.slot.manifest", "host.slot.read"}


class HostSlotHandlers:
    def __init__(self, state: Path, layr_root: Path, agents: Any):
        self.state, self.layr_root, self.agents = state, layr_root, agents
        self.root = layr_root.parent / "host-slots"
        self.root.mkdir(exist_ok=True, mode=0o711)

    async def layr(self, context: dict[str, Any], args: list[str]) -> str:
        environment = {**os.environ, "LAYR_ROOT": str(self.layr_root), "LAYR_PROJECT": context["projectId"],
                       "LAYR_LINE": context["line"], "HOME": context["home"]}
        child = await asyncio.create_subprocess_exec("layr", *args, cwd=context["cwd"], env=environment,
            user=context["uid"], group=context["gid"], extra_groups=[], start_new_session=True,
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            stdout, stderr = await asyncio.wait_for(child.communicate(), 1800)
        except asyncio.TimeoutError as exc:
            os.killpg(child.pid, signal.SIGKILL)
            await child.wait()
            raise HostExecError("outcome_unknown", "The layr slot operation exceeded its deadline") from exc
        require(child.returncode == 0, "layr slot failed: " + stderr.decode(errors="replace")[-2000:], "layr_failed")
        return stdout.decode(errors="replace")

    async def io(self, context: dict[str, Any], params: dict[str, Any]) -> dict[str, Any]:
        child = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("host_exec_slot_io.py")),
            user=context["uid"], group=context["gid"], extra_groups=[], start_new_session=True,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
        try:
            output, _ = await asyncio.wait_for(child.communicate(json.dumps(params).encode()), 300)
        except asyncio.TimeoutError as exc:
            os.killpg(child.pid, signal.SIGKILL)
            await child.wait()
            raise HostExecError("outcome_unknown", "The slot file operation exceeded its deadline") from exc
        require(len(output) <= 150 * 1024**2, "The slot manifest exceeds its limit", "output_limit")
        result = json.loads(output)
        if "error" in result:
            raise HostExecError(result["error"]["code"], result["error"]["message"])
        require(child.returncode == 0, "The slot file helper failed", "outcome_unknown")
        return dict(result["result"])

    async def dispatch(self, request_id: str, method: str, params: dict[str, Any], emit: Any = None) -> dict[str, Any]:
        context = await self.agents.dispatch(request_id + ":context", "agent.context", {"agentId": params["agentId"]}, emit)
        require(not context.get("readOnly"), "A review line cannot accept host command changes", "forbidden")
        require(isinstance(params.get("slotId"), str) and params["slotId"].isdigit(), "Invalid host slot ID")
        require(isinstance(params.get("generation"), str) and len(params["generation"]) == 32
                and all(char in "0123456789abcdef" for char in params["generation"]), "Invalid host slot generation")
        parent = self.root
        for part in (key(context["projectId"]), key(context["agentId"]), params["slotId"], params["generation"]):
            parent /= part
            parent.mkdir(exist_ok=True, mode=0o711)
        source = parent / "source"
        source.mkdir(exist_ok=True, mode=0o700)
        os.chown(source, context["uid"], context["gid"])
        operation = params.get("operationId")
        require(isinstance(operation, str) and 0 < len(operation) <= 128, "Invalid slot operation ID")
        lease = parent / "lease.json"
        if method == "host.slot.prepare":
            if lease.exists():
                require(json.loads(lease.read_text())["operationId"] == operation,
                        "Another command retains the guest slot lease", "busy")
            atomic(lease, {"operationId": operation})
            output = await self.layr(context, ["slot", "sync", str(source), "--force"])
            bad: list[str] = []
            marker = "not copied (names differ only in case or Unicode form):"
            if marker in output:
                bad = [line.strip() for line in output.split(marker, 1)[1].splitlines() if line.strip()]
            return {"path": str(source), "nameConflicts": bad, "syncOutput": output,
                    "status": await self.layr(context, ["slot", "status", str(source)])}
        require(lease.exists() and json.loads(lease.read_text())["operationId"] == operation,
                "The command does not own its guest slot", "lease_lost")
        if method == "host.slot.forget":
            output = await self.layr(context, ["slot", "forget", str(source)])
            lease.unlink()
            return {"output": output}
        if method == "host.slot.manifest":
            manifest_path = parent / (key(operation) + ".manifest.json")
            if not params.get("after"):
                entries = (await self.io(context, {"action": "manifest", "root": str(source)}))["entries"]
                atomic(manifest_path, entries)
            entries = json.loads(manifest_path.read_text())
            after = params.get("after", "")
            page = [(path, entries[path]) for path in sorted(entries) if path > after][:256]
            return {"entries": dict(page), "next": page[-1][0] if len(page) == 256 else None}
        if method == "host.slot.read":
            return await self.io(context, {**params, "root": str(source), "action": "read"})
        if method in {"host.slot.apply", "host.slot.artifact"}:
            if method == "host.slot.artifact":
                # Root parents protect the artifact folder inode. The line owner owns its content.
                source = parent / ("artifacts-" + key(operation))
                source.mkdir(exist_ok=True, mode=0o700)
                os.chown(source, context["uid"], context["gid"])
            entry_path = parent / (key(method + ":" + params["path"]) + ".entry.json")
            values = {**params, "root": str(source)}
            if params["action"] == "entry" and params["entry"]["kind"] == "file":
                atomic(entry_path, params["entry"])
            elif params["action"] in {"chunk", "commit"}:
                values["entry"] = json.loads(entry_path.read_text())
            return await self.io(context, values)
        if method == "host.slot.collect":
            paths = params.get("paths", [])
            require(isinstance(paths, list) and len(paths) <= 128, "The collect path count exceeds its limit")
            for path in paths:
                inside(source, path)
            entries = (await self.io(context, {"action": "manifest", "root": str(source)}))["entries"]
            require(not conflicts(list(entries)), "The guest slot names conflict", "name_conflict")
            output = await self.layr(context, ["slot", "collect", str(source), *paths])
            if params.get("final"):
                lease.unlink()
            return {"output": output}
        raise HostExecError("invalid_params", "Unknown host slot method")
