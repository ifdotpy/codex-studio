#!/usr/bin/env python3
"""Host_exec caller, lease, transfer, identity and output contracts."""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "vm/guest"))
from codex_host_exec_server import HostService
from codex_host_exec import run_tool, tool_definition
from host_exec import HostExec
from host_exec_protocol import HostExecError, conflicts, key, mapped, scan
from host_exec_slot import HostSlotHandlers


class Agents:
    def __init__(self, context):
        self.context = context

    async def dispatch(self, request_id, method, params, emit=None):
        if params["agentId"] != self.context["agentId"]:
            raise HostExecError("forbidden", "Unknown agent")
        return self.context


class Slots(HostSlotHandlers):
    async def io(self, context, params):
        from host_exec_slot_io import run
        return run(params)

    # Contract fixture for the external layr binary. Live proof uses real layr.
    async def layr(self, context, args):
        source = Path(args[2])
        line = Path(context["path"])
        if args[1] == "sync":
            shutil.rmtree(source)
            shutil.copytree(line, source, symlinks=True)
            if self.bad_names:
                return "not copied (names differ only in case or Unicode form):\n\tReadme\n\tREADME\n"
            return "slot holds state fixture"
        if args[1] == "collect":
            paths = Path(args[4]).read_text().splitlines() if args[3:4] == ["--paths-from"] else args[3:]
            self.collect_batches.append(paths)
            for relative in paths:
                src, dst = source / relative, line / relative
                if dst.is_symlink() or dst.is_file():
                    dst.unlink()
                elif dst.exists():
                    shutil.rmtree(dst)
                if src.is_symlink():
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    dst.symlink_to(os.readlink(src))
                elif src.is_file():
                    dst.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(src, dst)
            return "collected fixture paths"
        return "slot fixture"


