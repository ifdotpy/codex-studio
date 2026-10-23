#!/usr/bin/env python3
"""The guest host_exec broker against the Mac service process.

The guest code (host_exec.py, host_exec_slot.py) runs unchanged; only the layr binary and the line
merge are fixtures. Each request goes over host-exec.sock to `studio-runtime-server host-exec-service`.
The Rust witness `host_exec_service::the_guest_broker_runs_against_the_mac_service` runs this file
with HOST_EXEC_SERVICE_BINARY set to that executable.
"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
from host_exec import HostExec  # noqa: E402
import host_exec_protocol  # noqa: E402
from host_exec_protocol import FRAME_LIMIT, HostExecError, key, scan  # noqa: E402
from host_exec_slot import HostSlotHandlers  # noqa: E402

BINARY = os.environ.get("HOST_EXEC_SERVICE_BINARY", "")
TOKEN = "a" * 64
# The fake guest layr: the saved line state and the git tree ID that `rev-parse <state>^{tree}` prints.
FIXTURE_STATE = "0123456789abcdef0123456789abcdef"
FIXTURE_TREE = "5e2352431d25bc530ae038e5958b5c3ec3574d26"
# Only guards against a hang; a passing run never waits this long.
HANG_GUARD_SECONDS = 60


class Agents:
    def __init__(self, context):
        self.context = context
        self.contexts = {context["agentId"]: context}

    async def dispatch(self, request_id, method, params, emit=None):
        if params["agentId"] not in self.contexts:
            raise HostExecError("forbidden", "Unknown agent")
        return self.contexts[params["agentId"]]


class Slots(HostSlotHandlers):
    # Fixture for the external layr binary and the line merge.
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
            if args[1].endswith("^{tree}"):
                return FIXTURE_TREE + "\n"
            return FIXTURE_STATE
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


class MacService:
    """One service process for the VM state folder of a test."""

    def __init__(self, state: Path):
        self.state = state
        self.socket = state / "host-exec.sock"
        self.log_path = state.parent / "service.log"
        log = self.log_path.open("ab")
        self.process = subprocess.Popen([BINARY, "host-exec-service", str(state), "--build", "contract"],
                                        stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        log.close()
        deadline = time.monotonic() + HANG_GUARD_SECONDS
        while True:
            try:
                if "result" in self.call_sync("health"):
                    return
            except OSError:
                pass
            if self.process.poll() is not None or time.monotonic() > deadline:
                raise AssertionError("The Mac service did not start: " + self.log_path.read_text())
            time.sleep(0.02)

    def call_sync(self, method, params=None):
        request = {"id": "fixture:" + method + ":" + str(time.monotonic_ns()), "method": method,
                   "params": params or {}, "token": TOKEN}
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as channel:
            channel.settimeout(10)
            channel.connect(str(self.socket))
            channel.sendall(json.dumps(request).encode() + b"\n")
            with channel.makefile("rb") as stream:
                return json.loads(stream.readline())

    async def request(self, request):
        reader, writer = await asyncio.open_unix_connection(str(self.socket), limit=FRAME_LIMIT + 1)
        try:
            writer.write(json.dumps(request).encode() + b"\n")
            await writer.drain()
            line = await reader.readline()
        finally:
            writer.close()
        if not line:
            raise HostExecError("outcome_unknown", "The Mac service sent no answer")
        return json.loads(line)

    def status(self, operation):
        return json.loads((self.state / "host-operations" / key(operation) / "status.json").read_text())

    def running(self):
        return [op for op in self.operations() if op.get("state") == "running"]

    def operations(self):
        found = []
        for path in (self.state / "host-operations").glob("*/status.json"):
            try:
                found.append(json.loads(path.read_text()))
            except (OSError, ValueError):
                continue
        return found

    def wait_idle(self):
        deadline = time.monotonic() + HANG_GUARD_SECONDS
        while self.call_sync("health")["result"]["activeCommands"]:
            if time.monotonic() > deadline:
                raise AssertionError("A Mac command did not end")
            time.sleep(0.02)

    def stop(self):
        try:
            for op in self.running():
                self.call_sync("cancel", {"operationId": op["operationId"], "agentId": op["agentId"]})
            self.wait_idle()
            health = self.call_sync("health")["result"]
            self.call_sync("service.retire", {"buildId": health["buildId"], "pid": health["pid"]})
            self.process.wait(timeout=HANG_GUARD_SECONDS)
        finally:
            if self.process.poll() is None:
                for op in self.operations():
                    if op.get("pid"):
                        try:
                            os.killpg(op["pid"], signal.SIGKILL)
                        except (ProcessLookupError, PermissionError):
                            pass
                self.process.kill()
                self.process.wait(timeout=HANG_GUARD_SECONDS)


class Contract(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.assertTrue(BINARY, "Set HOST_EXEC_SERVICE_BINARY to the studio-runtime-server executable")
        self.temp = tempfile.TemporaryDirectory(prefix="host-exec-mac-")
        self.root = Path(self.temp.name).resolve()
        self.state = self.root / "state"
        self.state.mkdir()
        (self.state / "host-exec-token").write_text(TOKEN)
        self.project_settings({"slots": 2})
        self.host = MacService(self.state)
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
        self.guest.configure({"token": TOKEN})
        self.calls = []

        async def call(method, params, phase, timeout=300):
            request = {"id": key(params["operationId"] + ":" + phase), "method": method, "params": params, "token": TOKEN}
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
            return await self.slots.dispatch(key(params["operationId"] + phase), method, params)
        self.guest.broker = broker
        self.events = []

        async def emit(event, value):
            self.events.append(value)
        self.emit = emit

    async def asyncTearDown(self):
        await asyncio.to_thread(self.host.stop)
        self.temp.cleanup()

    def output(self):
        return "".join(row.get("text", "") for row in self.events)

    async def execute(self, operation, script, timeout=HANG_GUARD_SECONDS, agent="agent", guest=None):
        cwd = self.slots.agents.contexts[agent]["path"]
        return await (guest or self.guest).handle(operation, {"agentId": agent, "operationId": operation,
            "argv": [sys.executable, "-c", script], "cwd": cwd, "timeoutSeconds": timeout}, self.emit)

    def another_line(self, agent, path):
        self.slots.agents.contexts[agent] = {**self.slots.agents.context, "agentId": agent, "line": agent,
                                            "path": str(path), "cwd": str(path)}

    def project_settings(self, value):
        folder = self.state / "layr-projects" / "project"
        folder.mkdir(parents=True, exist_ok=True)
        (folder / "host-exec.json").write_text(json.dumps(value))

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

    async def acquire(self, operation):
        return await self.guest.call("acquire", {"operationId": operation, "agentId": "agent", "projectId": "project",
                                                 "linePath": str(self.line)}, "acquire")

    def restarted_guest(self):
        """A new guest service process: a new owner with the same channels."""
        guest = HostExec(SimpleNamespace(state=self.state, host_context=self.guest.service.host_context))
        guest.call, guest.broker = self.guest.call, self.guest.broker
        return guest

    async def run_then_lose_guest(self, operation, script):
        """The guest sends the run request and dies before its response."""
        call = self.guest.call

        async def lost(method, params, phase, *options):
            result = await call(method, params, phase, *options)
            if method == "run":
                raise HostExecError("outcome_unknown", "The VM stopped")
            return result
        with patch.object(self.guest, "call", side_effect=lost):
            with self.assertRaises(HostExecError):
                await self.execute(operation, script)

    async def test_source_return_artifact_file_and_warm_path(self):
        first = await self.execute("first", 'import os,pathlib; p=pathlib.Path("main.swift");p.write_text("host edit\\n");'
            'pathlib.Path(os.environ["HOST_EXEC_ARTIFACTS"],"result.txt").write_text("artifact");print(p.resolve())')
        self.assertEqual(first["exitCode"], 0, first)
        self.assertTrue(first["collected"])
        self.assertEqual((self.line / "main.swift").read_text(), "host edit\n")
        self.assertEqual(Path(first["artifacts"][0]["path"]).read_text(), "artifact")
        self.assertNotIn(first["slotPath"], self.output())
        self.assertIn(str(self.line), self.output())
        second = await self.execute("second", 'print("warm")')
        self.assertEqual(first["slotPath"], second["slotPath"])
        self.assertEqual(first["derivedData"], second["derivedData"])
        self.assertEqual(second["transferredBytes"], 0)

    async def test_the_command_knows_the_layr_state_and_tree_of_its_slot(self):
        result = await self.execute("tree", 'import os; print("state=" + os.environ["HOST_EXEC_STATE"]);'
                                            'print("tree=" + os.environ.get("HOST_EXEC_TREE", "none"))')
        self.assertEqual(result["exitCode"], 0, result)
        self.assertIn("state=" + FIXTURE_STATE, self.output())
        self.assertIn("tree=" + FIXTURE_TREE, self.output())
        self.assertEqual(result["layrTree"], FIXTURE_TREE)

    async def test_collision_report_refuses_command(self):
        self.slots.bad_names = True
        result = await self.execute("names", 'open("ran", "w").write("bad")')
        self.assertEqual(result["reason"], "name_conflict")
        self.assertFalse((self.line / "ran").exists())
        self.assertFalse((Path(result["slotPath"]) / "ran").exists())

    async def test_lost_run_response_keeps_lease(self):
        await self.run_then_lose_guest("run-lost", 'import time;time.sleep(0.5)')
        old = self.host.status("run-lost")
        self.assertIn(old["state"], ("running", "exited"))
        self.assertTrue((Path(old["slotPath"]).parent / "lease.json").exists())

    async def test_a_guest_restart_frees_the_slot_of_an_exited_command(self):
        self.project_settings({"slots": 1})
        await self.run_then_lose_guest("panic", 'open("out", "w").write("mac result")')
        await asyncio.to_thread(self.host.wait_idle)
        old = self.host.status("panic")
        self.assertEqual(old["state"], "exited")
        with self.assertRaises(HostExecError) as caught:
            await self.execute("same-guest", 'print("busy")')
        self.assertEqual(caught.exception.code, "busy")
        guest = self.restarted_guest()
        new = await self.execute("after-restart", 'print("next")', guest=guest)
        self.assertEqual(new["exitCode"], 0)
        self.assertEqual(new["slotPath"], old["slotPath"])
        self.assertNotEqual(new["generation"], old["generation"])
        old = self.host.status("panic")
        self.assertEqual((old["state"], old["abandoned"], old["collected"]), ("released", "guest_owner_lost", False))
        changes = await guest.call("result.manifest", {"operationId": "panic", "agentId": "agent", "area": "changes",
                                                      "after": ""}, "changes")
        self.assertIn("out", changes["entries"])
        self.assertFalse((self.line / "out").exists())

    async def test_the_agent_lists_and_releases_its_own_operations(self):
        self.project_settings({"slots": 1})
        broker = self.guest.broker

        async def failed(method, params, phase):
            if method == "host.slot.collect":
                raise HostExecError("layr_failed", "Collect failed")
            return await broker(method, params, phase)
        with patch.object(self.guest, "broker", side_effect=failed):
            with self.assertRaises(HostExecError):
                await self.execute("uncollected", 'open("out", "w").write("mac result")')
        listed = await self.guest.handle("list", {"agentId": "agent", "action": "list"}, self.emit)
        self.assertEqual([(row["operationId"], row["state"], row["leased"]) for row in listed["operations"]],
                         [("uncollected", "exited", True)])
        self.another_line("other", self.line)
        with self.assertRaises(HostExecError) as caught:
            await self.guest.handle("release-other", {"agentId": "other", "action": "release", "operationId": "uncollected"}, self.emit)
        self.assertEqual(caught.exception.code, "forbidden")
        released = await self.guest.handle("release", {"agentId": "agent", "action": "release", "operationId": "uncollected"}, self.emit)
        self.assertEqual((released["state"], released["abandoned"], released["collected"]), ("released", "agent_release", False))
        new = await self.execute("after-release", 'print("next")')
        self.assertEqual(new["exitCode"], 0)
        self.assertNotEqual(new["generation"], released["generation"])

    async def test_timeout_and_cancel_keep_completed_edits(self):
        result = await self.execute("timeout", 'import time,pathlib;pathlib.Path("edit").write_text("before timeout");time.sleep(20)', timeout=2)
        self.assertEqual(result["reason"], "timeout")
        self.assertTrue(result["collected"])
        self.assertEqual((self.line / "edit").read_text(), "before timeout")
        started = self.root / "edit-started"
        task = asyncio.create_task(self.execute("cancel", 'import pathlib,threading;pathlib.Path("edit").write_text("before cancel");'
                                                f'pathlib.Path({str(started)!r}).write_text("ready");threading.Event().wait()'))
        deadline = time.monotonic() + HANG_GUARD_SECONDS
        while not started.exists():
            self.assertLess(time.monotonic(), deadline)
            await asyncio.sleep(0.02)
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
        self.assertTrue(result["collected"], result)
        self.assertEqual(tool.stat().st_mode & 0o777, 0o644)
        self.assertTrue((self.line / "link").is_symlink())
        # A mode change on the Mac returns without the content.
        self.assertEqual(self.applied, ["mode"])

    async def test_output_alias_split_and_large_manifest(self):
        for index in range(270):
            (self.line / ("file-%03d" % index)).write_text("x")
        # Several manifest pages on the guest side.
        with patch.object(host_exec_protocol, "PAGE_ENTRIES", 100):
            result = await self.execute("paged", 'import os,pathlib,time;value=str(pathlib.Path("main.swift").resolve());'
                'value=value.replace("/private/tmp/","/tmp/").replace("/private/var/","/var/");data=value.encode();'
                'os.write(1,data[:len(data)//2]);time.sleep(0.05);os.write(1,data[len(data)//2:])')
        self.assertEqual(self.output(), str(self.line / "main.swift"))
        self.assertTrue(result["collected"])

    async def test_disk_budget_stops_cache_growth(self):
        self.project_settings({"slots": 2, "diskBudgetGiB": 64 / (1024 * 1024)})
        result = await self.execute("budget", 'import os,pathlib,time;'
            'pathlib.Path(os.environ["HOST_EXEC_DERIVED_DATA"],"large").write_bytes(b"x"*128*1024);time.sleep(20)')
        self.assertEqual(result["reason"], "disk_budget")
        self.assertTrue(result["collected"])

    async def test_a_cache_folder_reaches_the_mac_and_build_output_stays_there(self):
        self.project_settings({"slots": 1, "diskBudgetGiB": 300, "macOnly": ["chromium/src/out"]})
        self.slots.caches = ["chromium"]
        source = self.line / "chromium" / "src"
        (source / ".git").mkdir(parents=True)
        (source / ".git" / "HEAD").write_text("ref: refs/heads/main\n")
        (source / "a.cc").write_text("int a = 1;\n")
        first = await self.execute("chromium-1", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert (s/"a.cc").read_text()=="int a = 1;\\n" and (s/".git/HEAD").exists();'
            '(s/"out/Default").mkdir(parents=True);(s/"out/Default/a.o").write_text("object 1");'
            '(s/"a.cc").write_text("mac edit");(s/"gen.h").write_text("generated");'
            'pathlib.Path("main.swift").write_text("host edit\\n")')
        self.assertEqual((first["exitCode"], first["diskBudget"]), (0, 300 * 1024**3), first)
        self.assertEqual((self.line / "main.swift").read_text(), "host edit\n")
        self.assertEqual((source / "a.cc").read_text(), "int a = 1;\n")
        self.assertFalse((source / "gen.h").exists())
        line2 = self.root / "line2"
        shutil.copytree(self.line, line2, symlinks=True)
        (line2 / "chromium" / "src" / "a.cc").write_text("int a = 2;\n")
        self.another_line("agent2", line2)
        second = await self.execute("chromium-2", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert (s/"out/Default/a.o").read_text()=="object 1";'
            'assert (s/"a.cc").read_text()=="int a = 2;\\n";assert not (s/"gen.h").exists()', agent="agent2")
        self.assertEqual(second["exitCode"], 0, second)
        self.assertEqual(second["slotPath"], first["slotPath"])
        self.assertEqual(second["transferredBytes"], len("int a = 2;\n"))

    async def test_the_slot_gets_the_line_git_layer_with_the_project_objects(self):
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
        self.slots.caches = [".git"]
        result = await self.execute("git-1", 'import subprocess as s;'
            f'assert s.run(["git","rev-parse","HEAD"],capture_output=True,text=True).stdout.strip()=="{head}";'
            'assert s.run(["git","cat-file","-t","HEAD"],capture_output=True,text=True).stdout.strip()=="commit";'
            'open("new.txt","w").write("x")')
        self.assertEqual(result["exitCode"], 0, result)
        self.assertEqual((self.line / "new.txt").read_text(), "x")

    @unittest.skipUnless(sys.platform == "darwin", "clonefile(2) exists only on macOS")
    async def test_a_new_slot_clones_the_cache_folder_and_a_large_file(self):
        files = {"chromium/src/big.bin": os.urandom(3 * 1024 * 1024), "chromium/src/a.cc": b"int a = 1;\n"}
        self.mac_project({**files, "chromium/src/out/Default/user.o": b"user build", "assets/big.bin": files["chromium/src/big.bin"]})
        self.project_settings({"slots": 1, "macOnly": ["chromium/src/out"]})
        self.slots.caches = ["chromium"]
        for name, data in files.items():
            (self.line / name).parent.mkdir(parents=True, exist_ok=True)
            (self.line / name).write_bytes(data)
        (self.line / "chromium/src/a.cc").write_text("int a = 2;\n")
        (self.line / "assets").mkdir()
        (self.line / "assets/big.bin").write_bytes(files["chromium/src/big.bin"])
        first = await self.execute("prime-1", 'import pathlib;s=pathlib.Path("chromium/src");'
            'assert (s/"a.cc").read_text()=="int a = 2;\\n";'
            'assert (s/"out/Default/user.o").read_text()=="user build";'
            'assert pathlib.Path("assets/big.bin").stat().st_size==3*1024*1024')
        self.assertEqual(first["exitCode"], 0, first)
        self.assertEqual([(row["path"], row["primed"]) for row in first["primed"]], [("chromium", True)])
        # The cache clone and the file clone have the same content: only the changed file and main.swift travel.
        self.assertEqual(first["clonedBytes"], 3 * 1024 * 1024)
        self.assertEqual(first["transferredBytes"], len("int a = 2;\n") + len('print("Hello")\n'))

    async def test_a_mode_change_alone_changes_the_mode_of_the_slot_file(self):
        big = self.line / "pack.bin"
        big.write_bytes(os.urandom(600 * 1024))
        big.chmod(0o400)
        first = await self.execute("mode-1", 'import pathlib,stat;'
            'print(oct(stat.S_IMODE(pathlib.Path("pack.bin").stat().st_mode)), pathlib.Path("pack.bin").stat().st_ino)')
        self.assertEqual(first["exitCode"], 0)
        self.assertIn("0o400", self.output())
        inode = self.output().split()[-1]
        big.chmod(0o600)
        self.events.clear()
        self.calls.clear()
        second = await self.execute("mode-2", 'import pathlib,stat;'
            'print(oct(stat.S_IMODE(pathlib.Path("pack.bin").stat().st_mode)), pathlib.Path("pack.bin").stat().st_ino)')
        self.assertEqual(second["exitCode"], 0)
        self.assertEqual(second["transferredBytes"], 0)
        self.assertIn("sync.mode", self.calls)
        self.assertNotIn("sync.chunk", self.calls)
        self.assertEqual(self.output().split(), ["0o600", inode])

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
        self.assertEqual(result["exitCode"], 0, result)
        sync = [method for method in self.calls if method.startswith("sync.")]
        self.assertLess(len(sync), 20)
        self.assertGreater(sync.count("sync.batch"), 1)
        self.assertIn("sync.chunk", sync)

    async def test_slot_generation_numbers_only_grow(self):
        self.project_settings({"slots": 1})
        first = await self.acquire("number-1")
        self.assertEqual(first["generationNumber"], 1)
        await self.guest.call("release", {"operationId": "number-1", "agentId": "agent"}, "release")
        warm = await self.acquire("number-2")
        self.assertEqual((warm["generation"], warm["generationNumber"]), (first["generation"], 1))
        await self.guest.call("release", {"operationId": "number-2", "agentId": "agent", "abandon": True}, "release")
        fresh = await self.acquire("number-3")
        self.assertNotEqual(fresh["generation"], first["generation"])
        self.assertEqual(fresh["generationNumber"], 2)
        await self.guest.call("release", {"operationId": "number-3", "agentId": "agent"}, "release")
        # A removed and remade slot continues the numbers of its project.
        shutil.rmtree(Path(fresh["slotPath"]).parent)
        again = await self.acquire("number-4")
        self.assertEqual((again["slotId"], again["generationNumber"]), (fresh["slotId"], 3))

    async def test_an_exact_retry_returns_the_saved_answer(self):
        op = await self.acquire("retry")
        manifest = scan(Path(op["slotPath"]))
        run = {"operationId": "retry", "agentId": "agent",
               "argv": [sys.executable, "-c", 'import pathlib;p=pathlib.Path("count");p.write_text(p.read_text()+"x" if p.exists() else "x")'],
               "manifestSha256": key(json.dumps(manifest, sort_keys=True))}
        first = await self.guest.call("run", run, "run")
        await asyncio.to_thread(self.host.wait_idle)
        self.assertEqual(await self.guest.call("run", run, "run"), first)
        self.assertEqual((Path(op["slotPath"]) / "count").read_text(), "x")
        with self.assertRaises(HostExecError) as caught:
            await self.guest.call("run", {**run, "argv": ["true"]}, "run")
        self.assertEqual(caught.exception.code, "id_conflict")


if __name__ == "__main__":
    unittest.main(verbosity=2)
