"""Root broker slot adapter.

A slot is a folder of the agent that holds one layr state of its line: `layr save` captures the
line, `layr export --all` writes the state into the folder (incrementally after the first time),
and the collect step brings the paths that the Mac command changed back into the line with a
three-way merge against that held state (host_exec_slot_io.py, as the agent).
"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import signal
import sys
from typing import Any

from host_exec_protocol import HostExecError, atomic, conflicts, inside, key, require

EXPORT_MARKER = ".layr-export.json"
METHODS = {"host.slot.prepare", "host.slot.manifest", "host.slot.read", "host.slot.apply", "host.slot.collect", "host.slot.forget", "host.slot.artifact"}
READ_METHODS = {"host.slot.manifest", "host.slot.read"}


class HostSlotHandlers:
    def __init__(self, state: Path, layr_root: Path, agents: Any):
        self.state, self.layr_root, self.agents = state, layr_root, agents
        self.root = layr_root.parent / "host-slots"
        self.root.mkdir(exist_ok=True, mode=0o711)

    def layr_env(self, context: dict[str, Any]) -> dict[str, str]:
        return {"PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8", "LAYR_ROOT": str(self.layr_root),
                "LAYR_PROJECT": context["projectId"], "LAYR_LINE": context["line"], "HOME": context["home"],
                "LAYR_SESSION": context["agentId"]}

    async def layr(self, context: dict[str, Any], args: list[str], ok: tuple[int, ...] = (0,)) -> str:
        environment = self.layr_env(context)
        child = await asyncio.create_subprocess_exec("layr", *args, cwd=context["cwd"], env=environment,
            user=context["uid"], group=context["gid"], extra_groups=[], start_new_session=True,
            stdin=asyncio.subprocess.DEVNULL, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
        try:
            stdout, stderr = await asyncio.wait_for(child.communicate(), 1800)
        except asyncio.TimeoutError as exc:
            os.killpg(child.pid, signal.SIGKILL)
            await child.wait()
            raise HostExecError("outcome_unknown", "The layr slot operation exceeded its deadline") from exc
        output = stdout.decode(errors="replace")
        require(child.returncode in ok, "layr " + args[0] + " failed: " + (stderr.decode(errors="replace") + output)[-2000:], "layr_failed")
        return output

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
        # The layr state the slot folder holds (root-owned, next to the agent's folder).
        held = parent / "held.json"
        if method == "host.slot.prepare":
            if lease.exists():
                require(json.loads(lease.read_text())["operationId"] == operation,
                        "Another command retains the guest slot lease", "busy")
            atomic(lease, {"operationId": operation})
            await self.layr(context, ["save", "-q", "--force", "-m", "host_exec slot " + params["slotId"]])
            state = (await self.layr(context, ["rev-parse", "HEAD@{0}"])).strip()
            require(len(state) >= 7 and all(c in "0123456789abcdef" for c in state), "layr gave no state ID", "layr_failed")
            if not held.exists():
                # An unknown folder content: start from an empty folder, then a full copy.
                await self.io(context, {"action": "clear", "root": str(source)})
            # Exit code 1: names that differ only in case or Unicode form were not written.
            output = await self.layr(context, ["export", state, str(source), "--all", "--force"], ok=(0, 1))
            bad: list[str] = []
            marker = "they cannot coexist on APFS):"
            if marker in output:
                bad = [line.strip() for line in output.split(marker, 1)[1].splitlines() if line.strip()]
            atomic(held, {"state": state})
            return {"path": str(source), "nameConflicts": bad, "syncOutput": output,
                    "status": "slot holds state " + state}
        require(lease.exists() and json.loads(lease.read_text())["operationId"] == operation,
                "The command does not own its guest slot", "lease_lost")
        if method == "host.slot.forget":
            held.unlink(missing_ok=True)
            await self.io(context, {"action": "delete", "root": str(source), "path": EXPORT_MARKER})
            lease.unlink()
            return {"output": "slot forgotten"}
        if method == "host.slot.manifest":
            manifest_path = parent / (key(operation) + ".manifest.json")
            if not params.get("after"):
                entries = (await self.io(context, {"action": "manifest", "root": str(source)}))["entries"]
                entries.pop(EXPORT_MARKER, None)
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
            path_list = parent / (key(operation) + ".collect.json")
            all_paths = json.loads(path_list.read_text()) if path_list.exists() else []
            all_paths = sorted(set(all_paths) | set(paths))
            atomic(path_list, all_paths)
            if not params.get("final"):
                return {"output": "", "paths": len(all_paths), "conflicts": []}
            entries = (await self.io(context, {"action": "manifest", "root": str(source)}))["entries"]
            require(not conflicts(list(entries)), "The guest slot names conflict", "name_conflict")
            require(held.exists(), "The slot holds no known state", "lease_lost")
            report = await self.io(context, {"action": "collect", "root": str(source), "line": context["path"],
                                             "held": json.loads(held.read_text())["state"], "paths": all_paths,
                                             "env": self.layr_env(context)})
            output = "collected %d path(s) into line %s\n" % (len(report["copied"]), context["line"])
            if report["conflicts"]:
                output += "conflicts (the line also changed these paths):\n" + "".join(
                    "\t" + line + "\n" for line in report["conflicts"])
            lease.unlink()
            return {"output": output, "conflicts": report["conflicts"]}
        raise HostExecError("invalid_params", "Unknown host slot method")
