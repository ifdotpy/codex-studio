"""Guest contract tests through real processes, journals, and socket connections."""
from __future__ import annotations

import asyncio
import base64
import fcntl
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent))
from common import GuestError, MAX_LINE, process_identity
from service import Service


def encoded(data):
    return base64.b64encode(data).decode()


def archive(entries):
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode="w") as output:
        for name, value in entries.items():
            item = tarfile.TarInfo(name)
            if isinstance(value, tuple):
                item.type = tarfile.SYMTYPE
                item.linkname = value[0]
                output.addfile(item)
            else:
                item.size = len(value)
                output.addfile(item, io.BytesIO(value))
    return data.getvalue()


class GuestTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="guest-", dir="/tmp")
        self.root = Path(self.temp.name).resolve()
        self.home = self.root / "home"
        self.home.mkdir()
        self.old_floor = os.environ.get("CODEX_WORKSPACE_MIN_FREE_BYTES")
        os.environ["CODEX_WORKSPACE_MIN_FREE_BYTES"] = "0"
        self.service = Service(self.root / "state", self.root / "workspaces", self.root / "projects", self.home)
        self.instances = [self.service]
        self.events = []
        self.next_id = 0
        self.servers = []

    async def asyncTearDown(self):
        for server in self.servers:
            server.close()
            await server.wait_closed()
        for row in self.service.list_providers():
            if row["state"] in {"running", "starting"}:
                try:
                    await self.call("provider.stop", {"handle": row["handle"]})
                except Exception:
                    pass
        lease = self.service.native.root / "supervisor.lock"
        if lease.exists():
            record = json.loads(lease.read_text())
            sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
            from codex_process_supervisor import process_start_time
            if process_start_time(record["pid"]) == record["startTime"]:
                try:
                    os.kill(record["pid"], signal.SIGTERM)
                except ProcessLookupError:
                    pass
        for instance in self.instances:
            processes = list(instance.processes)
            if instance.native.process is not None:
                processes.append(instance.native.process)
            for process in processes:
                await asyncio.to_thread(process.wait, timeout=5)
        self.service.close()
        await asyncio.sleep(0.1)
        self.temp.cleanup()
        if self.old_floor is None:
            os.environ.pop("CODEX_WORKSPACE_MIN_FREE_BYTES", None)
        else:
            os.environ["CODEX_WORKSPACE_MIN_FREE_BYTES"] = self.old_floor

    async def call(self, method, params=None, identity=None):
        self.next_id += 1
        request = {"id": identity or str(self.next_id), "method": method, "params": params or {}}

        async def emit(event, data):
            self.events.append((event, data))

        response = await self.service.request(request, emit)
        if "error" in response:
            raise GuestError(response["error"]["code"], response["error"]["message"])
        return response["result"]

    def command(self, source):
        return {"argv": [sys.executable, "-u", "-c", source], "cwd": str(self.home)}

    async def test_exec_stream_and_exact_retry(self):
        params = self.command("import sys; print('out'); print('err',file=sys.stderr)")
        result = await self.call("exec", params, "exec-1")
        self.assertEqual(result["exitCode"], 0)
        self.assertEqual(self.events[-1][0], "exit")
        self.assertEqual([row[1]["seq"] for row in self.events], list(range(1, result["lastSeq"] + 1)))
        first = list(self.events)
        self.events.clear()
        again = await self.call("exec", params, "exec-1")
        self.assertEqual(again, result)
        self.assertEqual(self.events, first)
        self.assertEqual(len(self.service.list_providers()), 1)
        with self.assertRaisesRegex(GuestError, "different content"):
            await self.call("exec", self.command("print('wrong')"), "exec-1")

    async def test_exec_timeout(self):
        result = await self.call("exec", {**self.command("import time; time.sleep(60)"), "timeoutSeconds": 0.1})
        self.assertEqual(result["reason"], "timeout")
        self.assertIsNotNone(result["exitCode"])

    async def test_stdio_write_restart_and_cursor(self):
        result = await self.call("provider.start", self.command("import sys,time; print('ready'); s=sys.stdin.readline(); print(s.strip()); time.sleep(60)"))
        handle = result["handle"]
        await self.call("provider.write", {"handle": handle, "data": encoded(b"once\n")}, "input-1")
        again = await self.call("provider.write", {"handle": handle, "data": encoded(b"once\n")}, "input-1")
        self.assertEqual(again["written"], 5)
        self.service.close()
        self.service = Service(self.root / "state", self.root / "workspaces", self.root / "projects", self.home)
        self.instances.append(self.service)
        self.assertEqual(self.service.status(handle)["state"], "running")
        deadline = asyncio.get_running_loop().time() + 5
        cursor = 0
        while sum(len(base64.b64decode(data["data"])) for event, data in self.events if event == "output") < 11:
            result = await self.call("provider.attach", {"handle": handle, "afterSeq": cursor, "waitMs": 200, "maxEvents": 100})
            cursor = result["nextSeq"]
            self.assertLess(asyncio.get_running_loop().time(), deadline)
        output = b"".join(base64.b64decode(data["data"]) for event, data in self.events if event == "output")
        self.assertEqual(output, b"ready\nonce\n")
        cursor = self.events[-1][1]["seq"]
        self.events.clear()
        await self.call("provider.attach", {"handle": handle, "afterSeq": cursor, "waitMs": 0})
        self.assertEqual(self.events, [])
        stopped = await self.call("provider.stop", {"handle": handle})
        self.assertEqual(stopped["state"], "exited")

    async def test_disconnect_does_not_stop_exec(self):
        socket_path = self.root / "guest.sock"
        server = await asyncio.start_unix_server(self.service.client, path=str(socket_path))
        self.servers.append(server)
        reader, writer = await asyncio.open_unix_connection(str(socket_path))
        marker = self.home / "proof"
        params = self.command(f"import time; time.sleep(.2); open({str(marker)!r},'w').write('one')")
        writer.write(json.dumps({"id": "lost-exec", "method": "exec", "params": params}).encode() + b"\n")
        await writer.drain()
        writer.close()
        await writer.wait_closed()
        deadline = asyncio.get_running_loop().time() + 5
        while not marker.exists() and asyncio.get_running_loop().time() < deadline:
            await asyncio.sleep(0.025)
        self.assertEqual(marker.read_text(), "one")
        await asyncio.sleep(0.2)
        result = await self.call("exec", params, "lost-exec")
        self.assertEqual(result["exitCode"], 0)
        self.assertEqual(len(self.service.list_providers()), 1)

    async def test_idle_timeout_starts_after_operation_result_and_reconnect_replays(self):
        socket_path = self.root / "guest.sock"
        server = await asyncio.start_unix_server(self.service.client, path=str(socket_path))
        self.servers.append(server)
        reader, writer = await asyncio.open_unix_connection(str(socket_path))
        marker = self.home / "long-exec"
        params = self.command(f"import time; time.sleep(.25); open({str(marker)!r},'a').write('once'); print('done')")
        request = {"id": "long-exec", "method": "exec", "params": params}
        received = []
        with patch("service.CLIENT_IDLE_SECONDS", 0.05):
            writer.write(json.dumps(request).encode() + b"\n")
            await writer.drain()
            while True:
                line = await asyncio.wait_for(reader.readline(), 5)
                self.assertTrue(line, "The idle timeout closed a connection with an active operation")
                response = json.loads(line)
                received.append(response)
                if "result" in response:
                    self.assertEqual(response["result"]["exitCode"], 0)
                    break
            self.assertEqual(await asyncio.wait_for(reader.readline(), 2), b"")
        writer.close()
        await writer.wait_closed()
        # The same durable request ID retrieves the complete result after idle close.
        reader, writer = await asyncio.open_unix_connection(str(socket_path))
        writer.write(json.dumps(request).encode() + b"\n")
        await writer.drain()
        replay = []
        while True:
            response = json.loads(await asyncio.wait_for(reader.readline(), 5))
            replay.append(response)
            if "result" in response:
                break
        writer.close()
        await writer.wait_closed()
        self.assertEqual(replay, received)
        self.assertEqual(marker.read_text(), "once")

    async def test_native_recovers_dead_supervisor_socket(self):
        await self.service.native.ensure()
        previous = self.service.native.process
        previous.terminate()
        await asyncio.to_thread(previous.wait, timeout=5)
        self.assertTrue(self.service.native.socket.exists())
        await self.service.native.ensure()
        self.assertNotEqual(self.service.native.process.pid, previous.pid)
        await self.service.native.call("health")
        record = json.loads((self.service.native.root / "supervisor.lock").read_text())
        self.assertEqual(record["pid"], self.service.native.process.pid)

    async def test_native_never_replaces_unresponsive_live_or_unproven_owner(self):
        sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
        from codex_process_supervisor import process_start_time
        # A bound socket without listen refuses connections. The lease owns this
        # test process, so recovery must not create or replace a supervisor.
        with socket.socket(socket.AF_UNIX) as stale:
            stale.bind(str(self.service.native.socket))
            before = self.service.native.socket.stat().st_ino
            lease = self.service.native.root / "supervisor.lock"
            lease.write_text(json.dumps({"pid": os.getpid(), "startTime": process_start_time(os.getpid())}))
            try:
                with self.assertRaises(GuestError) as error:
                    await self.service.native.ensure()
                self.assertEqual(error.exception.code, "outcome_unknown")
                self.assertIsNone(self.service.native.process)
                self.assertEqual(self.service.native.socket.stat().st_ino, before)
                lease.write_text("invalid")
                with self.assertRaises(GuestError):
                    await self.service.native.ensure()
                self.assertIsNone(self.service.native.process)
            finally:
                # This test lease names the test runner, not a test supervisor.
                lease.unlink()

    async def test_pending_receipt_is_never_repeated(self):
        from common import digest
        params = self.command("print('must not start')")
        self.service.db.execute("INSERT INTO receipts VALUES (?,?,NULL)", ("unknown", digest("exec", params)))
        self.service.db.commit()
        with self.assertRaises(GuestError) as error:
            await self.call("exec", params, "unknown")
        self.assertEqual(error.exception.code, "outcome_unknown")
        self.assertEqual(self.service.list_providers(), [])

    async def test_credentials_private_and_confined(self):
        await self.call("credentials.put", {"files": [{"path": ".codex/auth.json", "data": encoded(b"private")}]})
        path = self.home / ".codex/auth.json"
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(path.read_bytes(), b"private")
        for relative in ("../outside", "/outside", "."):
            with self.assertRaises(GuestError):
                await self.call("credentials.put", {"files": [{"path": relative, "data": encoded(b"bad")}]})
        (self.home / "link").symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(GuestError):
            await self.call("credentials.put", {"files": [{"path": "link/auth", "data": encoded(b"bad")}]})

    async def stage_upload(self, identity, data, *, mode="full", deletes=()):
        root = self.service.projects / "repo"
        params = {"uploadId": identity, "root": str(root), "totalBytes": len(data),
                  "sha256": hashlib.sha256(data).hexdigest(), "mode": mode, "deletePaths": list(deletes)}
        await self.call("upload.begin", params)
        for sequence, offset in enumerate(range(0, len(data), 4096)):
            chunk = {"uploadId": identity, "seq": sequence, "data": encoded(data[offset:offset + 4096])}
            await self.call("upload.chunk", chunk)
            await self.call("upload.chunk", chunk)
        return root

    async def upload(self, identity, data, *, mode="full", deletes=()):
        await self.stage_upload(identity, data, mode=mode, deletes=deletes)
        return await self.call("upload.commit", {"uploadId": identity})

    async def test_upload_retry_after_extraction_is_killed_mid_file(self):
        class Zeros:
            def read(self, count):
                return b"0" * count

        data = io.BytesIO()
        size = 256 * 1024 * 1024
        with tarfile.open(fileobj=data, mode="w:gz") as output:
            item = tarfile.TarInfo("large")
            item.size = size
            output.addfile(item, Zeros())
        data = data.getvalue()
        root = await self.stage_upload("interrupted", data)
        directory = self.service.uploads.path / "interrupted"
        staging = directory / "expanded"
        process = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("upload.py")),
            str(directory / "tree.tar"), str(staging), hashlib.sha256(data).hexdigest(),
            stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL, env=self.service.environment)
        try:
            deadline = asyncio.get_running_loop().time() + 10
            partial = staging / "large"
            while not partial.exists() or not partial.stat().st_size:
                self.assertIsNone(process.returncode)
                self.assertLess(asyncio.get_running_loop().time(), deadline)
                await asyncio.sleep(0.001)
            process.kill()
            await asyncio.wait_for(process.wait(), 5)
            self.assertGreater(partial.stat().st_size, 0)
            self.assertLess(partial.stat().st_size, size)
        finally:
            if process.returncode is None:
                process.kill()
                await asyncio.wait_for(process.wait(), 5)
        self.assertEqual(self.service.uploads.row("interrupted")[5], "receiving")
        result = await self.call("upload.commit", {"uploadId": "interrupted"})
        self.assertEqual(result["state"], "applied")
        self.assertEqual((root / "large").stat().st_size, size)
        with (root / "large").open("rb") as source:
            self.assertEqual(source.read(8), b"00000000")
        modified = (root / "large").stat().st_mtime_ns
        self.assertEqual(await self.call("upload.commit", {"uploadId": "interrupted"}), result)
        self.assertEqual((root / "large").stat().st_mtime_ns, modified)

    async def test_upload_retry_does_not_remove_a_live_extraction(self):
        await self.stage_upload("locked", archive({"file": b"new"}))
        directory = self.service.uploads.path / "locked"
        staging = directory / "expanded"
        staging.mkdir()
        (staging / "partial").write_bytes(b"owned")
        with (directory / "extract.lock").open("a+") as lease:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(GuestError) as error:
                await self.call("upload.commit", {"uploadId": "locked"})
            self.assertEqual(error.exception.code, "busy")
            self.assertEqual((staging / "partial").read_bytes(), b"owned")
        self.assertEqual((await self.call("upload.commit", {"uploadId": "locked"}))["state"], "applied")

    async def test_compressed_upload_reserves_expanded_space_before_apply(self):
        data = io.BytesIO()
        size = 32 * 1024 * 1024
        with tarfile.open(fileobj=data, mode="w:gz") as output:
            item = tarfile.TarInfo("large")
            item.size = size
            output.addfile(item, io.BytesIO(b"0" * size))
        data = data.getvalue()
        root = await self.stage_upload("space", data)
        self.assertLess(len(data), size // 100)
        with patch("service.tree_space", side_effect=GuestError("busy", "The data disk has insufficient free space")) as floor:
            with self.assertRaises(GuestError) as error:
                await self.call("upload.commit", {"uploadId": "space"})
            self.assertEqual(error.exception.code, "busy")
            floor.assert_called_once_with(self.service.projects, size)
        self.assertFalse(root.exists())
        self.assertFalse((self.service.uploads.path / "space" / "expanded").exists())
        self.assertEqual(self.service.uploads.row("space")[5], "receiving")
        self.assertEqual((await self.call("upload.commit", {"uploadId": "space"}))["state"], "applied")

    async def test_upload_apply_aborts_and_cleans_when_free_space_crosses_floor(self):
        from upload_apply import apply_tree
        staging = self.root / "expanded"
        staging.mkdir()
        (staging / "source").write_bytes(b"source")
        root = self.root / "destination"
        root.mkdir()
        (root / "existing").write_bytes(b"keep")
        executable = self.root / "bin"
        executable.mkdir()
        marker = self.home / "rsync-pid"
        fixture = executable / "rsync"
        fixture.write_text(f"#!{sys.executable}\nimport os,pathlib,sys,time\np=pathlib.Path(next(a.split('=',1)[1] for a in sys.argv if a.startswith('--temp-dir=')))\n(p/'partial').write_bytes(b'partial')\npathlib.Path({str(marker)!r}).write_text(str(os.getpid()))\ntime.sleep(60)\n")
        fixture.chmod(0o700)

        def floor(path, required=0):
            if marker.exists():
                raise GuestError("busy", "The data disk has insufficient free space")

        with patch("upload_apply.tree_space", side_effect=floor), patch.dict(os.environ, {"PATH": str(executable)}):
            with self.assertRaises(GuestError) as error:
                await asyncio.to_thread(apply_tree, staging, root, 6, "full")
        self.assertEqual(error.exception.code, "outcome_unknown")
        self.assertIn("free-space floor", str(error.exception))
        self.assertIsNone(process_identity(int(marker.read_text())))
        self.assertFalse((self.root / "rsync-temp").exists())
        self.assertFalse(staging.exists())
        self.assertEqual((root / "existing").read_bytes(), b"keep")

    async def test_upload_apply_without_a_response_is_unknown_and_not_repeated(self):
        await self.stage_upload("lost-apply", archive({"source": b"new"}))
        worker = self.service.worker

        async def interrupted(script, *args, **kwargs):
            if script == "upload_apply.py":
                return 247, b""
            return await worker(script, *args, **kwargs)

        with patch.object(self.service, "worker", side_effect=interrupted):
            with self.assertRaises(GuestError) as error:
                await self.call("upload.commit", {"uploadId": "lost-apply"})
        self.assertEqual(error.exception.code, "outcome_unknown")
        self.assertEqual(self.service.uploads.row("lost-apply")[5], "applying")
        with patch.object(self.service, "worker") as repeated:
            with self.assertRaises(GuestError) as error:
                await self.call("upload.commit", {"uploadId": "lost-apply"})
            self.assertEqual(error.exception.code, "outcome_unknown")
            repeated.assert_not_called()

    @unittest.skipUnless(__import__("shutil").which("rsync"), "rsync is required")
    async def test_full_delta_deletes_symlinks_and_commit_retry(self):
        await self.upload("first", archive({"a": b"one", "b": b"keep", "link": ("a",)}))
        root = self.service.projects / "repo"
        self.assertEqual((root / "link").read_bytes(), b"one")
        await self.upload("delta", archive({"a": b"two"}), mode="delta", deletes=["link"])
        self.assertEqual((root / "a").read_bytes(), b"two")
        self.assertEqual((root / "b").read_bytes(), b"keep")
        self.assertFalse((root / "link").exists())
        first = await self.call("upload.commit", {"uploadId": "delta"})
        (root / "a").write_bytes(b"worker change")
        self.assertEqual(await self.call("upload.commit", {"uploadId": "delta"}), first)
        self.assertEqual((root / "a").read_bytes(), b"worker change")
        await self.upload("full", archive({"c": b"new"}))
        self.assertEqual(sorted(path.name for path in root.iterdir()), ["c"])

    async def test_archive_traversal_rejected(self):
        for index, entries in enumerate(({"../escape": b"no"}, {"/escape": b"no"})):
            with self.assertRaises(GuestError):
                await self.upload("bad" + str(index), archive(entries))
        self.assertFalse((self.root / "escape").exists())

    @unittest.skipUnless(__import__("shutil").which("rsync"), "rsync is required")
    async def test_upload_preserves_absolute_and_outside_relative_link_objects(self):
        outside = self.home / "outside"
        outside.mkdir()
        (outside / "sentinel").write_bytes(b"keep")
        targets = {"absolute": str(outside), "relative": "../../home/outside", "cycle": "cycle"}
        await self.upload("links", archive({"ordinary": b"safe", "directory/file": b"old", "directory/untracked": b"old",
            **{name: (target,) for name, target in targets.items()}}))
        root = self.service.projects / "repo"
        for name, target in targets.items():
            self.assertTrue((root / name).is_symlink())
            self.assertEqual(os.readlink(root / name), target)
        self.assertEqual((root / "ordinary").read_bytes(), b"safe")
        self.assertEqual((outside / "sentinel").read_bytes(), b"keep")
        await self.upload("link-change", archive({"absolute": ("/not-present",)}), mode="delta")
        self.assertEqual(os.readlink(root / "absolute"), "/not-present")
        await self.upload("directory-link", archive({"directory": (str(outside),)}), mode="delta", deletes=["directory/file"])
        self.assertEqual(os.readlink(root / "directory"), str(outside))
        self.assertEqual((outside / "sentinel").read_bytes(), b"keep")

    async def test_archive_rejects_a_file_below_a_link_in_either_order(self):
        for index, entries in enumerate((
                {"link": (str(self.home),), "link/escape": b"no"},
                {"link/escape": b"no", "link": (str(self.home),)},
                {"link": ("directory",), "link/nested": ("/tmp",)})):
            with self.assertRaises(GuestError):
                await self.upload("link-parent" + str(index), archive(entries))
        self.assertFalse((self.home / "escape").exists())

    async def test_extraction_rejects_a_link_parent_already_in_the_destination(self):
        from upload import extract_tree
        destination = self.root / "expanded"
        destination.mkdir()
        (destination / "link").symlink_to(self.home)
        source = self.root / "archive.tar"
        source.write_bytes(archive({"link/escape": b"no"}))
        with self.assertRaises(GuestError) as error:
            extract_tree(source, destination)
        self.assertEqual(error.exception.code, "invalid_params")
        self.assertFalse((self.home / "escape").exists())

    @unittest.skipUnless(__import__("shutil").which("rsync"), "rsync is required")
    async def test_later_upload_rejects_any_existing_symlink_parent(self):
        outside = self.home / "outside"
        outside.mkdir()
        root = self.service.projects / "repo"
        targets = {"absolute": str(outside), "relative": "../../home/outside", "internal": "real"}
        await self.upload("parents", archive({"real/sentinel": b"keep", **{name: (target,) for name, target in targets.items()}}))
        for index, name in enumerate(targets):
            identity = "under-link" + str(index)
            with self.assertRaises(GuestError) as error:
                await self.upload(identity, archive({name + "/escape": b"no"}), mode="delta")
            self.assertEqual(error.exception.code, "invalid_params")
            self.assertEqual(self.service.uploads.row(identity)[5], "receiving")
            self.assertEqual(os.readlink(root / name), targets[name])
        self.assertFalse((outside / "escape").exists())
        self.assertFalse((root / "real" / "escape").exists())
        self.assertEqual((root / "real" / "sentinel").read_bytes(), b"keep")

    async def test_link_parent_replaced_after_validation_is_still_rejected(self):
        from upload_apply import apply_tree
        staging = self.root / "expanded"
        staging.mkdir()
        (staging / "directory").mkdir()
        (staging / "directory" / "link").symlink_to("/tmp")
        root = self.root / "destination"
        root.mkdir()
        (root / "directory").mkdir()
        outside = self.home / "outside"
        outside.mkdir()

        class Rsync:
            def wait(self, timeout):
                (root / "directory").rmdir()
                (root / "directory").symlink_to(outside)
                return 0

            def poll(self):
                return 0

        with patch("upload_apply.subprocess.Popen", return_value=Rsync()):
            with self.assertRaises(GuestError):
                apply_tree(staging, root, 0, "delta")
        self.assertEqual(list(outside.iterdir()), [])

    async def test_chunk_conflict_and_resume(self):
        data = archive({"a": b"ok"})
        params = {"uploadId": "resume", "root": str(self.service.projects / "repo"),
                  "totalBytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
        await self.call("upload.begin", params)
        await self.call("upload.chunk", {"uploadId": "resume", "seq": 0, "data": encoded(data[:4096])})
        self.service.close()
        self.service = Service(self.root / "state", self.root / "workspaces", self.root / "projects", self.home)
        self.instances.append(self.service)
        result = await self.call("upload.begin", params)
        self.assertEqual(result["receivedBytes"], 4096)
        self.assertEqual(result["nextSeq"], 1)
        with self.assertRaises(GuestError) as error:
            await self.call("upload.chunk", {"uploadId": "resume", "seq": 0, "data": encoded(b"changed")})
        self.assertEqual(error.exception.code, "id_conflict")

    async def test_native_supervisor_init_remap_replay_and_restart(self):
        source = """import json,sys
for line in sys.stdin:
 r=json.loads(line)
 if 'id' in r:
  print(json.dumps({'id':r['id'],'result':{'method':r.get('method'),'proof':'same'}}),flush=True)
"""
        params = {**self.command(source), "transport": "native", "handle": "linux-worker:contract"}
        opened = await self.call("provider.start", params, "native-open")
        handle = opened["handle"]
        rpc = {"handle": handle, "action": "write", "operationId": "init", "nativeId": 7,
               "message": {"id": 7, "method": "initialize", "params": {}}}
        written = await self.call("provider.rpc", rpc)
        self.assertEqual(written["remoteId"], 1)
        again = await self.call("provider.rpc", {**rpc, "nativeId": 19, "message": {**rpc["message"], "id": 19}})
        self.assertTrue(again["duplicate"])
        await asyncio.sleep(0.1)
        events = await self.call("provider.rpc", {"handle": handle, "action": "replay", "cursor": 0, "limit": 128})
        self.assertEqual(len(events["events"]), 1)
        self.assertEqual(json.loads(events["events"][0]["payload"])["id"], 1)
        self.service.close()
        self.service = Service(self.root / "state", self.root / "workspaces", self.root / "projects", self.home)
        self.instances.append(self.service)
        attached = await self.call("provider.start", params, "native-reattach")
        self.assertTrue(attached["resumed"])
        self.assertEqual(attached["initResult"], {"method": "initialize", "proof": "same"})
        await self.call("provider.rpc", {"handle": handle, "action": "ack", "sequence": 1})
        attached = await self.call("provider.start", params)
        self.assertEqual(attached["acknowledged"], 1)
        await self.call("provider.rpc", {"handle": handle, "action": "write", "operationId": "tool-response:one",
                                         "nativeId": "tool-one", "message": {"id": "tool-one", "result": {"ok": True}}})
        proof = await self.call("provider.rpc", {"handle": handle, "action": "responseStatus", "nativeId": "tool-one",
                                                  "generation": attached["generation"]})
        self.assertTrue(proof["accepted"])
        missing = await self.call("provider.rpc", {"handle": handle, "action": "responseStatus", "nativeId": "missing",
                                                    "generation": attached["generation"]})
        self.assertFalse(missing["accepted"])
        stopped = await self.call("provider.stop", {"handle": handle})
        self.assertEqual(stopped["state"], "exited")

    async def test_large_native_event_chunks_survive_reattach_and_ack(self):
        text = "é🙂" + "x" * 2_200_000
        source = "import json,time; print(json.dumps({'method':'fixture/large','params':{'text':'é🙂'+'x'*2200000}},ensure_ascii=False),flush=True); time.sleep(60)"
        params = {**self.command(source), "transport": "native", "handle": "linux-worker:large"}
        opened = await self.call("provider.start", params)
        handle = opened["handle"]
        deadline = asyncio.get_running_loop().time() + 5
        while self.service.native.info(handle)["sequence"] == 0:
            self.assertLess(asyncio.get_running_loop().time(), deadline)
            await asyncio.sleep(0.025)
        socket_path = self.root / "guest.sock"

        async def listen():
            server = await asyncio.start_unix_server(self.service.client, path=str(socket_path), limit=MAX_LINE + 1)
            self.servers.append(server)
            return server

        async def rpc(action, **values):
            reader, writer = await asyncio.open_unix_connection(str(socket_path), limit=MAX_LINE + 1)
            self.next_id += 1
            writer.write(json.dumps({"id": str(self.next_id), "method": "provider.rpc",
                "params": {"handle": handle, "action": action, **values}}).encode() + b"\n")
            await writer.drain()
            try:
                line = await asyncio.wait_for(reader.readline(), 10)
                self.assertTrue(line)
                self.assertLessEqual(len(line), MAX_LINE)
                response = json.loads(line)
                self.assertNotIn("error", response)
                return response["result"]
            finally:
                writer.close()
                await writer.wait_closed()

        server = await listen()
        event = (await rpc("next", cursor=0))["event"]
        self.assertIsNone(event["payload"])
        self.assertGreater(event["payloadBytes"], MAX_LINE)
        replay = await rpc("replay", cursor=0, limit=128)
        self.assertEqual(replay["events"], [event])
        self.assertFalse(replay["hasMore"])
        server.close()
        await server.wait_closed()
        socket_path.unlink(missing_ok=True)
        self.service.close()
        self.service = Service(self.root / "state", self.root / "workspaces", self.root / "projects", self.home)
        self.instances.append(self.service)
        attached = await self.call("provider.start", params)
        self.assertTrue(attached["resumed"])
        self.assertEqual(attached["acknowledged"], 0)
        await listen()
        self.assertEqual((await rpc("next", cursor=0))["event"], event)
        payload = bytearray()
        while len(payload) < event["payloadBytes"]:
            chunk = await rpc("eventRead", sequence=event["sequence"], offset=len(payload), maxBytes=1024 * 1024)
            self.assertEqual(chunk["offset"], len(payload))
            self.assertEqual((chunk["sequence"], chunk["generation"]), (event["sequence"], event["generation"]))
            data = base64.b64decode(chunk["data"], validate=True)
            self.assertLessEqual(len(data), 1024 * 1024)
            payload.extend(data)
            self.assertEqual(chunk["nextOffset"], len(payload))
            self.assertEqual(chunk["eof"], len(payload) == chunk["bytes"])
            self.assertEqual(self.service.native.info(handle)["acknowledged"], 0)
        self.assertEqual(len(payload), event["payloadBytes"])
        decoded = json.loads(payload)
        self.assertIsInstance(decoded.pop("_studioSupervisorReceivedAt"), (int, float))
        self.assertEqual(decoded, {"method": "fixture/large", "params": {"text": text}})
        await rpc("ack", sequence=event["sequence"])
        self.assertEqual(self.service.native.info(handle)["acknowledged"], event["sequence"])
        self.assertIsNone((await rpc("next", cursor=event["sequence"]))["event"])
        with self.assertRaises(GuestError) as error:
            await self.call("provider.rpc", {"handle": handle, "action": "eventRead", "sequence": event["sequence"], "offset": 0})
        self.assertEqual(error.exception.code, "not_found")

    async def test_output_limit_stops_the_process(self):
        params = {**self.command("import sys; sys.stdout.buffer.write(b'x' * (8 * 1024 * 1024)); sys.stdout.flush()"),
                  "outputLimitBytes": 1024 * 1024, "timeoutSeconds": 5}
        result = await asyncio.wait_for(self.call("exec", params), 12)
        self.assertEqual(result["reason"], "output_limit")
        output_bytes = sum(len(base64.b64decode(data["data"])) for event, data in self.events if event == "output")
        self.assertLessEqual(output_bytes, 1024 * 1024)

    async def test_file_chunks_and_change_detection(self):
        path = self.home / "bundle"
        path.write_bytes(b"one" * 700000)
        info = await self.call("file.stat", {"path": str(path)})
        self.assertEqual(info["bytes"], 2100000)
        output = bytearray()
        while len(output) < info["bytes"]:
            chunk = await self.call("file.read", {"path": str(path), "token": info["token"],
                                                  "offset": len(output), "maxBytes": 1024 * 1024})
            data = base64.b64decode(chunk["data"])
            self.assertEqual(hashlib.sha256(data).hexdigest(), chunk["sha256"])
            output.extend(data)
        self.assertEqual(hashlib.sha256(output).hexdigest(), info["sha256"])
        path.write_bytes(b"changed")
        with self.assertRaises(GuestError):
            await self.call("file.read", {"path": str(path), "token": info["token"]})

    async def test_live_exec_retry_replays_to_second_client(self):
        params = self.command("import time; print('first'); time.sleep(.3); print('second')")
        first_events = []
        second_events = []
        request = {"id": "concurrent", "method": "exec", "params": params}

        async def first(event, data):
            first_events.append((event, data))

        async def second(event, data):
            second_events.append((event, data))

        started = asyncio.create_task(self.service.request(request, first))
        await asyncio.sleep(0.1)
        attached = await self.service.request(request, second)
        self.assertEqual(await started, attached)
        self.assertEqual(first_events, second_events)

    async def test_helper_deadline_terminates_process_group(self):
        source = "import subprocess,sys,time; subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); time.sleep(60)"
        process = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("deadline.py")),
            "0.1", sys.executable, "-c", source, stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE)
        await asyncio.wait_for(process.communicate(b"{}"), 5)
        self.assertEqual(process.returncode, 124)

    async def test_failed_helper_terminates_its_descendants(self):
        marker = self.home / "helper-child"
        source = f"import subprocess,sys,os,json; sys.path.insert(0,{str(Path(__file__).parent)!r}); from common import process_identity; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL); open({str(marker)!r},'w').write(json.dumps({{'pid':p.pid,'start':process_identity(p.pid)}})); os._exit(3)"
        process = await asyncio.create_subprocess_exec(sys.executable, str(Path(__file__).with_name("deadline.py")),
            "2", sys.executable, "-c", source, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE)
        await asyncio.wait_for(process.communicate(b"{}"), 5)
        self.assertEqual(process.returncode, 3)
        child = json.loads(marker.read_text())
        pid = child["pid"]
        self.assertIsNotNone(child["start"])
        try:
            deadline = asyncio.get_running_loop().time() + 3
            while process_identity(pid) is not None and asyncio.get_running_loop().time() < deadline:
                await asyncio.sleep(0.025)
            self.assertIsNone(process_identity(pid))
        finally:
            # Clean up the child even when this regression fails on the prior code.
            if process_identity(pid) == child["start"]:
                os.kill(pid, signal.SIGKILL)

    async def test_invalid_shapes(self):
        for request in ([], {}, {"id": True}, {"id": "a", "method": "health", "params": []}):
            async def emit(*unused):
                pass
            response = await self.service.request(request, emit)
            self.assertIn("error", response)
        with self.assertRaises(GuestError):
            await self.call("exec", {**self.command("pass"), "timeoutSeconds": -1})


if __name__ == "__main__":
    unittest.main()
