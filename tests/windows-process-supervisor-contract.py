#!/usr/bin/env python3
"""Windows process-supervisor named-pipe, Job Object, and reattach contracts."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import codex_process_supervisor as supervisor
from codex_process_supervisor import process_start_time
from codex_windows_supervisor import membership_for_pid, open_job

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
        self.assertEqual(supervisor._recv(connection, reader), {"ok": True})
        return connection, reader

    def call(self, connection, reader, counter, action, **values):
        counter[0] += 1
        identity = counter[0]
        supervisor._send(connection, {
            "requestId": identity, "handle": "test:tree", "action": action, **values
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


if __name__ == "__main__":
    if WINDOWS:
        tempfile.tempdir = str(Path.home() / "studio-dev" / "tmp")
        Path(tempfile.tempdir).mkdir(parents=True, exist_ok=True)
    unittest.main(verbosity=2)
