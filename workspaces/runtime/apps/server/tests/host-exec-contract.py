#!/usr/bin/env python3
"""Host_exec caller, lease, transfer, identity and output contracts."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import asyncio
import base64
import errno
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_TESTS_ROOT, VM_GUEST_ROOT  # noqa: E402
from server_test_waits import wait_for_condition  # noqa: E402

sys.path.insert(0, str(VM_GUEST_ROOT))
import codex_host_exec_server
from codex_host_exec_server import HostService, clone_tree
from codex_host_exec import run_tool, tool_definition
from host_exec import HostExec
import host_exec_protocol
from host_exec_protocol import FRAME_LIMIT, HostExecError, conflicts, key, mapped, page, scan
from host_exec_slot import HostSlotHandlers


class Agents:
    def __init__(self, context):
        self.context = context
        self.contexts = {context["agentId"]: context}

    async def dispatch(self, request_id, method, params, emit=None):
        if params["agentId"] not in self.contexts:
            raise HostExecError("forbidden", "Unknown agent")
        return self.contexts[params["agentId"]]


class Slots(HostSlotHandlers):
    # Contract fixture for the external layr binary and the line merge. Live proof uses real
    # layr; the three-way collect of host_exec_slot_io has its own test below.
    async def io(self, context, params):
        if params["action"] == "collect":
            self.collect_batches.append(params["paths"])
            source, line = Path(params["root"]), Path(params["line"])
            for relative in params["paths"]:
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
            return {"copied": list(params["paths"]), "conflicts": list(self.collect_conflicts)}
        from host_exec_slot_io import run
        return run(params)

    async def layr(self, context, args, ok=(0,)):
        line = Path(context["path"])
        if args[0] == "rev-parse":
            return "0123456789abcdef0123456789abcdef"
        if args[:2] == ["cache", "list"]:
            return "".join(path + ("\tcache\n" if (line / path).is_dir() else "\tabsent\n") for path in self.caches)
        if args[0] == "export":
            source = Path(args[2])
            shutil.rmtree(source)
            # layr keeps cache folders outside states: an export has none of their files.
            shutil.copytree(line, source, symlinks=True, ignore=lambda folder, names: [
                name for name in names if (Path(folder) / name).relative_to(line).as_posix() in self.caches])
            (source / ".layr-export.json").write_text('{"state": "%s"}' % args[1])
            if self.bad_names:
                return ("exported state fixture\nnot written (names differ only in case or Unicode form; "
                        "they cannot coexist on APFS):\n\tReadme\n\tREADME\n")
            return "exported state fixture"
        return ""


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
        self.slots.caches = []
        self.slots.collect_batches = []
        self.slots.collect_conflicts = []
        async def host_context(agent):
            return await self.slots.agents.dispatch("context", "agent.context", {"agentId": agent})
        self.guest = HostExec(SimpleNamespace(state=self.state, host_context=host_context))
        self.guest.configure({"token": "a" * 64})
        self.calls = []
        async def call(method, params, phase, timeout=300):
            request = {"id": key(params["operationId"] + ":" + phase), "method": method, "params": params, "token": "a" * 64}
            # The real channel refuses a larger request.
            self.assertLessEqual(len(json.dumps(request)) + 1, FRAME_LIMIT)
            self.calls.append(method)
            response = await self.host.request(request)
            if "error" in response:
                raise HostExecError(response["error"]["code"], response["error"]["message"])
            return response["result"]
        self.guest.call = call
        self.applied = []
        async def broker(method, params, phase):
            if method == "host.slot.apply":
                self.applied.append(params["action"])
            result = await self.slots.dispatch(key(params["operationId"] + phase), method, params)
            # The admin channel has the same frame limit.
            self.assertLessEqual(len(json.dumps({"id": "x" * 64, "result": result})) + 1, FRAME_LIMIT)
            return result
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

    async def execute(self, operation, script, timeout=5, agent="agent"):
        cwd = self.slots.agents.contexts[agent]["path"]
        return await self.guest.handle(operation, {"agentId": agent, "operationId": operation,
            "argv": [sys.executable, "-c", script], "cwd": cwd, "timeoutSeconds": timeout}, self.emit)

    def another_line(self, agent, path):
        """A second line of the project with its own agent; the Mac slots belong to the project."""
        self.slots.agents.contexts[agent] = {**self.slots.agents.context, "agentId": agent, "line": agent,
                                            "path": str(path), "cwd": str(path)}

    def project_settings(self, value):
        folder = self.state / "layr-projects" / "project"
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "host-exec.json").write_text(value if isinstance(value, str) else json.dumps(value))

    async def acquire(self, operation):
        return await self.guest.call("acquire", {"operationId": operation, "agentId": "agent", "projectId": "project",
                                                 "linePath": str(self.line)}, "acquire")

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

    async def test_the_command_knows_the_layr_state_of_its_slot(self):
        result = await self.execute("state", 'import os; print("state=" + os.environ["HOST_EXEC_STATE"])')
        self.assertEqual(result["exitCode"], 0)
        # The fake guest layr names this state for the saved line (rev-parse HEAD@{0}).
        self.assertIn("state=0123456789abcdef0123456789abcdef", "".join(row.get("text", "") for row in self.events))

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

    async def test_retirement_requires_token_and_build_identity(self):
        params = {"buildId": self.host.build_id, "pid": os.getpid()}
        denied = await self.host.request({"id": "retire-auth", "method": "service.retire", "params": params, "token": "b" * 64})
        self.assertEqual(denied["error"]["code"], "forbidden")
        wrong = await self.host.request({"id": "retire-build", "method": "service.retire",
            "params": {**params, "buildId": "old"}, "token": "a" * 64})
        self.assertEqual(wrong["error"]["code"], "id_conflict")
        self.assertFalse(self.host.retiring)
        health = await self.host.request({"id": "health", "method": "health", "token": "a" * 64})
        self.assertEqual((health["result"]["buildId"], health["result"]["busy"]), (self.host.build_id, False))
        retired = await self.host.request({"id": "retire", "method": "service.retire", "params": params, "token": "a" * 64})
        self.assertEqual(retired["result"]["state"], "retiring")
        refused = await self.host.request({"id": "after-retire", "method": "status",
            "params": {"operationId": "x", "agentId": "agent"}, "token": "a" * 64})
        self.assertEqual(refused["error"]["code"], "busy")

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

    async def test_failed_sync_releases_even_when_forget_fails(self):
        self.host.slots = 1
        layr, io = self.slots.layr, self.slots.io
        async def failed_layr(context, args, ok=(0,)):
            if args[0] == "export":
                raise HostExecError("layr_failed", "Permission denied")
            return await layr(context, args, ok)
        async def failed_io(context, params):
            if params["action"] == "delete":
                raise HostExecError("operation_failed", "Permission denied")
            return await io(context, params)
        with patch.object(self.slots, "layr", side_effect=failed_layr), \
                patch.object(self.slots, "io", side_effect=failed_io):
            with self.assertRaises(HostExecError) as caught:
                await self.execute("sync-failed", 'open("must-not-run", "w").write("bad")')
        self.assertEqual(caught.exception.code, "layr_failed")
        self.assertIn("Mac slot released", str(caught.exception))
        old = self.host.load("sync-failed")
        self.assertEqual(old["state"], "released")
        self.assertEqual(old["reason"], "sync_failed")
        self.assertEqual(old["preRunError"]["code"], "layr_failed")
        self.assertFalse((Path(old["slotPath"]).parent / "lease.json").exists())
        staging = self.slots.root / key("project") / key("agent") / old["slotId"] / old["generation"]
        self.assertTrue((staging / "lease.json").exists())
        self.assertFalse((self.line / "must-not-run").exists())
        self.assertFalse(self.host.commands)
        # The only Mac slot is free again, with a new generation: the old staging is never reused.
        new = await self.execute("after-failure", 'print("next command")')
        self.assertEqual(new["slotPath"], old["slotPath"])
        self.assertNotEqual(new["generation"], old["generation"])
        self.assertEqual(new["exitCode"], 0)

    async def test_failed_transfer_releases_both_leases(self):
        call = self.guest.call
        async def failed(method, params, phase, *options):
            # The held manifest is a Mac transfer step before the run request.
            if method == "manifest":
                raise HostExecError("sync_conflict", "Checksum differs")
            return await call(method, params, phase, *options)
        with patch.object(self.guest, "call", side_effect=failed):
            with self.assertRaises(HostExecError) as caught:
                await self.execute("transfer-failed", 'open("must-not-run", "w").write("bad")')
        self.assertEqual(caught.exception.code, "sync_conflict")
        old = self.host.load("transfer-failed")
        staging = self.slots.root / key("project") / key("agent") / old["slotId"] / old["generation"]
        self.assertEqual(old["state"], "released")
        self.assertFalse((staging / "lease.json").exists())
        self.assertFalse((Path(old["slotPath"]).parent / "lease.json").exists())
        self.assertFalse((self.line / "must-not-run").exists())

    async def test_rejected_run_releases_both_leases_and_rotates_generation(self):
        self.host.slots = 1
        call = self.guest.call
        async def changed(method, params, phase, *options):
            if method == "run":
                slot = self.host.slot(self.host.load(params["operationId"]))
                (slot / "source" / "main.swift").write_text("changed after transfer")
            return await call(method, params, phase, *options)
        with patch.object(self.guest, "call", side_effect=changed):
            with self.assertRaises(HostExecError) as caught:
                await self.execute("run-rejected", 'open("must-not-run", "w").write("bad")')
        self.assertEqual(caught.exception.code, "sync_conflict")
        old = self.host.load("run-rejected")
        self.assertEqual(old["state"], "released")
        self.assertEqual(old["preRunError"]["code"], "sync_conflict")
        self.assertNotIn("startedAt", old)
        self.assertFalse(self.host.commands)
        self.assertFalse((Path(old["slotPath"]) / "must-not-run").exists())
        self.assertFalse((Path(old["slotPath"]).parent / "lease.json").exists())
        staging = self.slots.root / key("project") / key("agent") / old["slotId"] / old["generation"]
        self.assertFalse((staging / "lease.json").exists())
        new = await self.execute("after-rejected-run", 'print("next command")')
        self.assertEqual(new["slotPath"], old["slotPath"])
        self.assertNotEqual(new["generation"], old["generation"])
        self.assertEqual(new["exitCode"], 0)

    async def test_lost_run_response_keeps_lease(self):
        call = self.guest.call
        async def lost(method, params, phase, *options):
            result = await call(method, params, phase, *options)
            if method == "run":
                raise HostExecError("outcome_unknown", "Run response lost")
            return result
        with patch.object(self.guest, "call", side_effect=lost):
            with self.assertRaises(HostExecError) as caught:
                await self.execute("run-lost", 'import time;time.sleep(0.2)')
        self.assertEqual(caught.exception.code, "outcome_unknown")
        old = self.host.load("run-lost")
        self.assertEqual(old["state"], "running")
        self.assertTrue((Path(old["slotPath"]).parent / "lease.json").exists())

    def restarted_guest(self):
        """A new guest service process: a new owner with the same channels."""
        guest = HostExec(SimpleNamespace(state=self.state, host_context=self.guest.service.host_context))
        guest.call, guest.broker = self.guest.call, self.guest.broker
        return guest

    async def run_then_lose_guest(self, operation, script):
        """The guest sends the run request and dies before its response (2026-10-20 incident)."""
        call = self.guest.call
        async def lost(method, params, phase, *options):
            result = await call(method, params, phase, *options)
            if method == "run":
                raise HostExecError("outcome_unknown", "The VM stopped")
            return result
        with patch.object(self.guest, "call", side_effect=lost):
            with self.assertRaises(HostExecError):
                await self.execute(operation, script)

    async def test_a_guest_restart_frees_the_slot_of_an_exited_command(self):
        self.host.slots = 1
        await self.run_then_lose_guest("panic", 'open("out", "w").write("mac result")')
        await asyncio.gather(*list(self.host.commands.values()))
        old = self.host.load("panic")
        self.assertEqual(old["state"], "exited")
        self.assertTrue((Path(old["slotPath"]).parent / "lease.json").exists())
        # The same guest keeps the lease for inspection.
        with self.assertRaises(HostExecError) as caught:
            await self.execute("same-guest", 'print("busy")')
        self.assertEqual(caught.exception.code, "busy")
        guest = self.restarted_guest()
        new = await guest.handle("after-restart", {"agentId": "agent", "operationId": "after-restart",
            "argv": [sys.executable, "-c", 'print("next")'], "cwd": str(self.line), "timeoutSeconds": 5}, self.emit)
        self.assertEqual(new["exitCode"], 0)
        self.assertEqual(new["slotPath"], old["slotPath"])
        self.assertNotEqual(new["generation"], old["generation"])
        old = self.host.load("panic")
        self.assertEqual((old["state"], old["abandoned"], old["collected"]), ("released", "guest_owner_lost", False))
        # The result of the lost command stays readable.
        changes = await guest.call("result.manifest", {"operationId": "panic", "agentId": "agent", "area": "changes",
                                                      "after": ""}, "changes")
        self.assertIn("out", changes["entries"])
        self.assertFalse((self.line / "out").exists())

    async def test_a_running_command_of_a_lost_guest_keeps_its_slot_until_it_exits(self):
        self.host.slots = 1
        gate = self.root / "gate"
        await self.run_then_lose_guest("running", f'import os,time\nwhile not os.path.exists({str(gate)!r}): time.sleep(0.02)')
        guest = self.restarted_guest()
        listed = await guest.handle("list", {"agentId": "agent", "action": "list"}, self.emit)
        row = next(item for item in listed["operations"] if item["operationId"] == "running")
        self.assertEqual((row["state"], row["leased"], row["orphaned"]), ("running", True, True))
        # A running command is never released.
        with self.assertRaises(HostExecError) as caught:
            await guest.handle("release-running", {"agentId": "agent", "action": "release", "operationId": "running"}, self.emit)
        self.assertEqual(caught.exception.code, "invalid_params")
        with self.assertRaises(HostExecError) as caught:
            await guest.handle("while-running", {"agentId": "agent", "operationId": "while-running",
                "argv": [sys.executable, "-c", "print()"], "cwd": str(self.line), "timeoutSeconds": 5}, self.emit)
        self.assertEqual(caught.exception.code, "busy")
        self.assertTrue(self.host.children)
        gate.write_text("exit")
        await asyncio.gather(*list(self.host.commands.values()))
        old = self.host.load("running")
        self.assertEqual((old["state"], old["exitCode"], old["abandoned"]), ("released", 0, "guest_owner_lost"))
        new = await guest.handle("after-exit", {"agentId": "agent", "operationId": "after-exit",
            "argv": [sys.executable, "-c", 'print("next")'], "cwd": str(self.line), "timeoutSeconds": 5}, self.emit)
        self.assertEqual(new["exitCode"], 0)

    async def test_an_unstarted_command_of_a_lost_guest_never_starts(self):
        op = await self.guest.call("acquire", {"operationId": "starting", "agentId": "agent", "projectId": "project",
                                               "linePath": str(self.line), "owner": "old"}, "acquire")
        manifest = scan(Path(op["slotPath"]))
        lock = self.host.locks.setdefault("run\0starting", asyncio.Lock())
        async with lock:
            # The old guest's run request holds the start while the new guest recovers.
            report = await self.guest.call("recover", {"operationId": "guest-owner:new", "owner": "new"}, "recover")
            self.assertEqual(report["orphaned"], ["starting"])
            self.assertTrue((self.host.slot(op) / "lease.json").exists())
        with self.assertRaises(HostExecError) as caught:
            await self.guest.call("run", {"operationId": "starting", "agentId": "agent",
                "argv": [sys.executable, "-c", 'open("ran", "w").write("bad")'],
                "manifestSha256": key(json.dumps(manifest, sort_keys=True))}, "run")
        self.assertEqual(caught.exception.code, "guest_owner_lost")
        self.assertFalse(self.host.commands)
        self.assertFalse((Path(op["slotPath"]) / "ran").exists())
        self.assertFalse((Path(op["slotPath"]).parent / "lease.json").exists())
        self.assertEqual(self.host.load("starting")["abandoned"], "guest_owner_lost")
        # An unstarted operation without a held start is released at once; the owner's own stays.
        idle = await self.guest.call("acquire", {"operationId": "idle", "agentId": "agent", "projectId": "project",
                                                 "linePath": str(self.line), "owner": "old"}, "acquire")
        mine = await self.guest.call("acquire", {"operationId": "mine", "agentId": "agent", "projectId": "project",
                                                 "linePath": str(self.line), "owner": "new"}, "acquire")
        report = await self.guest.call("recover", {"operationId": "guest-owner:new", "owner": "new"}, "recover-2")
        self.assertEqual(report["released"], ["idle"])
        self.assertEqual(self.host.load("idle")["state"], "released")
        self.assertTrue((self.host.slot(mine) / "lease.json").exists())
        self.assertNotEqual(json.loads((Path(idle["slotPath"]).parent / "slot.json").read_text())["generation"],
                            idle["generation"])

    async def test_the_agent_lists_and_releases_its_own_operations(self):
        self.host.slots = 1
        broker = self.guest.broker
        async def failed(method, params, phase):
            if method == "host.slot.collect":
                raise HostExecError("layr_failed", "Collect failed")
            return await broker(method, params, phase)
        with patch.object(self.guest, "broker", side_effect=failed):
            with self.assertRaises(HostExecError):
                await self.execute("uncollected", 'open("out", "w").write("mac result")')
        self.assertTrue((Path(self.host.load("uncollected")["slotPath"]).parent / "lease.json").exists())
        listed = await self.guest.handle("list", {"agentId": "agent", "action": "list"}, self.emit)
        self.assertEqual([(row["operationId"], row["state"], row["leased"]) for row in listed["operations"]],
                         [("uncollected", "exited", True)])
        self.another_line("other", self.line)
        other = await self.guest.handle("list-other", {"agentId": "other", "action": "list"}, self.emit)
        self.assertEqual(other["operations"], [])
        with self.assertRaises(HostExecError) as caught:
            await self.guest.handle("release-other", {"agentId": "other", "action": "release", "operationId": "uncollected"}, self.emit)
        self.assertEqual(caught.exception.code, "forbidden")
        # A request of this guest that runs the operation keeps it.
        self.guest.live.add("uncollected")
        with self.assertRaises(HostExecError) as caught:
            await self.guest.handle("release-live", {"agentId": "agent", "action": "release", "operationId": "uncollected"}, self.emit)
        self.assertEqual(caught.exception.code, "busy")
        self.guest.live.discard("uncollected")
        released = await self.guest.handle("release", {"agentId": "agent", "action": "release", "operationId": "uncollected"}, self.emit)
        self.assertEqual((released["state"], released["abandoned"], released["collected"]), ("released", "agent_release", False))
        again = await self.guest.handle("release-again", {"agentId": "agent", "action": "release", "operationId": "uncollected"}, self.emit)
        self.assertEqual(again["state"], "released")
        changes = await self.guest.call("result.manifest", {"operationId": "uncollected", "agentId": "agent",
                                                           "area": "changes", "after": ""}, "changes")
        self.assertIn("out", changes["entries"])
        new = await self.execute("after-release", 'print("next")')
        self.assertEqual(new["exitCode"], 0)
        self.assertNotEqual(new["generation"], released["generation"])

    async def test_a_transfer_error_release_needs_an_error_object(self):
        op = await self.guest.call("acquire", {"operationId": "pre-run", "agentId": "agent", "projectId": "project",
                                               "linePath": str(self.line)}, "acquire")
        with self.assertRaises(HostExecError) as caught:
            await self.guest.call("release", {"operationId": "pre-run", "agentId": "agent", "preRunError": "text"},
                                  "release-text")
        self.assertEqual(caught.exception.code, "invalid_params")
        self.assertTrue((self.host.slot(op) / "lease.json").exists())

    async def test_timeout_collects_completed_edits(self):
        result = await self.execute("timeout", 'import time,pathlib;pathlib.Path("edit").write_text("before timeout");time.sleep(20)', timeout=0.1)
        self.assertEqual(result["reason"], "timeout")
        self.assertTrue(result["collected"])
        self.assertEqual((self.line / "edit").read_text(), "before timeout")

    async def test_cancel_retains_source_changes(self):
        edited = self.line / "edit"
        edit_started = self.line / "edit-started"
        script = (
            'import pathlib,threading;pathlib.Path("edit").write_text("before cancel");'
            f'pathlib.Path({str(edit_started)!r}).write_text("ready");threading.Event().wait()'
        )
        task = asyncio.create_task(self.execute("cancel", script))
        await asyncio.to_thread(wait_for_condition, lambda: bool(self.host.children), 'host exec child start')
        await asyncio.to_thread(
            wait_for_condition,
            lambda: edit_started.read_text() if edit_started.exists() else None,
            'host exec child edit before cancellation',
        )
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
        budget = self.host.size(slot_b) + 8192
        self.host.evict(slot_b.parent, budget, exclude=slot_b)
        self.assertFalse(slot_a.exists())
        self.assertTrue(slot_b.exists())
        with self.assertRaises(HostExecError):
            self.host.evict(slot_b.parent, budget, reserve=budget + 1)

    async def test_partial_slot_eviction_preserves_retirement_and_blocks_reuse(self):
        await self.acquire("retire-old")
        slot = self.host.slot(self.host.load("retire-old"))
        metadata = json.loads((slot / "slot.json").read_text())
        await self.guest.call("release", {"operationId": "retire-old", "agentId": "agent"}, "release")
        evidence = slot / "DerivedData" / "evidence.json"
        evidence.write_bytes(b"x" * 65536)
        self.host.budget = 8192

        def interrupted(victim):
            self.assertEqual(victim, slot)
            (victim / "slot.json").unlink()
            raise OSError(errno.ENOTEMPTY, "Directory not empty", str(victim))

        with patch.object(codex_host_exec_server.shutil, "rmtree", side_effect=interrupted):
            with self.assertRaises(HostExecError) as caught:
                await self.acquire("retire-failed")
        self.assertEqual(caught.exception.code, "slot_cleanup_failed")
        marker = slot.parent / (".retiring-" + slot.name + ".json")
        self.assertEqual(json.loads(marker.read_text())["metadata"]["generation"], metadata["generation"])
        self.assertTrue(evidence.exists())
        self.assertFalse(self.host.opdir("retire-failed").exists())
        # Even with spare capacity, preserve the failed retirement until explicit recovery.
        self.host.budget = 1024 * 1024
        for operation in ("retire-failed", "retire-followup"):
            with self.assertRaises(HostExecError) as retry:
                await self.acquire(operation)
            self.assertEqual(retry.exception.code, "slot_cleanup_failed")
        self.assertFalse(self.host.opdir("retire-followup").exists())
        self.assertTrue(evidence.exists())

    async def test_slot_count_shrink_records_retirement_before_deletion(self):
        for operation in ("shrink-a", "shrink-b"):
            await self.acquire(operation)
        slot = self.host.slot(self.host.load("shrink-a"))
        for operation in ("shrink-a", "shrink-b"):
            await self.guest.call("release", {"operationId": operation, "agentId": "agent"}, "release")
        self.project_settings({"slots": 1})

        def interrupted(victim):
            self.assertEqual(victim, slot)
            self.assertTrue((slot.parent / (".retiring-" + slot.name + ".json")).is_file())
            (slot / "slot.json").unlink()
            raise OSError(errno.ENOTEMPTY, "Directory not empty")

        with patch.object(codex_host_exec_server.shutil, "rmtree", side_effect=interrupted):
            with self.assertRaises(HostExecError) as caught:
                await self.acquire("shrink-c")
        self.assertEqual(caught.exception.code, "slot_cleanup_failed")
        self.assertFalse(self.host.opdir("shrink-c").exists())

    async def test_missing_legacy_slot_metadata_refuses_before_new_operation(self):
        await self.acquire("legacy-old")
        slot = self.host.slot(self.host.load("legacy-old"))
        await self.guest.call("release", {"operationId": "legacy-old", "agentId": "agent"}, "release")
        (slot / "slot.json").unlink()
        evidence = slot / "DerivedData" / "evidence.json"
        evidence.write_text("preserve")
        with self.assertRaises(HostExecError) as caught:
            await self.acquire("legacy-next")
        self.assertEqual(caught.exception.code, "slot_invalid")
        self.assertEqual(evidence.read_text(), "preserve")
        self.assertFalse(self.host.opdir("legacy-next").exists())

    async def test_output_alias_split_and_large_manifest(self):
        for index in range(270):
            (self.line / ("file-%03d" % index)).write_text("x")
        # Several manifest pages on both sides.
        self.enterContext(patch.object(host_exec_protocol, "PAGE_ENTRIES", 100))
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

    async def test_broker_error_keeps_its_code(self):
        from common import GuestError
        module = SimpleNamespace(admin_request=AsyncMock(side_effect=GuestError("lease_lost", "No lease")))
        with patch.dict(sys.modules, {"layr_admin_client": module}):
            with self.assertRaises(HostExecError) as caught:
                await HostExec.broker(self.guest, "host.slot.forget", {"operationId": "lost"}, "forget")
        self.assertEqual(caught.exception.code, "lease_lost")

    async def test_layr_runs_as_the_line_owner_and_accepts_listed_codes(self):
        names = "not written (names differ only in case or Unicode form; they cannot coexist on APFS):\n\tA\n\ta\n"
        child = SimpleNamespace(returncode=1, communicate=AsyncMock(return_value=(names.encode(), b"")))
        with patch("host_exec_slot.asyncio.create_subprocess_exec", return_value=child) as launch:
            report = await HostSlotHandlers.layr(self.slots, self.slots.agents.context,
                ["export", "abc", str(self.line), "--all", "--force"], ok=(0, 1))
        self.assertEqual(report, names)
        self.assertEqual(launch.call_args.kwargs["user"], os.getuid())
        self.assertEqual(launch.call_args.kwargs["group"], os.getgid())
        self.assertEqual(launch.call_args.kwargs["env"]["LAYR_SESSION"], "agent")
        child.communicate = AsyncMock(return_value=(b"other failure", b"error"))
        with patch("host_exec_slot.asyncio.create_subprocess_exec", return_value=child):
            with self.assertRaises(HostExecError) as caught:
                await HostSlotHandlers.layr(self.slots, self.slots.agents.context, ["save", "-q"])
        self.assertEqual(caught.exception.code, "layr_failed")

    async def test_the_export_marker_does_not_reach_the_mac(self):
        result = await self.execute("marker", 'print("ok")')
        self.assertTrue(result["collected"])
        slots = list((self.state / "host-slots").rglob(".layr-export.json"))
        self.assertEqual(slots, [])

    async def test_guest_result_reports_collection_conflicts(self):
        self.slots.collect_conflicts = ["main.swift (content: markers written)"]
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

    async def test_project_settings_set_the_slot_count_and_budget(self):
        self.project_settings({"slots": 1, "diskBudgetGiB": 300})
        first = await self.acquire("one")
        self.assertEqual((first["diskBudget"], first["macOnly"]), (300 * 1024**3, []))
        with self.assertRaises(HostExecError) as caught:
            await self.acquire("two")
        self.assertEqual(caught.exception.code, "busy")

    async def test_invalid_project_settings_refuse_the_command(self):
        for index, value in enumerate(({"slots": 0}, {"slots": True}, {"diskBudgetGiB": -1}, {"diskBudgetGiB": "300"},
                                       {"budget": 300}, {"macOnly": ["../out"]}, {"macOnly": "out"}, "{not json")):
            with self.subTest(value=value):
                self.project_settings(value)
                with self.assertRaises(HostExecError) as caught:
                    await self.acquire("invalid-%d" % index)
                self.assertEqual(caught.exception.code, "invalid_config")
                self.assertIn("host-exec.json", str(caught.exception))
                self.assertFalse(self.host.opdir("invalid-%d" % index).exists())

    async def test_a_smaller_slot_count_removes_idle_slots_oldest_first(self):
        for operation in ("a", "b"):
            await self.acquire(operation)
        for operation in ("a", "b"):
            await self.guest.call("release", {"operationId": operation, "agentId": "agent"}, "release")
        self.project_settings({"slots": 1})
        third = await self.acquire("c")
        self.assertEqual(third["slotId"], self.host.load("b")["slotId"])
        self.assertEqual(len(list((self.state / "host-slots" / key("project")).iterdir())), 1)

    async def test_source_transfer_walks_the_project_folder_rarely(self):
        for index in range(300):
            (self.line / ("file-%03d" % index)).write_text("x")
        with patch.object(self.host, "size", wraps=self.host.size) as size:
            result = await self.execute("walks", 'print("ok")')
        self.assertTrue(result["collected"])
        # One walk per lease step and budget check, not one for each of the 301 files.
        self.assertLess(size.call_count, 20)

    async def test_a_cache_folder_reaches_the_mac_and_build_output_stays_there(self):
        self.project_settings({"slots": 1, "diskBudgetGiB": 300, "macOnly": ["chromium/src/out"]})
        self.slots.caches = ["chromium"]
        source = self.line / "chromium" / "src"
        (source / ".git").mkdir(parents=True)
        (source / ".git" / "HEAD").write_text("ref: refs/heads/main\n")
        (source / "a.cc").write_text("int a = 1;\n")
        (source / "out" / "Linux").mkdir(parents=True)
        (source / "out" / "Linux" / "linux.o").write_bytes(b"linux")
        first = await self.execute("chromium-1", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert (s/"a.cc").read_text()=="int a = 1;\\n" and (s/".git/HEAD").exists();'
            'assert not (s/"out/Linux").exists();'
            '(s/"out/Default").mkdir(parents=True);(s/"out/Default/a.o").write_text("object 1");'
            '(s/"a.cc").write_text("mac edit");(s/"gen.h").write_text("generated");'
            'pathlib.Path("main.swift").write_text("host edit\\n")')
        self.assertEqual((first["exitCode"], first["diskBudget"]), (0, 300 * 1024**3))
        self.assertTrue(first["collected"])
        # Normal sources return; nothing inside the cache folder does.
        self.assertEqual((self.line / "main.swift").read_text(), "host edit\n")
        self.assertEqual((source / "a.cc").read_text(), "int a = 1;\n")
        self.assertFalse((source / "gen.h").exists())
        self.assertFalse((source / "out" / "Default").exists())

        # Another line of the project: the warm slot receives only the file that differs, and
        # the build output survives for an incremental build.
        line2 = self.root / "line2"
        shutil.copytree(self.line, line2, symlinks=True)
        (line2 / "chromium" / "src" / "a.cc").write_text("int a = 2;\n")
        self.another_line("agent2", line2)
        second = await self.execute("chromium-2", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert (s/"out/Default/a.o").read_text()=="object 1";'
            'assert (s/"a.cc").read_text()=="int a = 2;\\n";assert not (s/"gen.h").exists()', agent="agent2")
        self.assertEqual(second["exitCode"], 0)
        self.assertEqual(second["slotPath"], first["slotPath"])
        self.assertEqual(second["transferredBytes"], len("int a = 2;\n"))

        # A line without the cache folder: the slot follows it and keeps the Mac-only output.
        line3 = self.root / "line3"
        line3.mkdir()
        (line3 / "main.swift").write_text("host edit\n")
        self.another_line("agent3", line3)
        third = await self.execute("chromium-3", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert not (s/"a.cc").exists() and not (s/".git").exists();'
            'assert (s/"out/Default/a.o").read_text()=="object 1"', agent="agent3")
        self.assertEqual(third["exitCode"], 0)

    async def test_the_broker_never_collects_cache_or_mac_only_paths(self):
        self.slots.caches = ["chromium"]
        (self.line / "chromium").mkdir()
        slot = {"agentId": "agent", "slotId": "0", "generation": "0" * 32, "operationId": "filter"}
        prepared = await self.slots.dispatch("prepare", "host.slot.prepare", {**slot, "macOnly": ["keep"]})
        self.assertEqual(prepared["caches"], ["chromium"])
        await self.slots.dispatch("collect", "host.slot.collect",
                                  {**slot, "paths": ["chromium/x.cc", "keep/y", "main.swift"], "final": True})
        self.assertEqual(self.slots.collect_batches, [["main.swift"]])

    def mac_project(self, files):
        """The project folder on this Mac, as project.json names it."""
        folder = self.root / "mac-project"
        for name, data in files.items():
            (folder / name).parent.mkdir(parents=True, exist_ok=True)
            (folder / name).write_bytes(data)
        record = self.state / "layr-projects" / "project"
        record.mkdir(parents=True, exist_ok=True)
        (record / "project.json").write_text(json.dumps({"projectId": "project", "source": str(folder)}))
        return folder

    async def test_the_slot_gets_the_line_git_layer_with_the_project_objects(self):
        # 2026-10-14: a Mac step of the Attar lead ran `git rev-parse HEAD` in its slot, and the
        # slot had no .git. The line's git layer keeps its objects in the layr project store.
        import subprocess
        project = self.mac_project({"main.swift": b'print("Hello")\n'})
        git = ["git", "-c", "user.name=t", "-c", "user.email=t@example.test"]
        subprocess.run([*git, "init", "-q", str(project)], check=True)
        subprocess.run([*git, "-C", str(project), "add", "."], check=True)
        subprocess.run([*git, "-C", str(project), "commit", "-qm", "base"], check=True)
        head = subprocess.run(["git", "-C", str(project), "rev-parse", "HEAD"], check=True,
                              capture_output=True, text=True).stdout.strip()
        shutil.copytree(project / ".git", self.line / ".git", ignore=shutil.ignore_patterns("objects"))
        (self.line / ".git" / "objects" / "info").mkdir(parents=True)
        (self.line / ".git" / "objects" / "info" / "alternates").write_text("/var/lib/codex-studio/layr/projects/p/git/objects\n")
        before = sorted(p.relative_to(project / ".git").as_posix() for p in (project / ".git").rglob("*"))
        result = await self.execute("git-1", 'import subprocess as s;'
            f'assert s.run(["git","rev-parse","HEAD"],capture_output=True,text=True).stdout.strip()=="{head}";'
            'assert s.run(["git","cat-file","-t","HEAD"],capture_output=True,text=True).stdout.strip()=="commit";'
            'open("new.txt","w").write("x");'
            's.run(["git","-c","user.name=t","-c","user.email=t@example.test","add","new.txt"],check=True);'
            's.run(["git","-c","user.name=t","-c","user.email=t@example.test","commit","-qm","slot"],check=True)')
        self.assertEqual(result["exitCode"], 0, result)
        # The source file returns; git changes in the slot stay there, and the project repository
        # gets no new object or ref.
        self.assertEqual((self.line / "new.txt").read_text(), "x")
        self.assertEqual((self.line / ".git" / "objects" / "info" / "alternates").read_text(),
                         "/var/lib/codex-studio/layr/projects/p/git/objects\n")
        self.assertEqual(sorted(p.relative_to(project / ".git").as_posix() for p in (project / ".git").rglob("*")), before)

    @unittest.skipUnless(sys.platform == "darwin", "clonefile(2) exists only on macOS")
    async def test_a_new_slot_clones_the_cache_folder_and_sends_only_differences(self):
        files = {"chromium/src/big.bin": os.urandom(3 * 1024 * 1024), "chromium/src/a.cc": b"int a = 1;\n",
                 "chromium/src/.git/HEAD": b"ref: refs/heads/main\n"}
        project = self.mac_project({**files, "chromium/src/out/Default/user.o": b"user build"})
        self.project_settings({"slots": 1, "macOnly": ["chromium/src/out"]})
        self.slots.caches = ["chromium"]
        for name, data in files.items():
            (self.line / name).parent.mkdir(parents=True, exist_ok=True)
            (self.line / name).write_bytes(data)
        (self.line / "chromium/src/a.cc").write_text("int a = 2;\n")
        first = await self.execute("prime-1", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert (s/"a.cc").read_text()=="int a = 2;\\n";'
            'assert (s/"out/Default/user.o").read_text()=="user build";'
            '(s/"out/Default/a.o").write_text("slot build")')
        self.assertEqual(first["exitCode"], 0)
        self.assertEqual([(row["path"], row["primed"]) for row in first["primed"]], [("chromium", True)])
        # The clone has the same content, so only the changed file and main.swift travel.
        self.assertEqual(first["transferredBytes"], len("int a = 2;\n") + len('print("Hello")\n'))

        # A line without the cache folder leaves only Mac-only output in the slot ...
        empty = self.root / "empty-line"
        empty.mkdir()
        (empty / "main.swift").write_text('print("Hello")\n')
        self.another_line("empty", empty)
        await self.execute("prime-2", 'print("no cache")', agent="empty")
        (project / "chromium/src/out/Default/user.o").write_text("user build 2")
        # ... and the next line with the folder clones it again and keeps the slot's own output.
        again = await self.execute("prime-3", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert (s/"out/Default/a.o").read_text()=="slot build";assert (s/"out/Default/user.o").read_text()=="user build";'
            'assert (s/"big.bin").stat().st_size==3*1024*1024')
        self.assertEqual(again["exitCode"], 0)
        self.assertEqual([(row["path"], row["primed"]) for row in again["primed"]], [("chromium", True)])
        self.assertEqual(again["transferredBytes"], len("int a = 2;\n"))

    async def test_a_failed_clone_falls_back_to_the_transfer(self):
        files = {"chromium/a.cc": b"a" * 1000, "chromium/b.cc": b"b" * 2000}
        self.mac_project(files)
        self.slots.caches = ["chromium"]
        for name, data in files.items():
            (self.line / name).parent.mkdir(parents=True, exist_ok=True)
            (self.line / name).write_bytes(data)
        with patch.object(codex_host_exec_server, "clone_tree", side_effect=OSError(45, "Operation not supported")):
            result = await self.execute("no-clone", 'print("ok")')
        self.assertEqual(result["exitCode"], 0)
        self.assertFalse(result["primed"][0]["primed"])
        self.assertIn("clone failed", result["primed"][0]["reason"])
        self.assertEqual(result["transferredBytes"], 3000 + len('print("Hello")\n'))
        self.assertEqual(list((self.state / "host-slots").rglob("prime")), [])

    @unittest.skipUnless(sys.platform == "darwin", "clonefile(2) exists only on macOS")
    def test_clone_tree_copies_files_and_links_and_skips_special_files(self):
        origin = self.root / "origin"
        (origin / "a/b").mkdir(parents=True)
        (origin / "a/b/file").write_bytes(b"content")
        (origin / "a/b/file").chmod(0o751)
        (origin / "link").symlink_to("a/b/file")
        os.mkfifo(origin / "pipe")
        total = clone_tree(origin, self.root / "copy")
        self.assertEqual((self.root / "copy/a/b/file").read_bytes(), b"content")
        self.assertEqual((self.root / "copy/a/b/file").stat().st_mode & 0o777, 0o751)
        self.assertEqual(os.readlink(self.root / "copy/link"), "a/b/file")
        self.assertFalse((self.root / "copy/pipe").exists())
        self.assertEqual(total, (origin / "a/b/file").lstat().st_blocks * 512)

    def test_the_hash_cache_reads_only_changed_files(self):
        tree = self.root / "hashed"
        for index in range(20):
            (tree / "d").mkdir(parents=True, exist_ok=True)
            (tree / "d" / ("f%02d" % index)).write_text("x%02d" % index)
        hashes = {}
        self.enterContext(patch.object(host_exec_protocol, "RACY_NS", 0))
        digest = self.enterContext(patch.object(host_exec_protocol.hashlib, "file_digest",
                                                wraps=host_exec_protocol.hashlib.file_digest))
        first = scan(tree, hashes=hashes)
        self.assertEqual((digest.call_count, len(hashes)), (20, 20))
        digest.reset_mock()
        self.assertEqual(scan(tree, hashes=hashes), first)
        self.assertEqual(digest.call_count, 0)
        # The same size and new content: the stat key changes, so the file is read again.
        (tree / "d/f03").write_text("y03")
        os.utime(tree / "d/f05", ns=(1, 1))
        changed = scan(tree, hashes=hashes)
        self.assertEqual(digest.call_count, 2)
        self.assertNotEqual(changed["d/f03"]["sha256"], first["d/f03"]["sha256"])
        (tree / "d/f07").unlink()
        scan(tree, hashes=hashes)
        self.assertNotIn("d/f07", hashes)
        # A skipped folder keeps its cached hashes.
        scan(tree, ["d"], hashes)
        self.assertEqual(len(hashes), 19)

    def test_the_hash_cache_does_not_keep_a_file_that_just_changed(self):
        tree = self.root / "racy"
        tree.mkdir()
        (tree / "new").write_text("now")
        hashes = {}
        scan(tree, hashes=hashes)
        self.assertEqual(hashes, {})

    async def test_a_warm_command_does_not_hash_unchanged_cache_files_again(self):
        self.slots.caches = ["chromium"]
        for index in range(60):
            (self.line / "chromium" / ("f%02d.cc" % index)).parent.mkdir(parents=True, exist_ok=True)
            (self.line / "chromium" / ("f%02d.cc" % index)).write_text("int f%02d;\n" % index)
        self.enterContext(patch.object(host_exec_protocol, "RACY_NS", 0))
        await self.execute("hash-1", 'print("cold")')
        await asyncio.sleep(0.01)
        digest = self.enterContext(patch.object(host_exec_protocol.hashlib, "file_digest",
                                                wraps=host_exec_protocol.hashlib.file_digest))
        result = await self.execute("hash-2", 'print("warm")')
        self.assertEqual(result["exitCode"], 0)
        # Without the caches the guest and the Mac would read the 60 cache files three times.
        self.assertLess(digest.call_count, 10)

    async def test_a_mode_change_alone_changes_the_mode_of_the_slot_file(self):
        big = self.line / "pack.bin"
        big.write_bytes(os.urandom(600 * 1024))
        big.chmod(0o400)
        self.enterContext(patch.object(host_exec_protocol, "RACY_NS", 0))
        first = await self.execute("mode-1", 'import pathlib,stat;'
            'print(oct(stat.S_IMODE(pathlib.Path("pack.bin").stat().st_mode)), pathlib.Path("pack.bin").stat().st_ino)')
        self.assertEqual(first["exitCode"], 0)
        self.assertIn("0o400", "".join(row.get("text", "") for row in self.events))
        inode = "".join(row.get("text", "") for row in self.events).split()[-1]
        await asyncio.sleep(0.01)
        # The line has the same content with another mode, as after a seed upload.
        big.chmod(0o600)
        self.events.clear()
        self.calls.clear()
        digest = self.enterContext(patch.object(host_exec_protocol.hashlib, "file_digest",
                                                wraps=host_exec_protocol.hashlib.file_digest))
        second = await self.execute("mode-2", 'import pathlib,stat;'
            'print(oct(stat.S_IMODE(pathlib.Path("pack.bin").stat().st_mode)), pathlib.Path("pack.bin").stat().st_ino)')
        self.assertEqual(second["exitCode"], 0)
        self.assertEqual(second["transferredBytes"], 0)
        self.assertIn("sync.mode", self.calls)
        self.assertNotIn("sync.chunk", self.calls)
        output = "".join(row.get("text", "") for row in self.events).split()
        # The same file with the new mode; the run check did not read it again.
        self.assertEqual(output, ["0o600", inode])
        self.assertLessEqual(digest.call_count, 3)

    async def test_a_mode_change_on_the_mac_returns_without_the_content(self):
        tool = self.line / "tool.sh"
        tool.write_bytes(b"#!/bin/sh\n" + b"x" * (700 * 1024))
        tool.chmod(0o644)
        result = await self.execute("mac-chmod", 'import pathlib;pathlib.Path("tool.sh").chmod(0o755)')
        self.assertTrue(result["collected"])
        self.assertEqual(self.applied, ["mode"])
        self.assertEqual(tool.stat().st_mode & 0o777, 0o755)

    async def test_a_mode_change_refuses_a_slot_file_with_other_content(self):
        await self.execute("mode-content", 'print("slot has main.swift")')
        await self.acquire("mode-refused")
        for name in ("main.swift", "missing.swift"):
            with self.subTest(path=name), self.assertRaises(HostExecError) as caught:
                await self.guest.call("sync.mode", {"operationId": "mode-refused", "agentId": "agent", "items": [
                    {"path": name, "entry": {"kind": "file", "bytes": 15, "mode": 0o600, "sha256": "0" * 64}}]}, "mode:" + name)
            self.assertEqual(caught.exception.code, "sync_conflict")

    @unittest.skipUnless(sys.platform == "darwin", "clonefile(2) exists only on macOS")
    async def test_a_large_file_is_cloned_from_the_project_folder_instead_of_sent(self):
        same, other = os.urandom(700 * 1024), os.urandom(400 * 1024)
        self.mac_project({"assets/big.bin": same, "assets/other.bin": os.urandom(400 * 1024)})
        (self.line / "assets").mkdir()
        (self.line / "assets/big.bin").write_bytes(same)
        (self.line / "assets/big.bin").chmod(0o600)
        (self.line / "assets/other.bin").write_bytes(other)
        self.enterContext(patch.object(host_exec_protocol, "RACY_NS", 0))
        await asyncio.sleep(0.01)
        read = []
        real = host_exec_protocol.hashlib.file_digest
        def recording(stream, digest):
            read.append(Path(stream.name).name)
            return real(stream, digest)
        self.enterContext(patch.object(host_exec_protocol.hashlib, "file_digest", side_effect=recording))
        result = await self.execute("clone-file", 'import pathlib,stat;p=pathlib.Path("assets/big.bin");'
            'assert p.read_bytes()==pathlib.Path(%r).read_bytes();assert stat.S_IMODE(p.stat().st_mode)==0o600'
            % str(self.line / "assets/big.bin"))
        self.assertEqual(result["exitCode"], 0)
        # The same file is a clone; the file with other content of the same size travels.
        self.assertEqual(result["clonedBytes"], len(same))
        self.assertEqual(result["transferredBytes"], len(other) + len('print("Hello")\n'))
        # The guest scan and the check of the project file read it; the run check does not.
        self.assertEqual(read.count("big.bin"), 2)

    async def test_small_files_travel_in_batches_below_the_frame_limit(self):
        for index in range(300):
            (self.line / "small" / ("f%03d" % index)).parent.mkdir(parents=True, exist_ok=True)
            (self.line / "small" / ("f%03d" % index)).write_text("x" * index)
        for index in range(12):
            (self.line / "medium" / ("m%02d" % index)).parent.mkdir(parents=True, exist_ok=True)
            (self.line / "medium" / ("m%02d" % index)).write_bytes(os.urandom(250 * 1024))
        (self.line / "large.bin").write_bytes(os.urandom(600 * 1024))
        result = await self.execute("batched", 'import pathlib;assert len(list(pathlib.Path("small").iterdir()))==300;'
            'assert pathlib.Path("large.bin").stat().st_size==600*1024')
        self.assertEqual(result["exitCode"], 0)
        sync = [method for method in self.calls if method.startswith("sync.")]
        # 316 entries: a few batches, and the chunks of the one large file.
        self.assertLess(len(sync), 20)
        self.assertGreater(sync.count("sync.batch"), 1)

    def test_scan_skips_paths_and_pages_follow_the_cursor(self):
        tree = self.root / "tree"
        for name in ("a/b/out/x.o", "a/b/c.cc", "z.txt"):
            (tree / name).parent.mkdir(parents=True, exist_ok=True)
            (tree / name).write_text(name)
        entries = scan(tree, ["a/b/out"])
        self.assertEqual(sorted(entries), ["a", "a/b", "a/b/c.cc", "z.txt"])
        items = sorted(scan(tree).items())
        found, after = {}, ""
        with patch.object(host_exec_protocol, "PAGE_ENTRIES", 2):
            while True:
                result = page(items, after)
                found.update(result["entries"])
                if result["next"] is None:
                    break
                after = result["next"]
        self.assertEqual(found, dict(items))

    def test_name_and_alias_rules(self):
        self.assertTrue(conflicts(["README", "readme"]))
        self.assertTrue(conflicts(["café", "cafe\u0301"]))
        self.assertTrue(conflicts(["Folder/a", "folder/b"]))
        self.assertEqual(mapped("/tmp/slot/source/file /private/tmp/slot/source/file", "/private/tmp/slot/source", "/line"), "/line/file /line/file")

    def test_live_runner_accepts_final_stdout(self):
        spec = importlib.util.spec_from_file_location("host_native_proof", SERVER_TESTS_ROOT / "host-exec-native.py")
        runner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(runner)
        result = {"exitCode": 0, "stdout": base64.b64encode(b"SOURCE_AND_ARTIFACT_OK\n").decode(), "stderr": ""}
        client = SimpleNamespace(stream=lambda *args, **kwargs: iter([{"result": result}]))
        actual, output = runner.frames(client, "layr.exec", {}, "proof")
        self.assertEqual(output, "SOURCE_AND_ARTIFACT_OK\n")
        streamed = SimpleNamespace(stream=lambda *args, **kwargs: iter([
            {"event": "output", "data": {"text": "once"}}, {"result": {**result, "stdout": base64.b64encode(b"once").decode()}}]))
        actual, output = runner.frames(streamed, "host.exec", {}, "proof")
        self.assertEqual(output, "once")

    async def test_slot_parents_under_private_umask(self):
        # The guest units run with UMask=0077; every slot parent must stay 0711.
        private = self.root / "private-broker"
        private.mkdir()
        previous = os.umask(0o077)
        try:
            slots = Slots(self.state, private / "layr", self.slots.agents)
            # The fixture settings of the shared Slots (bad_names and the others).
            for name, value in vars(self.slots).items():
                vars(slots).setdefault(name, value)
            params = {"agentId": "agent", "operationId": "private-umask", "slotId": "0", "generation": "f" * 32}
            prepared = await slots.dispatch("private-umask", "host.slot.prepare", params)
            # The export fixture copies the line folder, so only the owner of the source is checked.
            source = Path(prepared["path"])
            self.assertEqual(source.stat().st_uid, os.getuid())
            parent = source.parent
            while True:
                self.assertEqual(parent.stat().st_mode & 0o777, 0o711, str(parent))
                if parent == slots.root:
                    break
                parent = parent.parent
            slots.root.chmod(0o700)
            Slots(self.state, private / "layr", self.slots.agents)
            self.assertEqual(slots.root.stat().st_mode & 0o777, 0o711)
        finally:
            os.umask(previous)

    def test_slot_parent_rejects_symlink(self):
        private = self.root / "symlink-broker"
        private.mkdir()
        target = self.root / "private-target"
        target.mkdir(mode=0o700)
        (private / "host-slots").symlink_to(target, target_is_directory=True)
        with self.assertRaises(HostExecError) as caught:
            Slots(self.state, private / "layr", self.slots.agents)
        self.assertEqual(caught.exception.code, "forbidden")
        self.assertEqual(target.stat().st_mode & 0o777, 0o700)

    def test_vm_gate(self):
        runtime = SimpleNamespace(agent=lambda actor: {"executionMode": "native"})
        with self.assertRaises(PermissionError):
            run_tool(runtime, {"id": "agent"}, {"action": "execute", "command": "true"}, "tool")
        self.assertEqual(tool_definition()["name"], "host_exec")

    def test_reviewer_gate(self):
        runtime = SimpleNamespace(agent=lambda actor: {"executionMode": "vm"})
        with patch("codex_linux_workspaces.client") as client, patch("codex_host_exec.ensure_service") as ensured:
            for action in ("execute", "status", "cancel"):
                with self.subTest(action=action), self.assertRaises(PermissionError):
                    run_tool(runtime, {"id": "agent", "role": "reviewer"},
                             {"action": action, "command": "true", "operation_id": "existing"}, "tool")
            client.assert_not_called()
            for role in ("implementer", "orchestrator"):
                client.return_value.stream.return_value = iter([{"result": {"state": "done"}}])
                result = run_tool(runtime, {"id": "agent", "cwd": "/line", "role": role},
                                  {"action": "execute", "command": "true"}, "tool")
                self.assertEqual(result["state"], "done")
            # Each command first checks the Mac service: a VM start was the only check before.
            ensured.assert_called_with(client.return_value.state_dir)

    def test_a_lost_result_names_its_operation_and_stays_uncertain(self):
        from codex_linux_vm import LinuxVMError
        runtime = SimpleNamespace(agent=lambda actor: {"executionMode": "vm"})
        actor = {"id": "agent", "cwd": "/line", "role": "implementer"}
        with patch("codex_linux_workspaces.client") as client, patch("codex_host_exec.ensure_service"):
            def closed(*args, **kwargs):
                raise LinuxVMError("The Linux VM connection closed before its response.", uncertain=True)
                yield
            client.return_value.stream.side_effect = closed
            with self.assertRaises(LinuxVMError) as caught:
                run_tool(runtime, actor, {"action": "execute", "command": "true", "request_id": "build"}, "tool")
            operation = client.return_value.stream.call_args.args[1]["operationId"]
            self.assertIn("Operation: " + operation, str(caught.exception))
            self.assertTrue(caught.exception.uncertain)
            # The agent lists its operations without an ID.
            client.return_value.stream.side_effect = None
            client.return_value.stream.return_value = iter([{"result": {"operations": []}}])
            listed = run_tool(runtime, actor, {"action": "list"}, "tool-list")
            self.assertEqual(listed["operations"], [])
            self.assertEqual(client.return_value.stream.call_args.args[1], {"agentId": "agent", "action": "list"})
        for action in ("status", "cancel", "release"):
            with self.subTest(action=action), self.assertRaises(ValueError):
                with patch("codex_linux_workspaces.client"):
                    run_tool(runtime, actor, {"action": action}, "tool")
        self.assertEqual(tool_definition()["inputSchema"]["properties"]["action"]["enum"],
                         ["execute", "status", "cancel", "list", "release"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
