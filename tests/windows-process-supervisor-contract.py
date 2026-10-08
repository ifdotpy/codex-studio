#!/usr/bin/env python3
"""Windows process-supervisor named-pipe, Job Object, and reattach contracts."""
import json
import ctypes
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import codex_process_supervisor as supervisor
from codex_process_supervisor import process_start_time
from codex_windows_supervisor import (
    connect_pipe, current_user_sid, membership_for_pid, open_job, pipe_endpoint, pipe_identity,
)

WINDOWS = os.name == "nt"
skip_posix = unittest.skipUnless(WINDOWS, "Windows supervisor contract")

CHILD = r'''import json, os, subprocess, sys, time
from pathlib import Path
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
Path(os.environ["DESCENDANT_PID_FILE"]).write_text(str(child.pid))
print(json.dumps({"method":"fixture/ready","params":{"pid":os.getpid()}}), flush=True)
for line in sys.stdin:
    request=json.loads(line)
    if "id" in request:
        print(json.dumps({"id":request["id"],"result":{"echo":request.get("params")}}), flush=True)
    if request.get("method") == "fixture/stop":
        break
while True:
    time.sleep(1)
'''

EXITING_CHILD = r'''import os, subprocess, sys
from pathlib import Path
child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
Path(os.environ["DESCENDANT_PID_FILE"]).write_text(str(child.pid))
'''

BACKEND = r'''import sys,time
sys.path.insert(0,sys.argv[1])
import codex_process_supervisor as s
root, child, marker, desc = sys.argv[2:]
env=dict(__import__('os').environ); env['DESCENDANT_PID_FILE']=desc
c=s._connect(root); r=c.makefile('r',encoding='utf-8')
s._send(c,{'protocol':s.PROTOCOL,'stateDir':root,'backendId':'backend-one'})
assert s._recv(c,r).get('ok')
s._send(c,{'requestId':1,'action':'open','handle':'test:tree','command':[sys.executable,'-u',child],
           'env':env,'cwd':root})
opened=s._recv(c,r); assert opened.get('result',{}).get('generation') == 1, opened
open(marker,'w').write('attached')
while True: time.sleep(1)
'''


def wait_for(function, timeout=10):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            last = function()
            if last:
                return last
        except (OSError, RuntimeError, ConnectionError):
            pass
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for fixture; last value: {last!r}")


