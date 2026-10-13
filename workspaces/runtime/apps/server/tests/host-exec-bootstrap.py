#!/usr/bin/env python3
"""Real service processes: an idle host_exec service of an older build is replaced, a busy one stays."""
from __future__ import annotations

import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT, VM_GUEST_ROOT  # noqa: E402
from codex_host_exec import ServiceError, _service_call, ensure_service, service_build_id  # noqa: E402

# A service from before build hashes: it answers health with the protocol only.
LEGACY_SERVICE = """
import fcntl, json, socket, subprocess, sys
from pathlib import Path
state = Path(sys.argv[1])
lease = (state / "host-exec.lock").open("a+")
fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
command = subprocess.Popen(["sleep", "60"]) if "--command" in sys.argv else None
path = state / "host-exec.sock"
path.unlink(missing_ok=True)
server = socket.socket(socket.AF_UNIX)
server.bind(str(path))
server.listen()
while True:
    connection, _ = server.accept()
    with connection, connection.makefile("rwb") as stream:
        try:
            request = json.loads(stream.readline())
            stream.write(json.dumps({"id": request["id"], "result": {"protocol": 1}}).encode() + b"\\n")
        except (OSError, ValueError):
            pass
"""


class Bootstrap(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="he-up-", dir="/tmp")
        self.root = Path(self.temp.name).resolve()
        self.state = self.root / "state"
        self.state.mkdir()
        self.token = "a" * 64
        (self.state / "host-exec-token").write_text(self.token)
        self.log = (self.root / "child.log").open("ab")
        self.children = []
        self.addCleanup(self.cleanup)

    def start_old_build(self):
        # The packaged layout: scripts/ next to vm/guest/.
        old = self.root / "old"
        for source, name in ((SERVER_SOURCE_ROOT / "codex_host_exec.py", "scripts/codex_host_exec.py"),
                             (SERVER_SOURCE_ROOT / "codex_host_exec_server.py", "scripts/codex_host_exec_server.py"),
                             (SERVER_SOURCE_ROOT / "codex_layout.py", "scripts/codex_layout.py"),
                             (VM_GUEST_ROOT / "host_exec_protocol.py", "vm/guest/host_exec_protocol.py")):
            destination = old / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes(source.read_bytes())
        self.old_server = old / "scripts/codex_host_exec_server.py"
        with self.old_server.open("a") as stream:
            stream.write("\n# Older package build for the upgrade test.\n")
        self.child = self.spawn([sys.executable, str(self.old_server), str(self.state)])
        self.health = self.wait_health()
        self.assertNotEqual(self.health["buildId"], service_build_id())

    def start_legacy(self, name="codex_host_exec_server.py", *options):
        script = self.root / "legacy" / name
        script.parent.mkdir()
        script.write_text(LEGACY_SERVICE)
        self.child = self.spawn([sys.executable, str(script), str(self.state), *options])
        self.health = self.wait_health()
        self.assertNotIn("buildId", self.health)

    def spawn(self, argv):
        child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=self.log, stderr=self.log,
                                 start_new_session=True)
        self.children.append(child)
        return child

    def call(self, method, params=None):
        return _service_call(self.state, self.token, method, params)

    def wait_health(self):
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                return self.call("health")
            except (OSError, ValueError):
                time.sleep(0.02)
        self.fail("The fixture service did not start: " + (self.root / "child.log").read_text())

    def ensure(self):
        """ensure_service with its new service in the cleanup list. Returns its stderr."""
        factory = subprocess.Popen

        def spawn(*args, **kwargs):
            child = factory(*args, **kwargs)
            self.children.append(child)
            return child

        errors = io.StringIO()
        with patch("codex_host_exec.subprocess.Popen", side_effect=spawn), contextlib.redirect_stderr(errors):
            ensure_service(self.state)
        return errors.getvalue()

    def cleanup(self):
        try:
            for path in (self.state / "host-operations").glob("*/status.json"):
                op = json.loads(path.read_text())
                if op["state"] == "running":
                    with contextlib.suppress(OSError, ValueError, ServiceError):
                        self.call("cancel", {"operationId": op["operationId"], "agentId": op["agentId"]})
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    if not self.call("health").get("activeCommands"):
                        break
                except (OSError, ValueError, ServiceError):
                    break
                time.sleep(0.02)
        finally:
            for child in self.children:
                # The process group holds the command children of a legacy fixture too.
                with contextlib.suppress(ProcessLookupError, PermissionError):
                    os.killpg(child.pid, signal.SIGKILL)
                child.wait(timeout=5)
            self.log.close()
            self.temp.cleanup()

    def acquire(self):
        return self.call("acquire", {"operationId": "upgrade-operation", "agentId": "agent",
                                     "projectId": "project", "linePath": "/line"})

    def test_idle_old_build_replaced_preserves_receipts_and_lease(self):
        self.start_old_build()
        op = self.acquire()
        lease = Path(op["slotPath"]).parent / "lease.json"
        token = (self.state / "host-exec-token").read_bytes()
        before_lease = lease.read_bytes()
        with self.old_server.open("a") as stream:
            stream.write("# A source change must not alter the running build identity.\n")
        self.assertEqual(self.call("health")["buildId"], self.health["buildId"])
        self.assertEqual(self.ensure(), "")
        new = self.call("health")
        self.assertEqual(new["buildId"], service_build_id())
        self.assertNotEqual(new["pid"], self.health["pid"])
        self.assertEqual(self.child.wait(timeout=5), 0)
        self.assertEqual(lease.read_bytes(), before_lease)
        self.assertEqual((self.state / "host-exec-token").read_bytes(), token)
        # The saved receipt answers the exact retry; the operation keeps its lease.
        self.assertEqual(self.acquire(), op)
        self.assertEqual(self.call("status", {"operationId": op["operationId"], "agentId": "agent"}), op)

    def test_running_old_build_stays_and_the_start_reports_it(self):
        self.start_old_build()
        op = self.acquire()
        empty = hashlib.sha256(b"{}").hexdigest()
        self.call("run", {"operationId": op["operationId"], "agentId": "agent",
            "argv": [sys.executable, "-c", "import time;time.sleep(30)"], "manifestSha256": empty, "timeoutSeconds": 60})
        self.assertIn("older build stays because it has a running command", self.ensure())
        self.assertIsNone(self.child.poll())
        self.assertEqual(self.call("health")["pid"], self.health["pid"])
        self.assertEqual(self.call("health")["buildId"], self.health["buildId"])
        with self.assertRaises(ServiceError) as caught:
            self.call("service.retire", {"buildId": self.health["buildId"], "pid": self.health["pid"]})
        self.assertEqual(caught.exception.code, "busy")
        self.assertTrue((Path(op["slotPath"]).parent / "lease.json").exists())

    def test_older_unknown_outcomes_do_not_block_replacement(self):
        self.start_old_build()
        op = self.acquire()
        status = next((self.state / "host-operations").glob("*/status.json"))
        status.write_text(json.dumps({**op, "state": "unknown"}))
        with contextlib.closing(sqlite3.connect(self.state / "host-exec.sqlite3")) as database, database:
            database.execute("INSERT INTO receipts VALUES ('pending','digest',NULL)")
        self.assertEqual(self.ensure(), "")
        self.assertEqual(self.call("health")["buildId"], service_build_id())
        self.assertEqual(json.loads(status.read_text())["state"], "unknown")
        with contextlib.closing(sqlite3.connect(self.state / "host-exec.sqlite3")) as database:
            self.assertEqual(database.execute("SELECT response FROM receipts WHERE id='pending'").fetchone(), (None,))

    @unittest.skipUnless(sys.platform == "darwin", "LOCAL_PEERPID is a macOS socket option")
    def test_idle_service_without_build_identity_is_replaced(self):
        self.start_legacy()
        self.assertEqual(self.ensure(), "")
        self.assertEqual(self.child.wait(timeout=5), -signal.SIGTERM)
        self.assertEqual(self.call("health")["buildId"], service_build_id())

    @unittest.skipUnless(sys.platform == "darwin", "LOCAL_PEERPID is a macOS socket option")
    def test_service_without_build_identity_keeps_its_running_command(self):
        self.start_legacy("codex_host_exec_server.py", "--command")
        deadline = time.monotonic() + 5
        while not subprocess.run(["pgrep", "-P", str(self.child.pid)], capture_output=True).stdout:
            self.assertLess(time.monotonic(), deadline)
            time.sleep(0.02)
        self.assertIn("older build stays because it has a running command", self.ensure())
        self.assertIsNone(self.child.poll())
        self.assertNotIn("buildId", self.call("health"))

    @unittest.skipUnless(sys.platform == "darwin", "LOCAL_PEERPID is a macOS socket option")
    def test_an_unidentified_process_is_never_stopped(self):
        self.start_legacy("other_service.py")
        self.assertIn("is not the service of this state folder", self.ensure())
        self.assertIsNone(self.child.poll())


if __name__ == "__main__":
    unittest.main(verbosity=2)
