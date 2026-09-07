#!/usr/bin/env python3
"""Monitor outcomes retain their workspace lease until the native command exits."""
import concurrent.futures
import fcntl
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
spec = importlib.util.spec_from_file_location("runtime_fixture", ROOT / "tests/runtime-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import ResponseTimeout, Runtime


class MonitorServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.commands = {}
        self.command_wait_entered = threading.Event()
        self.timeout_commands = False
        self.terminate_error = None
        self.writes = []
        self.timeout_writes = False
        self.write_error = None
        self.write_submit_error = None
        self.command_submit_error = None

    def submit(self, method, params):
        if method == "command/exec/write":
            future = concurrent.futures.Future()
            self.calls.append((method, params))
            self.writes.append(future)
            if self.write_submit_error:
                raise RuntimeError(self.write_submit_error)
            if self.write_error:
                future.set_exception(RuntimeError(self.write_error))
            elif not self.timeout_writes:
                future.set_result({})
            return future
        if method != "command/exec":
            return super().submit(method, params)
        future = concurrent.futures.Future()
        self.calls.append((method, params))
        self.commands[params["processId"]] = future
        if self.command_submit_error:
            raise OSError(self.command_submit_error)
        return future

    def wait(self, submitted, timeout=60):
        if submitted in self.writes and self.timeout_writes:
            raise ResponseTimeout("command/exec/write response timed out; outcome unknown")
        if submitted in self.commands.values():
            self.command_wait_entered.set()
            if self.timeout_commands:
                raise ResponseTimeout("command/exec response timed out; outcome unknown")
        return super().wait(submitted, timeout)

    def call(self, method, params, timeout=60):
        if method == "command/exec/terminate":
            self.calls.append((method, params))
            if self.terminate_error:
                raise RuntimeError(self.terminate_error)
            # An acknowledgement alone does not prove the original command exited.
            return {}
        return super().call(method, params, timeout)

    def finish(self, key, code=0):
        future = self.commands[key]
        if not future.done():
            future.set_result({"exitCode": code, "stdout": "", "stderr": ""})

    def close(self):
        for key in tuple(self.commands):
            self.finish(key)
        super().close()


class MonitorLifecycleContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), MonitorServer)
        self.agent = self.runtime.create({"name": "Monitor lead", "cwd": self.temp.name, "prompt": "Wait"})
        fixture.eventually(lambda: self.runtime.agent(self.agent["id"])["status"] == "running")
        self.agent = self.runtime.agent(self.agent["id"])
        self.server = self.runtime.server
        self.server.complete(self.agent["threadId"], self.agent["turnId"])
        self.monitor_threads = []
        original_run = self.runtime.run_monitor
        def run(key):
            self.monitor_threads.append(threading.current_thread())
            return original_run(key)
        self.runtime.run_monitor = run

    def tearDown(self):
        self.runtime.close()
        for thread in self.monitor_threads:
            thread.join(3)
            self.assertFalse(thread.is_alive(), "The fixture monitor worker did not stop")
        self.temp.cleanup()

    def monitor(self, interactive=False):
        result = self.runtime.monitor(self.agent["id"], {"command": "fixture-command", "timeout_ms": 1000, "interactive": interactive}, approved=True)
        self.assertTrue(self.server.command_wait_entered.wait(3))
        return result["id"]

    def record(self, key):
        with self.runtime.db() as db:
            return json.loads(db.execute("SELECT record FROM runtime_monitors WHERE id=?", (key,)).fetchone()[0])

    def exits(self, key):
        with self.runtime.db() as db:
            return [dict(row) for row in db.execute("SELECT * FROM runtime_events WHERE id=?", ("monitor:" + key,))]

    def assert_lease(self, key):
        record = self.record(key)
        self.assertEqual(record["status"], "running")
        self.assertIsNone(record.get("finished"))
        self.assertIsNone(record.get("exitCode"))
        with self.assertRaisesRegex(ValueError, "monitor"):
            self.runtime.assert_workspace_idle(self.runtime.agent(self.agent["id"]))

    def test_rejected_termination_retains_lease_and_records_actual_exit(self):
        key = self.monitor()
        self.server.terminate_error = "Termination rejected"
        self.runtime.cancel_monitor(key)
        self.assert_lease(key)
        self.assertTrue(self.record(key).get("cancelRequested"))
        self.assertIn("Termination rejected", self.record(key)["error"])
        self.server.finish(key, 7)
        fixture.eventually(lambda: self.record(key)["status"] == "cancelled")
        self.assertEqual(self.record(key)["exitCode"], 7)
        self.assertIsNotNone(self.record(key).get("finished"))
        self.assertEqual(self.exits(key), [])

    def test_termination_acknowledgement_waits_for_command_exit(self):
        key = self.monitor()
        self.runtime.cancel_monitor(key)
        self.assert_lease(key)
        self.assertTrue(self.record(key).get("cancelRequested"))
        self.server.finish(key, 130)
        fixture.eventually(lambda: self.record(key)["status"] == "cancelled")
        self.assertEqual(self.record(key)["exitCode"], 130)
        self.assertEqual(self.exits(key), [])

    def test_command_timeout_retains_lease_until_late_success(self):
        self.server.timeout_commands = True
        key = self.monitor()
        fixture.eventually(lambda: bool(self.record(key).get("error")))
        self.assert_lease(key)
        self.assertEqual(self.exits(key), [])
        self.server.finish(key, 0)
        fixture.eventually(lambda: self.record(key)["status"] == "completed")
        self.assertEqual(self.record(key)["exitCode"], 0)
        self.assertIsNone(self.record(key).get("error"))
        self.assertEqual(len(self.exits(key)), 1)

    def test_command_timeout_then_cancel_retains_actual_failure(self):
        self.server.timeout_commands = True
        key = self.monitor()
        fixture.eventually(lambda: bool(self.record(key).get("error")))
        self.runtime.cancel_monitor(key)
        self.assert_lease(key)
        self.server.finish(key, 9)
        fixture.eventually(lambda: self.record(key)["status"] == "cancelled")
        self.assertEqual(self.record(key)["exitCode"], 9)
        self.assertEqual(self.exits(key), [])

    def test_late_result_after_disconnect_does_not_claim_success_or_wake(self):
        self.server.timeout_commands = True
        key = self.monitor()
        fixture.eventually(lambda: bool(self.record(key).get("error")))
        self.runtime.disconnected("default", self.runtime.connection_ids["default"])
        self.assertEqual(self.record(key)["status"], "lost")
        self.server.finish(key, 0)
        self.runtime.close()
        for thread in self.monitor_threads:
            thread.join(3)
        self.assertEqual(self.record(key)["status"], "lost")
        self.assertEqual(self.exits(key), [])

    def test_replayed_monitor_cannot_change_terminal_interactivity(self):
        body = {"command": "fixture-command", "timeout_ms": 1000, "interactive": False}
        first = self.runtime.monitor(self.agent["id"], body, key="exact-monitor")
        repeated = self.runtime.monitor(self.agent["id"], body, key="exact-monitor")
        self.assertEqual(first["id"], repeated["id"])
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.monitor(self.agent["id"], {**body, "interactive": True}, key="exact-monitor")

    def test_unknown_eof_blocks_more_text_and_late_acknowledgement_closes_stdin(self):
        key = self.monitor(interactive=True)
        self.server.timeout_writes = True
        with self.assertRaises(ResponseTimeout):
            self.runtime.monitor_input(key, {"closeStdin": True})
        with self.assertRaises(ValueError):
            self.runtime.monitor_input(key, {"text": "after pending EOF"})
        self.assertEqual(len(self.server.writes), 1)
        self.server.writes[0].set_result({})
        fixture.eventually(lambda: self.record(key).get("stdinClosed") is True)
        with self.assertRaises(ValueError):
            self.runtime.monitor_input(key, {"text": "after acknowledged EOF"})
        self.assertEqual(len(self.server.writes), 1)
        self.server.finish(key)
        fixture.eventually(lambda: self.record(key)["status"] == "completed")

    def test_rejected_eof_does_not_permanently_block_stdin(self):
        key = self.monitor(interactive=True)
        self.server.write_error = "EOF rejected"
        with self.assertRaisesRegex(RuntimeError, "EOF rejected"):
            self.runtime.monitor_input(key, {"closeStdin": True})
        self.server.write_error = None
        self.runtime.monitor_input(key, {"text": "still open"})
        self.assertEqual(len(self.server.writes), 2)
        self.assertFalse(self.record(key).get("stdinClosed", False))
        self.server.finish(key)
        fixture.eventually(lambda: self.record(key)["status"] == "completed")

    def test_late_rejected_eof_reopens_input_without_replaying_eof(self):
        key = self.monitor(interactive=True)
        self.server.timeout_writes = True
        with self.assertRaises(ResponseTimeout):
            self.runtime.monitor_input(key, {"closeStdin": True})
        self.server.writes[0].set_exception(RuntimeError("EOF rejected"))
        fixture.eventually(lambda: not self.record(key).get("stdinCloseRequested"))
        self.assertFalse(self.record(key).get("stdinClosed", False))
        self.server.timeout_writes = False
        self.runtime.monitor_input(key, {"text": "still open"})
        self.assertEqual(len(self.server.writes), 2)
        self.server.finish(key)
        fixture.eventually(lambda: self.record(key)["status"] == "completed")

    def test_unknown_eof_submission_error_cannot_reopen_input(self):
        key = self.monitor(interactive=True)
        self.server.write_submit_error = "command/exec/write submission failed; outcome unknown"
        with self.assertRaisesRegex(RuntimeError, "outcome unknown"):
            self.runtime.monitor_input(key, {"closeStdin": True})
        self.server.write_submit_error = None
        with self.assertRaises(ValueError):
            self.runtime.monitor_input(key, {"text": "after potentially accepted EOF"})
        self.assertEqual(len(self.server.writes), 1)
        self.server.finish(key)
        fixture.eventually(lambda: self.record(key)["status"] == "completed")

    def test_cancel_pending_rejects_more_terminal_input(self):
        key = self.monitor(interactive=True)
        self.runtime.cancel_monitor(key)
        self.assert_lease(key)
        with self.assertRaisesRegex(ValueError, "not active"):
            self.runtime.monitor_input(key, {"text": "after cancel"})
        with self.assertRaisesRegex(ValueError, "not active"):
            self.runtime.monitor_input(key, {"rows": 30, "cols": 90})
        self.assertEqual(len(self.server.writes), 0)
        self.server.finish(key, 130)
        fixture.eventually(lambda: self.record(key)["status"] == "cancelled")

    def test_command_submission_transport_error_preserves_unknown_execution(self):
        self.server.command_submit_error = "Transport failed after command write"
        body = {"command": "fixture-command", "timeout_ms": 1000}
        result = self.runtime.monitor(self.agent["id"], body, approved=True, key="uncertain-submission")
        fixture.eventually(lambda: bool(self.record(result["id"]).get("error")))
        self.assert_lease(result["id"])
        replay = self.runtime.monitor(self.agent["id"], body, approved=True, key="uncertain-submission")
        self.assertEqual(replay["id"], result["id"])
        self.assertEqual(len(self.server.commands), 1)
        self.assertEqual(self.exits(result["id"]), [])

    def test_shutdown_holds_runtime_lease_until_scheduler_leaves_its_tick(self):
        entered, release, closed = threading.Event(), threading.Event(), threading.Event()
        original_tick = self.runtime.rules_tick
        def tick():
            entered.set()
            release.wait(5)
            return original_tick()
        self.runtime.rules_tick = tick
        self.runtime.changed.set()
        self.assertTrue(entered.wait(3))
        def close():
            self.runtime.close()
            closed.set()
        worker = threading.Thread(target=close)
        worker.start()
        try:
            fixture.eventually(lambda: self.runtime.closed)
            with (Path(self.temp.name) / "runtime.lock").open("a+") as lease:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertFalse(closed.wait(.05))
                release.set()
                worker.join(3)
                self.assertFalse(worker.is_alive())
                self.assertTrue(closed.is_set())
                fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
                fcntl.flock(lease, fcntl.LOCK_UN)
        finally:
            release.set()
            worker.join(6)


if __name__ == "__main__":
    unittest.main()