@skip_posix
class WindowsSupervisorContract(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(
            prefix="supervisor-contract-", dir=Path.home() / "studio-dev" / "tmp"
        )
        self.root = Path(self.temporary.name)
        self.root.mkdir(parents=True, exist_ok=True)
        self.child_file = self.root / "child_fixture.py"
        self.child_file.write_text(CHILD, encoding="utf-8")
        self.descendant_file = self.root / "descendant.pid"
        self.backend_marker = self.root / "backend.marker"
        self.backend_file = self.root / "backend_fixture.py"
        self.backend_file.write_text(BACKEND, encoding="utf-8")
        self.supervisor_log = (self.root / "supervisor.log").open("w", encoding="utf-8")
        self.server = subprocess.Popen(
            [sys.executable, "-B", str(ROOT / "scripts" / "codex_process_supervisor.py"),
             "--state", str(self.root)],
            cwd=ROOT, stdout=subprocess.DEVNULL, stderr=self.supervisor_log,
        )
        self.addCleanup(self.cleanup)
        self.start_error = None

        def supervisor_ready():
            if self.server.poll() is not None:
                raise AssertionError(f"supervisor exited with {self.server.returncode}")
            try:
                return supervisor.status(self.root)
            except Exception as error:
                self.start_error = repr(error)
                return None

        try:
            wait_for(supervisor_ready)
        except Exception as error:
            if self.server.poll() is None:
                self.server.kill()
                self.server.wait(timeout=5)
            self.supervisor_log.flush()
            errors = Path(self.supervisor_log.name).read_text(encoding="utf-8")
            raise AssertionError(
                f"supervisor startup failed, exit={self.server.returncode}: "
                f"stderr={errors!r}; "
                f"last status error={self.start_error}"
            ) from error

    def cleanup(self):
        try:
            with supervisor.Journal(self.root).db() as db:
                rows = list(db.execute("SELECT job_name FROM child_identities WHERE job_name<>''"))
            for row in rows:
                job = open_job(row[0])
                if job:
                    job.terminate()
                    job.wait_empty(timeout=5)
                    job.close()
        except Exception:
            pass
        if self.server.poll() is None:
            self.server.kill()
            self.server.wait(timeout=5)
        self.supervisor_log.close()
        self.temporary.cleanup()

    def connect(self, backend="backend-test"):
        connection = supervisor._connect(self.root)
        reader = connection.makefile("r", encoding="utf-8")
        supervisor._send(connection, {
            "protocol": supervisor.PROTOCOL,
            "stateDir": str(self.root),
            "backendId": backend,
        })
        try:
            self.assertEqual(supervisor._recv(connection, reader), {"ok": True})
        except Exception:
            reader.close()
            connection.close()
            raise
        return connection, reader

    def call(self, connection, reader, counter, action, **values):
        counter[0] += 1
        identity = counter[0]
        handle = values.pop("_handle", "test:tree")
        supervisor._send(connection, {
            "requestId": identity, "handle": handle, "action": action, **values
        })
        response = supervisor._recv(connection, reader)
        self.assertEqual(response.get("requestId"), identity)
        if response.get("error"):
            raise RuntimeError(response["error"])
        return response["result"]

    def open_tree(self, connection, reader, counter):
        env = os.environ.copy()
        env["DESCENDANT_PID_FILE"] = str(self.descendant_file)
        return self.call(
            connection, reader, counter, "open",
            command=[sys.executable, "-u", str(self.child_file)],
            env=env, cwd=str(self.root),
        )

    def test_pipe_frames_replay_receipt_backend_restart_and_job_tree_stop(self):
        # The pipe is local, owner-only, and usable by this same user.
        connection, reader = self.connect("backend-first")
        counter = [0]
        first = self.open_tree(connection, reader, counter)
        self.assertFalse(first["resumed"])
        pid = wait_for(lambda: next((h["pid"] for h in supervisor.status(self.root)["handles"]
                                    if h["id"] == "test:tree"), None))
        descendant = wait_for(lambda: int(self.descendant_file.read_text())
                              if self.descendant_file.is_file() else None)
        self.assertNotEqual(pid, descendant)
        with supervisor.Journal(self.root).db() as db:
            saved = db.execute("SELECT pid,start_time,job_name FROM child_identities WHERE handle=?",
                                ("test:tree",)).fetchone()
            self.assertEqual(saved[0], pid)
            self.assertTrue(saved[1])
            self.assertTrue(saved[2].startswith("Local\\CodexStudioSupervisorJob-"))
        job = open_job(saved[2])
        self.assertIsNotNone(job)
        self.assertTrue(membership_for_pid(job, pid))
        self.assertTrue(membership_for_pid(job, descendant))

        wait_for(lambda: self.call(connection, reader, counter, "replay", cursor=0)["events"])
        message = {"id": 17, "method": "fixture/echo", "params": {"token": "exact-once"}}
        receipt = self.call(connection, reader, counter, "write", operationId="receipt-1",
                             nativeId=17, message=message)
        duplicate = self.call(connection, reader, counter, "write", operationId="receipt-1",
                               nativeId=17, message=message)
        self.assertFalse(receipt["duplicate"])
        self.assertTrue(duplicate["duplicate"])
        wait_for(lambda: any(json.loads(event["payload"]).get("result", {}).get("echo")
                             for event in self.call(connection, reader, counter, "replay", cursor=0)["events"]
                             if event["kind"] == "stdout"))

        # A backend process can die without closing its supervised child.
        connection.close()
        reader.close()
        worker = subprocess.Popen(
            [sys.executable, "-u", str(self.backend_file), str(ROOT / "scripts"),
             str(self.root), str(self.child_file), str(self.backend_marker), str(self.descendant_file)],
            cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        )
        wait_for(lambda: self.backend_marker.is_file())
        worker.kill()
        worker.wait(timeout=5)
        worker.stderr.close()
        def replacement_connect():
            try:
                return self.connect("backend-replacement")
            except AssertionError:
                return None

        replacement, replacement_reader = wait_for(replacement_connect)
        resumed = self.open_tree(replacement, replacement_reader, [0])
        self.assertTrue(resumed["resumed"])
        self.assertEqual(pid, supervisor.status(self.root)["handles"][0]["pid"])

        with supervisor.Journal(self.root).db() as db:
            db.execute("UPDATE child_identities SET job_name=? WHERE handle='test:tree'",
                       ("Local\\CodexStudioSupervisorJob-mismatch",))
        with self.assertRaisesRegex(RuntimeError, "job identity"):
            self.open_tree(replacement, replacement_reader, [0])
        with supervisor.Journal(self.root).db() as db:
            db.execute("UPDATE child_identities SET job_name=? WHERE handle='test:tree'",
                       (saved[2],))

        live = supervisor.status(self.root)["handles"][0]
        with supervisor.Journal(self.root).db() as db:
            original_start = db.execute("SELECT start_time FROM child_identities WHERE handle=?",
                                        ("test:tree",)).fetchone()[0]
            db.execute("UPDATE child_identities SET start_time='reused-pid' WHERE handle='test:tree'")
        with self.assertRaisesRegex(RuntimeError, "identity changed"):
            supervisor.admin_close_handle(self.root, "test:tree", live["pid"],
                                          live["startTime"], live["signature"])
        self.assertEqual(process_start_time(pid), original_start)
        with supervisor.Journal(self.root).db() as db:
            db.execute("UPDATE child_identities SET start_time=? WHERE handle='test:tree'",
                       (original_start,))
        result = supervisor.admin_close_handle(self.root, "test:tree", live["pid"],
                                               live["startTime"], live["signature"])
        self.assertEqual(result["closed"], True)
        wait_for(lambda: process_start_time(pid) is None)
        wait_for(lambda: process_start_time(descendant) is None)
        replacement_reader.close()
        replacement.close()
        job.close()

    def test_named_pipe_rejects_over_limit_frame(self):
        connection = supervisor._connect(self.root)
        reader = connection.makefile("r", encoding="utf-8")
        connection.sendall(b"{" + b" " * supervisor.MAX_FRAME_BYTES + b"}\n")
        result = supervisor._recv(connection, reader)
        self.assertIn("size limit", result["error"])
        reader.close()
        connection.close()

    def test_windows_responses_stay_within_the_frame_limit(self):
        class Writer:
            data = b""
            def write(self, value):
                self.data += value
            def flush(self):
                pass

        writer = Writer()
        supervisor._write_frame(writer, {"requestId": 8, "payload": "x" * supervisor.MAX_FRAME_BYTES})
        self.assertLessEqual(len(writer.data), supervisor.MAX_FRAME_BYTES)
        self.assertEqual(json.loads(writer.data), {
            "requestId": 8, "error": "Supervisor response exceeds the size limit"
        })

    def test_named_pipe_client_read_obeys_socket_timeout(self):
        connection = supervisor._connect(self.root)
        connection.settimeout(0.1)
        reader = connection.makefile("r", encoding="utf-8")
        started = time.monotonic()
        with self.assertRaises(TimeoutError):
            supervisor._recv(connection, reader)
        self.assertLess(time.monotonic() - started, 1)
        reader.close()
        connection.close()

    def test_process_proxy_keeps_authenticated_idle_connection(self):
        env = os.environ.copy()
        env["DESCENDANT_PID_FILE"] = str(self.descendant_file)
        proxy = supervisor.ProcessProxy(
            self.root, "test:idle-proxy",
            [sys.executable, "-u", str(self.child_file)], env, str(self.root), None,
        )
        try:
            time.sleep(2.5)
            self.assertEqual(proxy.call("status")["returnCode"], None)
        finally:
            proxy.detach()

    def test_pipe_dacl_is_protected_and_contains_only_the_owner(self):
        connection = supervisor._connect(self.root)
        windows = __import__("codex_windows_supervisor")
        kernel32, advapi32 = windows._api()
        dacl = ctypes.c_void_p()
        descriptor = ctypes.c_void_p()
        advapi32.GetSecurityInfo.argtypes = [
            ctypes.c_void_p, ctypes.c_int, ctypes.c_uint32,
            ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p),
            ctypes.POINTER(ctypes.c_void_p), ctypes.POINTER(ctypes.c_void_p),
            ctypes.POINTER(ctypes.c_void_p),
        ]
        advapi32.GetSecurityInfo.restype = ctypes.c_uint32
        result = advapi32.GetSecurityInfo(
            connection.handle, 6, 0x4, None, None, ctypes.byref(dacl), None,
            ctypes.byref(descriptor),
        )
        self.assertEqual(result, 0)
        value = ctypes.c_wchar_p()
        advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW.argtypes = [
            ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32,
            ctypes.POINTER(ctypes.c_wchar_p), ctypes.POINTER(ctypes.c_uint32),
        ]
        advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW.restype = ctypes.c_int
        self.assertTrue(advapi32.ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor, 1, 0x4, ctypes.byref(value), None,
        ))
        self.assertEqual(value.value, "D:P(A;;FA;;;{})".format(current_user_sid()))
        kernel32.LocalFree.argtypes = [ctypes.c_void_p]
        kernel32.LocalFree(descriptor)
        kernel32.LocalFree(value)
        connection.close()

    def test_slow_and_truncated_pipe_frames_close_the_connection(self):
        slow_first = supervisor._connect(self.root)
        slow_first.settimeout(5)
        slow_reader = slow_first.makefile("r", encoding="utf-8")
        slow_first.sendall(b'{"protocol":')
        response = supervisor._recv(slow_first, slow_reader)
        self.assertIn("timed out", response["error"])
        slow_reader.close()
        slow_first.close()

        truncated = supervisor._connect(self.root)
        truncated.sendall(b'{"protocol":')
        truncated.close()
        connection, reader = self.connect("after-truncated-frame")
        connection.close()
        reader.close()

        connection, reader = self.connect("slow-request")
        slow_next = b'{"requestId":1,"action":'
        connection.sendall(slow_next)
        response = supervisor._recv(connection, reader)
        self.assertIn("timed out", response["error"])
        reader.close()
        connection.close()

        def reconnect_after_truncation():
            try:
                return self.connect("after-truncated-request")
            except (AssertionError, OSError, RuntimeError):
                return None
        connection, reader = wait_for(reconnect_after_truncation)
        connection.sendall(b'{"requestId":2,"action":')
        connection.close()
        connection, reader = wait_for(reconnect_after_truncation, timeout=8)
        reader.close()
        connection.close()

    def test_exited_root_descendants_are_stopped_before_reopen(self):
        connection, reader = self.connect("root-exit")
        child = self.root / "exiting_child.py"
        child.write_text(EXITING_CHILD, encoding="utf-8")
        env = os.environ.copy()
        env["DESCENDANT_PID_FILE"] = str(self.descendant_file)
        self.call(connection, reader, [0], "open", _handle="test:exited-root",
                  command=[sys.executable, str(child)], env=env, cwd=str(self.root))
        pid = wait_for(lambda: next((h["pid"] for h in supervisor.status(self.root)["handles"]
                                    if h["id"] == "test:exited-root"), None))
        descendant = wait_for(lambda: int(self.descendant_file.read_text())
                              if self.descendant_file.exists() else None)
        wait_for(lambda: self.call(connection, reader, [3], "status", _handle="test:exited-root")["returnCode"] is not None)
        self.call(connection, reader, [1], "open", _handle="test:exited-root",
                  command=[sys.executable, "-u", "-c", "import time; time.sleep(120)"],
                  env=os.environ.copy(), cwd=str(self.root))
        wait_for(lambda: process_start_time(descendant) is None)
        new_handle = next(h for h in supervisor.status(self.root)["handles"]
                          if h["id"] == "test:exited-root")
        supervisor.admin_close_handle(self.root, "test:exited-root", new_handle["pid"],
                                      new_handle["startTime"], new_handle["signature"])
        wait_for(lambda: process_start_time(pid) is None)
        reader.close()
        connection.close()

    def test_failed_job_termination_keeps_handle_open_and_owned(self):
        job = Mock()
        with self.assertRaisesRegex(RuntimeError, "ownership remains active"):
            supervisor._close_stopped_job(job, "termination-failed")
        job.close.assert_not_called()
        supervisor._close_stopped_job(job, "killed")
        job.close.assert_called_once()

    def test_retained_launch_is_refused_with_clear_windows_error(self):
        saved = {"closed_at": None, "pid": 10, "identity_pid": 10, "start_time": "10"}
        with patch.object(supervisor, "supervisor_launch_snapshot", return_value=saved):
            with self.assertRaisesRegex(RuntimeError, "Windows retained native launch verification is unavailable"):
                supervisor.retained_native_launch(self.root, "test:retained", [], {}, None)
        connection, reader = self.connect("retained-environment")
        self.open_tree(connection, reader, [0])
        with self.assertRaisesRegex(RuntimeError, "Windows retained native launch verification is unavailable"):
            supervisor.native_launch_environment(self.root, "test:tree", ["new.exe"], {}, None)
        live = supervisor.status(self.root)["handles"][0]
        supervisor.admin_close_handle(self.root, "test:tree", live["pid"],
                                      live["startTime"], live["signature"])
        reader.close()
        connection.close()

    def test_supervisor_restart_recovers_and_stops_the_saved_job(self):
        connection, reader = self.connect("before-supervisor-restart")
        self.open_tree(connection, reader, [0])
        pid = supervisor.status(self.root)["handles"][0]["pid"]
        descendant = wait_for(lambda: int(self.descendant_file.read_text())
                              if self.descendant_file.exists() else None)
        reader.close()
        connection.close()
        self.server.kill()
        self.server.wait(timeout=5)
        self.server = subprocess.Popen(
            [sys.executable, "-B", str(ROOT / "scripts" / "codex_process_supervisor.py"),
             "--state", str(self.root)],
            cwd=ROOT, stdout=subprocess.DEVNULL, stderr=self.supervisor_log,
        )
        status = wait_for(lambda: supervisor.status(self.root))
        try:
            wait_for(lambda: process_start_time(pid) is None)
        except AssertionError as error:
            with supervisor.Journal(self.root).db() as db:
                events = [dict(row) for row in db.execute("SELECT outcome,detail FROM recovery_events")]
            raise AssertionError(
                f"supervisor recovery left root PID {pid} alive; recovery={status.get('recovery')!r}; "
                f"events={events!r}"
            ) from error
        wait_for(lambda: process_start_time(descendant) is None)
        self.assertFalse(status["handles"])

    def test_job_handles_do_not_grow_after_many_child_cycles(self):
        connection, reader = self.connect("many-child-cycles")
        windows = __import__("codex_windows_supervisor")
        kernel32, _ = windows._api()
        lease = json.loads((self.root / "supervisor.lock").read_text(encoding="utf-8"))
        kernel32.OpenProcess.argtypes = [ctypes.c_uint32, ctypes.c_int, ctypes.c_uint32]
        kernel32.OpenProcess.restype = ctypes.c_void_p
        process = kernel32.OpenProcess(0x1000, False, lease["pid"])
        self.assertTrue(process)
        kernel32.GetProcessHandleCount.argtypes = [ctypes.c_void_p, ctypes.POINTER(ctypes.c_uint32)]
        kernel32.GetProcessHandleCount.restype = ctypes.c_int
        def handle_count():
            count = ctypes.c_uint32()
            self.assertTrue(kernel32.GetProcessHandleCount(process, ctypes.byref(count)))
            return count.value
        before = handle_count()
        for index in range(12):
            handle = "test:many-{}".format(index)
            self.call(connection, reader, [index], "open", _handle=handle,
                      command=[sys.executable, "-u", "-c", "import time; time.sleep(120)"],
                      env=os.environ.copy(), cwd=str(self.root))
            live = next(item for item in supervisor.status(self.root)["handles"]
                        if item["id"] == handle)
            supervisor.admin_close_handle(self.root, handle, live["pid"],
                                          live["startTime"], live["signature"])
            wait_for(lambda: process_start_time(live["pid"]) is None)
        time.sleep(0.5)
        after = handle_count()
        kernel32.CloseHandle(process)
        reader.close()
        connection.close()
        self.assertLessEqual(after, before + 8, (before, after))

    def test_pipe_precreation_blocks_supervisor_start(self):
        temporary = tempfile.TemporaryDirectory(
            prefix="pipe-squat-", dir=Path.home() / "studio-dev" / "tmp"
        )
        root = Path(temporary.name)
        root.mkdir(parents=True, exist_ok=True)
        identity = pipe_identity(root, create=True)
        endpoint = pipe_endpoint(root, identity)
        windows = __import__("codex_windows_supervisor")
        kernel32, _ = windows._api()
        kernel32.CreateNamedPipeW.argtypes = [
            ctypes.c_wchar_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32,
            ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p,
        ]
        kernel32.CreateNamedPipeW.restype = ctypes.c_void_p
        descriptor = windows._SecurityDescriptor("D:P(A;;GA;;;{})".format(current_user_sid()))
        attributes = windows._SecurityAttributes(
            ctypes.sizeof(windows._SecurityAttributes), descriptor.pointer, False
        )
        handle = kernel32.CreateNamedPipeW(
            endpoint, 0x00000003 | windows.FILE_FLAG_FIRST_PIPE_INSTANCE,
            windows.PIPE_REJECT_REMOTE_CLIENTS, 1, 65536, 65536, 0,
            ctypes.byref(attributes),
        )
        self.assertNotEqual(handle, ctypes.c_void_p(-1).value)
        (root / "supervisor.lock").write_text(
            json.dumps({"pid": 2147483647, "startTime": "not-the-server"}), encoding="utf-8"
        )
        kernel32.ConnectNamedPipe.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
        kernel32.ConnectNamedPipe.restype = ctypes.c_int
        connected = threading.Event()
        accepter = threading.Thread(
            target=lambda: (kernel32.ConnectNamedPipe(handle, None), connected.set()), daemon=True
        )
        accepter.start()
        with self.assertRaisesRegex(PermissionError, "server PID"):
            connect_pipe(root, timeout=2)
        accepter.join(timeout=2)
        self.assertTrue(connected.is_set())
        log = (root / "server.log").open("w", encoding="utf-8")
        server = None
        try:
            server = subprocess.Popen(
                [sys.executable, "-B", str(ROOT / "scripts" / "codex_process_supervisor.py"),
                 "--state", str(root)],
                cwd=ROOT, stdout=subprocess.DEVNULL, stderr=log,
            )
            self.assertNotEqual(server.wait(timeout=8), 0)
            log.flush()
            self.assertIn("WinError", (root / "server.log").read_text(encoding="utf-8"))
        finally:
            if server is not None and server.poll() is None:
                server.kill()
                server.wait(timeout=5)
            kernel32.CloseHandle(handle)
            descriptor.close()
            log.close()
            temporary.cleanup()

    def test_revert_to_self_failure_closes_pipe_and_stops_process(self):
        windows = __import__("codex_windows_supervisor")
        advapi32 = Mock()
        advapi32.RevertToSelf.return_value = False
        kernel32 = Mock()
        with patch.object(windows.os, "_exit", side_effect=SystemExit) as stop_process:
            with self.assertRaises(SystemExit):
                windows._revert_to_self_or_exit(advapi32, kernel32, 123)
        kernel32.CloseHandle.assert_called_once_with(123)
        stop_process.assert_called_once_with(70)


if __name__ == "__main__":
    if WINDOWS:
        tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
        Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)
    unittest.main(verbosity=2)
