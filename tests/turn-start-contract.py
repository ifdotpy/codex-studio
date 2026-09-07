#!/usr/bin/env python3
"""Late native turn acknowledgements. Isolated state, no model requests."""

import concurrent.futures
import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import AppServer, ResponseTimeout, Runtime, SubmissionUnknown

spec = importlib.util.spec_from_file_location("runtime_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
eventually = fixture.eventually


class DeferredServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.mode = "started"
        self.deferred = []
        self.wait_gate = None

    def submit(self, method, params):
        if method != "turn/start":
            return super().submit(method, params)
        future = concurrent.futures.Future()
        turn = {"id": "deferred-" + str(len(self.deferred)), "status": "inProgress"}
        entry = {"future": future, "turn": turn, "params": params,
                 "prepared": threading.Event(), "registered": threading.Event(),
                 "handled": threading.Event(), "mode": self.mode}
        self.deferred.append(entry)
        self.calls.append((method, params))

        def publish():
            if entry["mode"] != "silent":
                self.notify({"method": "turn/started", "params": {"threadId": params["threadId"], "turn": turn}})
            if entry["mode"] == "completed":
                self.complete(params["threadId"], turn["id"])
            entry["prepared"].set()

        threading.Thread(target=publish, daemon=True).start()
        return future

    def wait(self, future, timeout=60):
        entry = next((e for e in self.deferred if e["future"] is future), None)
        if entry:
            assert entry["prepared"].wait(3)
            if self.wait_gate is not None:
                assert self.wait_gate.wait(3)
            raise ResponseTimeout("turn/start response timed out; outcome unknown")
        return super().wait(future, timeout)

    def on_result(self, future, callback):
        entry = next((e for e in self.deferred if e["future"] is future), None)
        if entry is None:
            return super().on_result(future, callback)
        def handle(done):
            work = callback(done)
            work.add_done_callback(lambda _: entry["handled"].set())
        future.add_done_callback(handle)
        entry["registered"].set()


class TurnStartContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), DeferredServer)
        self.server = self.runtime.connect()

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def start(self, mode="started"):
        self.server.mode = mode
        a = self.runtime.create({"name": "Lead", "cwd": self.temp.name, "prompt": "First input"})
        self.wait_start(0)
        return a["id"]

    def wait_start(self, index):
        eventually(lambda: len(self.server.deferred) > index)
        self.assertTrue(self.server.deferred[index]["registered"].wait(3))
        return self.server.deferred[index]

    def events(self):
        return self.runtime.snapshot()["events"]

    def accept(self, index=0):
        e = self.server.deferred[index]
        e["future"].set_result({"turn": e["turn"]})
        return e

    def reject(self, index=0, message="invalid request"):
        entry = self.server.deferred[index]
        entry["future"].set_exception(RuntimeError(message))
        self.assertTrue(entry["handled"].wait(3))

    def notify_input(self, index=0, client_id=None):
        e = self.server.deferred[index]
        self.server.notify({"method": "item/started", "params": {
            "threadId": e["params"]["threadId"], "turnId": e["turn"]["id"],
            "item": {"id": "input-" + str(index), "type": "userMessage",
                     "clientId": client_id or e["params"]["clientUserMessageId"], "content": []}}})

    def test_observed_turn_survives_timeout_and_late_response_confirms_delivery(self):
        key = self.start()
        self.assertEqual(self.runtime.agent(key)["status"], "running")
        self.assertTrue(self.runtime.agent(key)["inFlight"])
        self.assertEqual(self.events()[0]["status"], "dispatching", "turn/started has no client correlation")
        self.runtime.send(key, "Second input")
        self.runtime.dispatch()
        self.assertEqual(len(self.server.deferred), 1)
        self.accept()
        eventually(lambda: any(e["status"] == "delivered" for e in self.events()))
        self.assertEqual(self.runtime.agent(key)["turnId"], "deferred-0")

    def test_client_id_confirms_only_its_exact_batch(self):
        key = self.start()
        self.notify_input(client_id="unrelated-client")
        self.assertEqual(self.events()[0]["status"], "dispatching")
        self.notify_input()
        self.assertEqual(self.events()[0]["status"], "delivered")
        self.reject(message="late contradictory error")
        self.assertEqual(self.runtime.agent(key)["status"], "running")

    def test_completed_turn_survives_timeout_and_late_reply(self):
        key = self.start("completed")
        self.assertEqual(self.runtime.agent(key)["status"], "completed")
        self.assertFalse(self.runtime.agent(key)["inFlight"])
        self.accept()
        eventually(lambda: self.events()[0]["status"] == "delivered")
        self.assertEqual(self.runtime.agent(key)["status"], "completed")
        self.assertIsNone(self.runtime.agent(key)["turnId"])
        self.assertFalse(self.runtime.agent(key)["inFlight"])

    def test_native_failure_keeps_its_error_after_late_acceptance(self):
        key = self.start()
        entry = self.server.deferred[0]
        error = {"message": "Native tool failed"}
        self.server.notify({"method": "turn/completed", "params": {
            "threadId": entry["params"]["threadId"],
            "turn": {**entry["turn"], "status": "failed", "error": error}}})
        self.accept()
        self.assertTrue(entry["handled"].wait(3))
        self.assertEqual(self.runtime.agent(key)["status"], "failed")
        self.assertEqual(self.runtime.agent(key)["error"], error)

    def test_old_reply_confirms_history_without_changing_new_turn(self):
        key = self.start("completed")
        self.server.mode = "started"
        self.runtime.send(key, "Second input")
        self.wait_start(1)
        self.accept(0)
        first_id = self.server.deferred[0]["params"]["clientUserMessageId"]
        eventually(lambda: any(e["id"] == first_id and e["status"] == "delivered" for e in self.events()))
        self.assertEqual(self.runtime.agent(key)["turnId"], "deferred-1")
        self.assertTrue(self.runtime.agent(key)["inFlight"])
        self.server.notify({"method": "turn/started", "params": {
            "threadId": self.server.deferred[0]["params"]["threadId"], "turn": self.server.deferred[0]["turn"]}})
        self.assertEqual(self.runtime.agent(key)["turnId"], "deferred-1")

    def test_old_error_cannot_fail_a_newer_turn(self):
        key = self.start("completed")
        self.server.mode = "started"
        self.runtime.send(key, "Second input")
        self.wait_start(1)
        self.reject(message="late rejection")
        self.assertEqual(self.runtime.agent(key)["status"], "running")
        self.assertEqual(self.runtime.agent(key)["turnId"], "deferred-1")

    def test_unknown_holds_reservation_until_definitive_rejection(self):
        key = self.start("silent")
        self.assertEqual(self.runtime.agent(key)["status"], "starting")
        self.assertTrue(self.runtime.agent(key)["inFlight"])
        self.assertEqual(self.events()[0]["status"], "uncertain")
        self.runtime.send(key, "Second input")
        self.runtime.dispatch()
        self.assertEqual(len(self.server.deferred), 1)
        self.reject()
        eventually(lambda: self.runtime.agent(key)["status"] == "failed")
        self.assertFalse(self.runtime.agent(key)["inFlight"])
        self.assertEqual(len(self.server.deferred), 1)

    def test_immediate_rejection_releases_reservation(self):
        self.server.mode = "silent"
        with patch.object(self.server, "wait", side_effect=RuntimeError("invalid turn parameters")):
            a = self.runtime.create({"name": "Lead", "cwd": self.temp.name, "prompt": "Input"})
            eventually(lambda: self.runtime.agent(a["id"])["status"] == "failed")
        self.assertFalse(self.runtime.agent(a["id"])["inFlight"])
        self.assertEqual(len(self.server.deferred), 1)

    def test_unknown_late_success_recovers_running_without_resending(self):
        key = self.start("silent")
        self.accept()
        eventually(lambda: self.runtime.agent(key)["status"] == "running")
        self.assertEqual(self.events()[0]["status"], "delivered")
        self.assertTrue(self.runtime.agent(key)["inFlight"])
        self.assertEqual(len(self.server.deferred), 1)

    def test_notification_after_timeout_clears_only_dispatch_error(self):
        key = self.start("silent")
        entry = self.server.deferred[0]
        self.assertIn("outcome unknown", self.runtime.agent(key)["error"])
        self.server.notify({"method": "turn/started", "params": {
            "threadId": entry["params"]["threadId"], "turn": entry["turn"]}})
        self.assertEqual(self.runtime.agent(key)["status"], "running")
        self.assertIsNone(self.runtime.agent(key)["error"])
        self.notify_input()
        self.assertEqual(self.events()[0]["status"], "delivered")
        self.accept()
        self.assertTrue(entry["handled"].wait(3))
        self.assertIsNone(self.runtime.agent(key)["error"])

    def test_stop_before_late_acceptance_interrupts_without_resuming(self):
        key = self.start("silent")
        self.runtime.stop(key)
        stopped = self.runtime.agent(key)
        self.accept()
        eventually(lambda: any(method == "turn/interrupt" for method, _ in self.server.calls))
        eventually(lambda: not self.runtime.agent(key)["inFlight"])
        a = self.runtime.agent(key)
        self.assertEqual(a["status"], "paused")
        self.assertEqual(a["epoch"], stopped["epoch"])
        self.assertFalse(a["autoWake"])

    def test_stop_and_resume_before_timeout_keeps_old_epoch_reserved(self):
        self.server.mode = "silent"
        self.server.wait_gate = threading.Event()
        a = self.runtime.create({"name": "Lead", "cwd": self.temp.name, "prompt": "Input"})
        eventually(lambda: len(self.server.deferred) == 1)
        self.assertTrue(self.server.deferred[0]["prepared"].wait(3))
        self.runtime.stop(a["id"])
        self.runtime.send(a["id"], "Resume after old request ends")
        self.server.wait_gate.set()
        self.wait_start(0)
        self.runtime.dispatch()
        self.assertEqual(len(self.server.deferred), 1)
        self.assertTrue(self.runtime.agent(a["id"])["inFlight"])
        self.assertEqual(self.runtime.agent(a["id"])["status"], "queued")
        self.server.mode = "started"
        self.accept()
        self.wait_start(1)
        self.assertEqual(self.runtime.agent(a["id"])["turnId"], "deferred-1")
        self.assertEqual(self.runtime.agent(a["id"])["epoch"], 1)

    def test_disconnection_invalidates_late_state_updates(self):
        key = self.start("silent")
        self.runtime.disconnected()
        self.accept()
        self.assertTrue(self.server.deferred[0]["handled"].wait(3))
        self.assertEqual(self.events()[0]["status"], "uncertain")
        a = self.runtime.agent(key)
        self.assertEqual(a["status"], "interrupted")
        self.assertFalse(a["inFlight"])
        self.assertFalse(a["autoWake"])


