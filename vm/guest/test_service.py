"""Guest contract tests through real processes, journals, and socket connections."""
from __future__ import annotations

import asyncio
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import sqlite3
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
from common import GuestError, process_identity
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

    async def upload(self, identity, data, *, mode="full", deletes=()):
        root = self.service.projects / "repo"
        params = {"uploadId": identity, "root": str(root), "totalBytes": len(data),
                  "sha256": hashlib.sha256(data).hexdigest(), "mode": mode, "deletePaths": list(deletes)}
        await self.call("upload.begin", params)
        for sequence, offset in enumerate(range(0, len(data), 4096)):
            chunk = {"uploadId": identity, "seq": sequence, "data": encoded(data[offset:offset + 4096])}
            await self.call("upload.chunk", chunk)
            await self.call("upload.chunk", chunk)
        return await self.call("upload.commit", {"uploadId": identity})

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

    async def test_archive_traversal_and_links_rejected(self):
        for index, entries in enumerate(({"../escape": b"no"}, {"link": ("../../escape",)}, {"/escape": b"no"})):
            with self.assertRaises(GuestError):
                await self.upload("bad" + str(index), archive(entries))
        self.assertFalse((self.root / "escape").exists())

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