class Contract(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="host-exec-contract-", dir="/tmp")
        self.root = Path(self.temp.name).resolve()
        self.state = self.root / "state"
        self.state.mkdir()
        (self.state / "host-exec-token").write_text("a" * 64)
        self.host = HostService(self.state, slots=2)
        self.line = self.root / "line"
        self.line.mkdir()
        (self.line / "main.swift").write_text('print("Hello")\n')
        home = self.root / "home"
        home.mkdir()
        context = {"agentId": "agent", "projectId": "project", "line": "worker", "path": str(self.line),
                   "cwd": str(self.line), "uid": os.getuid(), "gid": os.getgid(), "home": str(home), "readOnly": False}
        layr = self.root / "layr"
        layr.mkdir()
        self.slots = Slots(self.state, layr, Agents(context))
        self.slots.bad_names = False
        self.slots.collect_batches = []
        async def host_context(agent):
            return await self.slots.agents.dispatch("context", "agent.context", {"agentId": agent})
        self.guest = HostExec(SimpleNamespace(state=self.state, host_context=host_context))
        self.guest.configure({"token": "a" * 64})
        async def call(method, params, phase):
            response = await self.host.request({"id": key(params["operationId"] + ":" + phase),
                "method": method, "params": params, "token": "a" * 64})
            if "error" in response:
                raise HostExecError(response["error"]["code"], response["error"]["message"])
            return response["result"]
        self.guest.call = call
        async def broker(method, params, phase):
            return await self.slots.dispatch(key(params["operationId"] + phase), method, params)
        self.guest.broker = broker
        self.events = []
        async def emit(event, value):
            self.events.append(value)
        self.emit = emit

    async def asyncTearDown(self):
        for child in list(self.host.children.values()):
            await self.host.terminate(child)
        await asyncio.gather(*list(self.host.commands.values()), return_exceptions=True)
        self.host.close()
        self.temp.cleanup()

    async def execute(self, operation, script, timeout=5):
        return await self.guest.handle(operation, {"agentId": "agent", "operationId": operation,
            "argv": [sys.executable, "-c", script], "cwd": str(self.line), "timeoutSeconds": timeout}, self.emit)

    async def test_source_return_artifact_file_and_warm_path(self):
        first = await self.execute("first", 'import os,pathlib; p=pathlib.Path("main.swift");p.write_text("host edit\\n");'
            'pathlib.Path(os.environ["HOST_EXEC_ARTIFACTS"],"result.txt").write_text("artifact");print(p.resolve())')
        self.assertEqual(first["exitCode"], 0)
        self.assertTrue(first["collected"])
        self.assertEqual((self.line / "main.swift").read_text(), "host edit\n")
        self.assertEqual(Path(first["artifacts"][0]["path"]).read_text(), "artifact")
        self.assertNotIn(first["slotPath"], "".join(row.get("text", "") for row in self.events))
        self.assertIn(str(self.line), "".join(row.get("text", "") for row in self.events))
        second = await self.execute("second", 'print("warm")')
        self.assertEqual(first["slotPath"], second["slotPath"])
        self.assertEqual(first["derivedData"], second["derivedData"])
        self.assertEqual(second["transferredBytes"], 0)

    async def test_exact_retry_and_changed_content(self):
        params = {"operationId": "retry", "agentId": "agent", "projectId": "project", "linePath": str(self.line)}
        request = {"id": "acquire", "method": "acquire", "params": params, "token": "a" * 64}
        first = await self.host.request(request)
        self.assertEqual(await self.host.request(request), first)
        changed = await self.host.request({**request, "params": {**params, "projectId": "other"}})
        self.assertEqual(changed["error"]["code"], "id_conflict")
        manifest = scan(Path(first["result"]["slotPath"]))
        run = {"id": "run-once", "method": "run", "token": "a" * 64, "params": {"operationId": "retry", "agentId": "agent",
            "argv": [sys.executable, "-c", 'import pathlib;p=pathlib.Path("count");p.write_text(p.read_text()+"x" if p.exists() else "x")'],
            "manifestSha256": key(json.dumps(manifest, sort_keys=True))}}
        response = await self.host.request(run)
        await asyncio.gather(*list(self.host.commands.values()))
        self.assertEqual(await self.host.request(run), response)
        self.assertEqual((Path(first["result"]["slotPath"]) / "count").read_text(), "x")

    async def test_unknown_receipt_never_replays(self):
        params = {"operationId": "unknown", "agentId": "agent", "projectId": "project", "linePath": str(self.line)}
        digest = hashlib.sha256(json.dumps(["acquire", params], sort_keys=True, allow_nan=False).encode()).hexdigest()
        self.host.db.execute("INSERT INTO receipts VALUES (?,?,NULL)", ("lost", digest))
        self.host.db.commit()
        response = await self.host.request({"id": "lost", "method": "acquire", "params": params, "token": "a" * 64})
        self.assertEqual(response["error"]["code"], "outcome_unknown")
        self.assertFalse(self.host.opdir("unknown").exists())

    async def test_token_and_agent_identity(self):
        denied = await self.host.request({"id": "bad-token", "method": "health", "token": "b" * 64})
        self.assertEqual(denied["error"]["code"], "forbidden")
        await self.guest.call("acquire", {"operationId": "owned", "agentId": "agent", "projectId": "project", "linePath": str(self.line)}, "acquire")
        with self.assertRaises(HostExecError) as caught:
            await self.guest.call("status", {"operationId": "owned", "agentId": "other"}, "other")
        self.assertEqual(caught.exception.code, "forbidden")

    async def test_slots_are_exclusive(self):
        for operation in ("a", "b"):
            await self.guest.call("acquire", {"operationId": operation, "agentId": "agent", "projectId": "project", "linePath": str(self.line)}, "acquire")
        with self.assertRaises(HostExecError) as caught:
            await self.guest.call("acquire", {"operationId": "c", "agentId": "agent", "projectId": "project", "linePath": str(self.line)}, "acquire")
        self.assertEqual(caught.exception.code, "busy")

    async def test_collision_report_refuses_command(self):
        self.slots.bad_names = True
        result = await self.execute("names", 'open("ran", "w").write("bad")')
        self.assertEqual(result["reason"], "name_conflict")
        self.assertEqual(result["nameConflicts"], ["Readme", "README"])
        self.assertFalse((self.line / "ran").exists())
        self.assertFalse((Path(result["slotPath"]) / "ran").exists())

    async def test_timeout_collects_completed_edits(self):
        result = await self.execute("timeout", 'import time,pathlib;pathlib.Path("edit").write_text("before timeout");time.sleep(20)', timeout=0.1)
        self.assertEqual(result["reason"], "timeout")
        self.assertTrue(result["collected"])
        self.assertEqual((self.line / "edit").read_text(), "before timeout")

    async def test_cancel_retains_source_changes(self):
        task = asyncio.create_task(self.execute("cancel", 'import time,pathlib;pathlib.Path("edit").write_text("before cancel");time.sleep(20)'))
        deadline = asyncio.get_running_loop().time() + 5
        while not self.host.children and asyncio.get_running_loop().time() < deadline:
            await asyncio.sleep(0.01)
        await asyncio.sleep(0.1)
        await self.guest.handle("cancel-request", {"action": "cancel", "agentId": "agent", "operationId": "cancel"}, self.emit)
        result = await task
        self.assertEqual(result["reason"], "cancelled")
        self.assertEqual((self.line / "edit").read_text(), "before cancel")

    async def test_executable_mode_and_link_roundtrip(self):
        tool = self.line / "tool"
        tool.write_text("source")
        tool.chmod(0o755)
        (self.line / "link").symlink_to("tool")
        result = await self.execute("modes", 'import pathlib;assert pathlib.Path("tool").stat().st_mode & 0o111;'
            'assert pathlib.Path("link").readlink()==pathlib.Path("tool");pathlib.Path("tool").chmod(0o644)')
        self.assertTrue(result["collected"])
        self.assertEqual(tool.stat().st_mode & 0o777, 0o644)
        self.assertTrue((self.line / "link").is_symlink())

    async def test_path_escape_and_manifest_mismatch(self):
        op = await self.guest.call("acquire", {"operationId": "escape", "agentId": "agent", "projectId": "project", "linePath": str(self.line)}, "acquire")
        for path in ("../escape", "/absolute"):
            with self.assertRaises(HostExecError):
                await self.guest.call("sync.entry", {"operationId": "escape", "agentId": "agent", "path": path, "entry": {"kind": "directory"}}, "entry:" + path)
        with self.assertRaises(HostExecError) as caught:
            await self.guest.call("run", {"operationId": "escape", "agentId": "agent", "argv": ["true"], "manifestSha256": "bad"}, "run")
        self.assertEqual(caught.exception.code, "sync_conflict")
        self.assertEqual(self.host.load("escape")["state"], "leased")

    async def test_disk_budget_evicts_idle_lru_only(self):
        for operation in ("a", "b"):
            await self.guest.call("acquire", {"operationId": operation, "agentId": "agent", "projectId": "project", "linePath": str(self.line)}, "acquire")
        slot_a = self.host.slot(self.host.load("a"))
        slot_b = self.host.slot(self.host.load("b"))
        await self.guest.call("release", {"operationId": "a", "agentId": "agent"}, "release")
        (slot_a / "DerivedData" / "large").write_bytes(b"x" * 65536)
        self.host.budget = self.host.size(slot_b) + 8192
        self.host.evict(slot_b.parent, exclude=slot_b)
        self.assertFalse(slot_a.exists())
        self.assertTrue(slot_b.exists())
        with self.assertRaises(HostExecError):
            self.host.evict(slot_b.parent, reserve=self.host.budget + 1)

    async def test_output_alias_split_and_large_manifest(self):
        for index in range(270):
            (self.line / ("file-%03d" % index)).write_text("x")
        result = await self.execute("paged", 'import os,pathlib,time;value=str(pathlib.Path("main.swift").resolve());'
            'value=value.replace("/private/tmp/","/tmp/");data=value.encode();'
            'os.write(1,data[:len(data)//2]);time.sleep(0.02);os.write(1,data[len(data)//2:])')
        output = "".join(row.get("text", "") for row in self.events)
        self.assertEqual(output, str(self.line / "main.swift"))
        self.assertTrue(result["collected"])

    async def test_disk_budget_stops_cache_growth(self):
        self.host.budget = 64 * 1024
        result = await self.execute("budget", 'import os,pathlib,time;'
            'pathlib.Path(os.environ["HOST_EXEC_DERIVED_DATA"],"large").write_bytes(b"x"*128*1024);time.sleep(20)')
        self.assertEqual(result["reason"], "disk_budget")
        self.assertTrue(result["collected"])

    async def test_collection_uses_one_complete_path_set(self):
        result = await self.execute("many-edits", 'import pathlib;'
            '[pathlib.Path("edit-%03d" % index).write_text("from Mac") for index in range(140)]')
        self.assertTrue(result["collected"])
        self.assertEqual(len(self.slots.collect_batches), 1)
        self.assertEqual(len(self.slots.collect_batches[0]), 140)

    async def test_layr_merge_conflict_report_survives_code_one(self):
        marker = "conflicts (the line also changed these paths):\n\tmain.swift (content: markers written)\n"
        child = SimpleNamespace(returncode=1, communicate=AsyncMock(return_value=(marker.encode(), b"")))
        with patch("host_exec_slot.asyncio.create_subprocess_exec", return_value=child) as launch:
            report = await HostSlotHandlers.layr(self.slots, self.slots.agents.context,
                ["slot", "collect", str(self.line)])
        self.assertEqual(report, marker)
        self.assertEqual(launch.call_args.kwargs["user"], os.getuid())
        self.assertEqual(launch.call_args.kwargs["group"], os.getgid())
        child.communicate = AsyncMock(return_value=(b"other failure", b"error"))
        with patch("host_exec_slot.asyncio.create_subprocess_exec", return_value=child):
            with self.assertRaises(HostExecError) as caught:
                await HostSlotHandlers.layr(self.slots, self.slots.agents.context,
                    ["slot", "collect", str(self.line)])
        self.assertEqual(caught.exception.code, "layr_failed")

    async def test_guest_result_reports_collection_conflicts(self):
        original = self.slots.layr
        async def conflict(context, args):
            output = await original(context, args)
            if args[1] == "collect":
                output += "\nconflicts (the line also changed these paths):\n\tmain.swift (content: markers written)\n"
            return output
        self.slots.layr = conflict
        result = await self.execute("merge-conflict", 'import pathlib;pathlib.Path("main.swift").write_text("host edit")')
        self.assertEqual(result["state"], "conflicted")
        self.assertTrue(result["collected"])
        self.assertEqual(result["collectionConflicts"], ["main.swift (content: markers written)"])

    async def test_lost_owner_retains_lease(self):
        op = await self.guest.call("acquire", {"operationId": "lost-owner", "agentId": "agent", "projectId": "project", "linePath": str(self.line)}, "acquire")
        op["state"] = "running"
        self.host.save("lost-owner", op)
        self.assertEqual(self.host.load("lost-owner")["state"], "unknown")
        with self.assertRaises(HostExecError) as caught:
            await self.guest.call("release", {"operationId": "lost-owner", "agentId": "agent"}, "release")
        self.assertEqual(caught.exception.code, "invalid_params")
        self.assertTrue((self.host.slot(op) / "lease.json").exists())

    def test_name_and_alias_rules(self):
        self.assertTrue(conflicts(["README", "readme"]))
        self.assertTrue(conflicts(["café", "cafe\u0301"]))
        self.assertTrue(conflicts(["Folder/a", "folder/b"]))
        self.assertEqual(mapped("/tmp/slot/source/file /private/tmp/slot/source/file", "/private/tmp/slot/source", "/line"), "/line/file /line/file")

    def test_vm_gate(self):
        runtime = SimpleNamespace(agent=lambda actor: {"executionMode": "native"})
        with self.assertRaises(PermissionError):
            run_tool(runtime, {"id": "agent"}, {"action": "execute", "command": "true"}, "tool")
        self.assertEqual(tool_definition()["name"], "host_exec")


if __name__ == "__main__":
    unittest.main(verbosity=2)
