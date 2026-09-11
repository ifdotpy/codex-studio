import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import fcntl

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("recovery", HERE / "recover_backend.py")
recovery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(recovery)


def free_port():
    with socket.socket() as server:
        server.bind(("127.0.0.1", 0))
        return server.getsockname()[1]


def wait_for(check, seconds=20):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError):
            pass
        time.sleep(.1)
    raise AssertionError("Recovery did not reach its expected state.")


class RecoveryTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-supervisor-test-")
        self.state = Path(self.temp.name).resolve()
        self.config = {"version": 1, "enabled": True, "stateDir": str(self.state),
                       "port": free_port(), "python": sys.executable,
                       "codex": "/usr/bin/true", "resources": str(HERE.parent),
                       "environment": {"CODEX_BOARD_STATE_DIR": str(self.state / "board")}}
        self.child = None
        self.supervisors = []
        self.recovered_pid = None

    def tearDown(self):
        for process in self.supervisors:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
        if self.child is not None and self.child.poll() is None:
            self.child.terminate()
            self.child.wait(timeout=10)
        if self.recovered_pid is not None:
            try:
                os.kill(self.recovered_pid, signal.SIGTERM)
                wait_for(lambda: recovery.identity(self.config["port"], self.state) is None)
            except ProcessLookupError:
                pass
        self.temp.cleanup()

    def test_free_port_with_occupied_runtime_never_starts(self):
        with (self.state / "runtime.lock").open("a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with patch.object(recovery.subprocess, "Popen") as spawn:
                child, status = recovery.tick(self.config, self.state)
            self.assertIsNone(child)
            self.assertEqual(status, "waiting for runtime owner")
            spawn.assert_not_called()

    def test_uncertain_or_incompatible_identity_never_starts(self):
        for error in (TimeoutError("timeout"), ValueError("invalid body"), RuntimeError("wrong state")):
            with patch.object(recovery, "identity", side_effect=error), patch.object(recovery.subprocess, "Popen") as spawn:
                with self.assertRaises(type(error)):
                    recovery.tick(self.config, self.state)
                spawn.assert_not_called()

    def test_same_owner_survives_supervisor_restart_and_crash_keeps_data(self):
        self.child, status = recovery.tick(self.config, self.state)
        self.assertEqual(status, "started")
        first = wait_for(lambda: recovery.identity(self.config["port"], self.state))
        self.assertEqual(first["pid"], self.child.pid)
        with sqlite3.connect(self.state / "canvas.sqlite3") as db:
            db.execute("CREATE TABLE recovery_test_receipt (id TEXT PRIMARY KEY, value TEXT)")
            db.execute("INSERT INTO recovery_test_receipt VALUES (?, ?)", ("accepted-once", "retained"))
        filename = self.state / "background-recovery.json"
        filename.write_text(json.dumps(self.config))
        for _ in range(2):
            log = self.state / f"supervisor-{len(self.supervisors)}.log"
            with log.open("w") as output:
                process = subprocess.Popen([sys.executable, "-B", str(HERE / "recover_backend.py"), "--config", str(filename)], stdout=output, stderr=output)
            self.supervisors.append(process)
            wait_for(lambda: 'attached' in log.read_text())
            self.assertEqual(recovery.identity(self.config["port"], self.state)["pid"], first["pid"])
            process.kill()
            process.wait(timeout=10)
            self.assertIsNone(self.child.poll())
        with (self.state / "supervisor-active.log").open("w") as output:
            process = subprocess.Popen([sys.executable, "-B", str(HERE / "recover_backend.py"), "--config", str(filename)], stdout=output, stderr=output)
        self.supervisors.append(process)
        wait_for(lambda: 'attached' in (self.state / "supervisor-active.log").read_text())
        self.child.kill()
        self.child.wait(timeout=10)
        second = wait_for(lambda: recovery.identity(self.config["port"], self.state))
        self.recovered_pid = second["pid"]
        self.assertNotEqual(second["pid"], first["pid"])
        with sqlite3.connect(self.state / "canvas.sqlite3") as db:
            self.assertEqual(db.execute("SELECT * FROM recovery_test_receipt").fetchall(), [("accepted-once", "retained")])

    def test_desktop_restarts_only_after_exact_saved_process_ends(self):
        filename = self.state / "desktop-recovery.json"
        intent = {"version": 1, "desiredOpen": True, "pid": os.getpid(),
                  "startedAt": subprocess.check_output(["/bin/ps", "-p", str(os.getpid()), "-o", "lstart="], text=True, env={**os.environ, "LC_ALL": "C"}).strip(),
                  "executable": "/usr/bin/true", "profile": str(self.state / "profile")}
        filename.write_text(json.dumps(intent))
        with patch.object(recovery.subprocess, "check_output", return_value=intent["startedAt"]), patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.desktop_tick(self.config, self.state)[1], "desktop attached")
            spawn.assert_not_called()
        dead = subprocess.Popen(["/usr/bin/true"])
        dead.wait(timeout=5)
        intent["pid"] = dead.pid
        filename.write_text(json.dumps(intent))
        child, status = recovery.desktop_tick(self.config, self.state)
        self.assertEqual(status, "desktop started")
        self.assertEqual(child.wait(timeout=5), 0)
        intent["desiredOpen"] = False
        filename.write_text(json.dumps(intent))
        with patch.object(recovery.subprocess, "Popen") as spawn:
            self.assertEqual(recovery.desktop_tick(self.config, self.state)[1], "desktop closed")
            spawn.assert_not_called()

    def test_desktop_unknown_process_and_changed_intent_do_not_restart(self):
        filename = self.state / "desktop-recovery.json"
        intent = {"version": 1, "desiredOpen": True, "pid": os.getpid(),
                  "startedAt": "unknown", "executable": "/usr/bin/true", "profile": str(self.state)}
        filename.write_text(json.dumps(intent))
        with patch.object(recovery.subprocess, "check_output", return_value=""), patch.object(recovery.subprocess, "Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "identity is unavailable"):
                recovery.desktop_tick(self.config, self.state)
            spawn.assert_not_called()

    def test_authoritative_absent_environment_stays_absent(self):
        with patch.dict(os.environ, {"CODEX_HOME": "/foreign-account"}):
            env = recovery.launch_environment({**self.config, "unsetEnvironment": ["CODEX_HOME"]}, self.state)
            self.assertNotIn("CODEX_HOME", env)
            self.assertEqual(env["CODEX_BOARD_STATE_DIR"], str(self.state / "board"))

    def test_configuration_cannot_switch_database(self):
        filename = self.state / "background-recovery.json"
        filename.write_text(json.dumps({**self.config, "stateDir": str(self.state / "other")}))
        with self.assertRaisesRegex(RuntimeError, "different state"):
            recovery.load_config(filename, self.state)


if __name__ == "__main__":
    unittest.main()