class RpcWaitContract(unittest.TestCase):
    def server(self):
        server = AppServer.__new__(AppServer)
        server.lock = threading.RLock()
        server.pending = {}
        server.closed = True
        server.log = io.BytesIO()
        server.notification = lambda _: None
        server.request = lambda _: None
        return server

    def test_timeout_keeps_future_for_late_response_and_reader_cleans_it(self):
        server = self.server()
        future = concurrent.futures.Future()
        server.pending[7] = future
        submitted = (7, "turn/start", future)
        with self.assertRaises(ResponseTimeout):
            server.wait(submitted, timeout=0)
        self.assertIs(server.pending[7], future)
        received = []
        server.on_result(submitted, lambda done: received.append(done.result()))
        server.proc = type("Process", (), {"stdout": io.StringIO(json.dumps({"id": 7, "result": {"turn": {"id": "accepted"}}}) + "\n")})()
        server.read()
        self.assertEqual(received, [{"turn": {"id": "accepted"}}])
        self.assertEqual(server.pending, {})
        self.assertEqual(server.wait(submitted, timeout=0), received[0])

    def test_disconnect_completes_and_removes_unanswered_future(self):
        server = self.server()
        future = concurrent.futures.Future()
        server.pending[8] = future
        server.proc = type("Process", (), {"stdout": io.StringIO("")})()
        server.read()
        self.assertEqual(server.pending, {})
        with self.assertRaisesRegex(RuntimeError, "disconnected; outcome unknown"):
            future.result()

    def test_partial_write_keeps_exact_future_for_late_reply(self):
        server = self.server()
        server.sequence = 0
        written = []
        def partial_write(message):
            written.append(message)
            raise OSError("flush failed after request write")
        server.write = partial_write
        with self.assertRaises(SubmissionUnknown) as caught:
            server.submit("turn/start", {"threadId": "thread"})
        submitted = caught.exception.submitted
        self.assertIs(server.pending[written[0]["id"]], submitted[2])
        with patch.object(server, "submit", side_effect=caught.exception):
            self.assertIs(Runtime.submit_reserved(server, "turn/start", {}), submitted)
        server.proc = type("Process", (), {"stdout": io.StringIO(json.dumps({
            "id": written[0]["id"], "result": {"turn": {"id": "accepted-after-write"}}}) + "\n")})()
        server.read()
        self.assertEqual(server.wait(submitted, 0)["turn"]["id"], "accepted-after-write")
        self.assertEqual(server.pending, {})


if __name__ == "__main__":
    unittest.main()
