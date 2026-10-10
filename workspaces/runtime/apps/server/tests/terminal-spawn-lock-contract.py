#!/usr/bin/env python3
"""Terminal spawn waits do not block output or repeat a native effect."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from concurrent.futures import Future
from contextlib import contextmanager
import codecs
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import ResponseTimeout, NativeRpcError
from codex_terminals import TerminalManager


class Runtime:
    def __init__(self, root):
        self.root = root
        self.lock = threading.RLock()

    @contextmanager
    def db(self):
        yield None

    def checked_actor(self, db, key):
        return {"id": key, "cwd": str(self.root)}


class Native:
    def __init__(self):
        self.closed = False
        self.transport_error = None
        self.proc = type("Process", (), {"poll": lambda _: None, "stdin": None,
                                          "stdout": None, "stderr": None})()
        self.submitted = []
        self.killed = []
        self.wait_entered = threading.Event()
        self.submit_entered = threading.Event()
        self.submit_release = threading.Event()
        self.submit_release.set()
        self.wait_release = threading.Event()
        self.future = Future()
        self.timeout = False

    def submit(self, method, params, *, operation_id):
        self.submit_entered.set()
        if not self.submit_release.wait(3):
            raise AssertionError("Submission gate was not released")
        self.submitted.append((method, params, operation_id))
        return (operation_id, method, self.future)

    def on_result(self, submitted, callback):
        submitted[2].add_done_callback(callback)

    def wait(self, submitted, **kwargs):
        self.wait_entered.set()
        if self.timeout:
            raise ResponseTimeout("Spawn response timed out; outcome unknown")
        if not self.wait_release.wait(3):
            raise AssertionError("Receipt gate was not released")
        return submitted[2].result(timeout=1)

    def call(self, method, params, **kwargs):
        self.killed.append((method, params))
        return {}

    def close(self):
        self.closed = True

    def join_callbacks(self):
        return True


class SpawnLockContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="terminal-spawn-lock-")
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root)
        self.manager = TerminalManager(self.root)
        self.native = Native()
        self.manager.server = self.native
        self.manager.connection = object()
        self.manager.terminate = lambda owned: None
        self.threads = []
        self.results = []
        self.errors = []
        with self.manager.lock, self.manager.db() as db:
            record = {"id": "existing", "status": "running", "created": 1}
            db.execute("INSERT INTO user_terminals VALUES (?,?,?,?)",
                       ("existing", json.dumps(record), "ready", 0))
            self.manager.processes["existing"] = {
                "server": self.native, "connection": self.manager.connection,
                "pid_path": self.root / "existing.pid",
                "decoder": codecs.getincrementaldecoder("utf-8")("replace"),
            }

    def tearDown(self):
        self.native.submit_release.set()
        if not self.native.future.done():
            self.native.future.set_result({})
        self.native.wait_release.set()
        for thread in self.threads:
            thread.join(4)
            self.assertFalse(thread.is_alive(), "A terminal operation did not finish")
        self.temp.cleanup()

    def start(self, operation):
        done = threading.Event()
        def run():
            try:
                self.results.append(operation())
            except BaseException as error:
                self.errors.append(error)
            finally:
                done.set()
        thread = threading.Thread(target=run, daemon=True)
        self.threads.append(thread)
        thread.start()
        return done

    def create(self):
        return self.manager.create(self.runtime, {"id": "create-once", "agent": "lead"})

    def assert_output_available(self):
        done = self.start(lambda: self.manager.output("existing"))
        self.assertTrue(done.wait(.5), "A new terminal blocked existing terminal output")
        self.assertEqual(self.results[-1]["text"], "ready")

    def test_receipt_wait_releases_lock_and_duplicate_creation_does_not_submit(self):
        done = self.start(self.create)
        self.assertTrue(self.native.wait_entered.wait(1))
        self.assert_output_available()
        duplicate = self.start(self.create)
        self.assertTrue(duplicate.wait(.5), "A duplicate waited for the native receipt")
        key = self.results[-1]["id"]
        self.assertEqual(len(self.native.submitted), 1)
        self.native.future.set_result({})
        self.native.wait_release.set()
        self.assertTrue(done.wait(1))
        self.assertEqual(self.results[-1]["id"], key)
        self.assertFalse(self.errors)

    def test_blocked_native_write_releases_lock_and_close_cannot_orphan_late_spawn(self):
        self.native.submit_release.clear()
        created = self.start(self.create)
        self.assertTrue(self.native.submit_entered.wait(1))
        self.assert_output_available()
        key = next(key for key in self.manager.processes if key != "existing")
        closed = self.start(lambda: self.manager.action("close", {"id": key}))
        self.assert_output_available()
        self.native.submit_release.set()
        self.assertTrue(closed.wait(1))
        self.native.future.set_result({})
        self.native.wait_release.set()
        self.assertTrue(created.wait(1))
        self.assertEqual(self.manager.output(key)["status"], "closed")
        self.assertEqual(self.create()["status"], "closed")
        self.assertTrue(self.native.killed)
        self.assertEqual(len(self.native.submitted), 1)
        self.assertFalse(self.errors)

    def test_unknown_spawn_and_late_receipt_do_not_repeat(self):
        self.native.timeout = True
        first = self.create()
        self.assertIn("unknown", first["error"])
        self.assertEqual(self.create(), first)
        self.native.future.set_result({})
        confirmed = self.create()
        self.assertNotIn("error", confirmed)
        self.assertEqual(confirmed["id"], first["id"])
        self.assertEqual(len(self.native.submitted), 1)

    def test_rejected_spawn_receipt_is_durable(self):
        self.native.timeout = True
        first = self.create()
        self.native.future.set_exception(NativeRpcError({"code": 1, "message": "Denied"}))
        self.assertEqual(self.create()["status"], "exited")
        self.assertNotIn(first["id"], self.manager.processes)
        self.assertEqual(len(self.native.submitted), 1)

    def test_late_spawn_ack_after_exit_updates_receipt_without_resurrection(self):
        self.native.timeout = True
        first = self.create()
        self.manager.finish(first["id"], 0)
        self.native.future.set_result({})
        latest = self.create()
        self.assertEqual(latest["status"], "exited")
        self.assertEqual(latest["exitCode"], 0)
        self.assertNotIn("error", latest)
        self.assertNotIn(first["id"], self.manager.processes)
        self.assertEqual(len(self.native.submitted), 1)

    def test_cold_connection_does_not_hold_manager_lock_and_shutdown_prevents_publication(self):
        self.manager.server = None
        entered, release = threading.Event(), threading.Event()
        def construct(*args, **kwargs):
            entered.set()
            self.assertTrue(release.wait(3))
            return self.native
        with patch("codex_runtime.AppServer", side_effect=construct):
            created = self.start(self.create)
            self.assertTrue(entered.wait(1))
            self.assert_output_available()
            stopped = self.start(self.manager.close)
            release.set()
            self.assertTrue(created.wait(1))
            self.assertTrue(stopped.wait(1))
        self.assertTrue(self.native.closed)
        self.assertEqual(self.native.submitted, [])
        self.assertTrue(any("closing" in str(error) for error in self.errors))


if __name__ == "__main__":
    unittest.main()
