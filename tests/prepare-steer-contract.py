#!/usr/bin/env python3
"""Preparation, steering, and native action receipts with delayed acknowledgements."""
import concurrent.futures
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_runtime import PreparationPending, ResponseTimeout, Runtime

spec = importlib.util.spec_from_file_location("prepare_fixture", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
eventually = fixture.eventually


class DelayedServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.hold = set()
        self.delayed = []

    def submit(self, method, params):
        if method not in self.hold:
            return super().submit(method, params)
        future = concurrent.futures.Future()
        self.calls.append((method, params))
        self.delayed.append({"method": method, "params": params, "future": future})
        return future

    def wait(self, future, timeout=60):
        if any(entry["future"] is future for entry in self.delayed):
            raise ResponseTimeout("Native response timed out; outcome unknown")
        return super().wait(future, timeout)

    def close(self):
        for entry in self.delayed:
            if not entry["future"].done():
                entry["future"].set_exception(RuntimeError("Codex disconnected; outcome unknown"))
        super().close()


class PrepareSteerContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, DelayedServer)
        self.runtime.preparation_wait_seconds = .01
        self.server = self.runtime.connect()

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def create(self, **extra):
        return self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": "First", **extra})

    def lead(self):
        a = self.create()
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "running")
        return self.runtime.agent(a["id"])

    def pending_prepare(self, a):
        eventually(lambda: "preparation acknowledgement" in str(self.runtime.agent(a["id"]).get("error")))
        return next(e for e in self.server.delayed if e["method"] in {"thread/start", "thread/resume"})

    def accept_prepare(self, entry, **extra):
        result = {"thread": {"id": entry["params"].get("threadId", "new-prepared-thread")},
                  "model": entry["params"].get("model"), "sandbox": {"type": "readOnly"}, "approvalPolicy": "on-request"}
        result.update(extra)
        entry["future"].set_result(result)

    def count(self, method):
        return sum(m == method for m, _ in self.server.calls)

    def test_late_new_thread_continues_exact_batch_once(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        self.assertTrue(self.runtime.agent(a["id"])["inFlight"])
        self.assertEqual(self.runtime.snapshot()["events"][0]["status"], "reserved")
        self.runtime.send(a["id"], "Second")
        self.runtime.dispatch()
        self.assertEqual(self.count("thread/start"), 1)
        self.assertEqual(self.count("turn/start"), 0)
        self.accept_prepare(entry)
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "running")
        self.assertEqual(self.count("turn/start"), 1)
        self.assertEqual(self.runtime.agent(a["id"])["threadId"], "new-prepared-thread")
        self.assertIsNone(self.runtime.agent(a["id"])["error"])

    def test_late_resume_uses_same_thread(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.runtime.loaded.discard(a["id"])
        self.server.hold.add("thread/resume")
        self.runtime.send(a["id"], "Continue")
        entry = self.pending_prepare(a)
        self.accept_prepare(entry)
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "running")
        self.assertEqual(self.runtime.agent(a["id"])["threadId"], a["threadId"])
        self.assertEqual(self.count("thread/resume"), 1)
        self.assertEqual(self.count("turn/start"), 2)

    def test_concurrent_preparation_shares_one_native_request(self):
        self.server.hold.add("thread/start")
        a = self.runtime.create({"name": "Idle", "cwd": str(self.root), "prompt": "First"}, defer=True)
        def prepare():
            try:
                self.runtime.prepare(a)
            except PreparationPending as error:
                return error.future
        with concurrent.futures.ThreadPoolExecutor(2) as pool:
            futures = list(pool.map(lambda _: prepare(), range(2)))
        self.assertIs(futures[0], futures[1])
        self.assertEqual(self.count("thread/start"), 1)
        self.accept_prepare(self.server.delayed[0])
        self.assertEqual(self.runtime.prepare(a)["threadId"], "new-prepared-thread")

    def test_stopped_preparation_never_submits_turn_or_changes_thread(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        self.runtime.stop(a["id"])
        self.accept_prepare(entry)
        eventually(lambda: not self.runtime.agent(a["id"])["inFlight"])
        current = self.runtime.agent(a["id"])
        self.assertEqual(current["status"], "paused")
        self.assertIsNone(current["threadId"])
        self.assertNotIn(a["id"], self.runtime.loaded)
        self.assertEqual(self.count("turn/start"), 0)

    def test_definitive_preparation_failure_is_not_uncertain_input(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        entry["future"].set_exception(RuntimeError("invalid project"))
        eventually(lambda: self.runtime.agent(a["id"])["status"] == "failed")
        self.assertEqual(self.runtime.snapshot()["events"][0]["status"], "failed")
        self.assertEqual(self.count("turn/start"), 0)

    def test_disconnect_finishes_shared_preparation_without_accepting_late_identity(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        self.runtime.disconnected()
        self.assertTrue(self.runtime.preparations[a["id"]]["future"].done())
        self.accept_prepare(entry)
        self.assertEqual(self.runtime.agent(a["id"])["status"], "interrupted")
        self.assertIsNone(self.runtime.agent(a["id"])["threadId"])
        self.assertNotIn(a["id"], self.runtime.loaded)
        self.assertEqual(self.count("turn/start"), 0)

    def test_preparation_connection_and_configuration_are_fenced(self):
        self.server.hold.add("thread/start")
        a = self.create()
        entry = self.pending_prepare(a)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a["id"], db)
            current["model"] = "changed-model"
            self.runtime.put(db, "agents", current)
        self.accept_prepare(entry)
        eventually(lambda: self.runtime.preparations[a["id"]]["future"].done())
        self.assertEqual(self.runtime.agent(a["id"])["model"], "changed-model")
        self.assertIsNone(self.runtime.agent(a["id"])["threadId"])
        self.assertEqual(self.count("turn/start"), 0)

    def test_monitor_preparation_blocks_model_edit(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.runtime.loaded.discard(a["id"])
        self.server.hold.add("thread/resume")
        m = self.runtime.monitor(a["id"], {"command": "example-command"}, approved=True)
        eventually(lambda: bool(self.server.delayed))
        with self.assertRaisesRegex(ValueError, "preparation"):
            self.runtime.conversation_settings(a["id"], {"model": "gpt-5.6-sol"})
        self.accept_prepare(self.server.delayed[0])
        eventually(lambda: self.count("command/exec") == 1)
        self.assertEqual(next(v for v in self.runtime.snapshot()["monitors"] if v["id"] == m["id"])["status"], "running")

    def steer(self, a, message_id="steer-1"):
        self.server.hold.add("turn/steer")
        result = self.runtime.send(a["id"], "Correction", message_id, delivery="steer")
        entry = next(e for e in self.server.delayed if e["method"] == "turn/steer")
        return result, entry

    def test_late_steer_ack_confirms_original_receipt_once(self):
        a = self.lead()
        result, entry = self.steer(a)
        self.assertEqual(result["status"], "uncertain")
        self.assertEqual(self.runtime.send(a["id"], "Correction", "steer-1", delivery="steer")["status"], "uncertain")
        entry["future"].set_result({"turnId": a["turnId"]})
        eventually(lambda: self.runtime.delivery_receipt("steer-1")["status"] == "delivered")
        self.assertEqual(self.count("turn/steer"), 1)
        self.assertEqual(self.count("turn/start"), 1)

    def test_steer_client_proof_survives_late_error_after_completion(self):
        a = self.lead()
        _, entry = self.steer(a)
        self.server.complete(a["threadId"], a["turnId"])
        self.server.notify({"method": "item/completed", "params": {"threadId": a["threadId"], "turnId": a["turnId"],
            "item": {"type": "userMessage", "id": "native-user", "clientId": "steer-1", "content": []}}})
        self.assertEqual(self.runtime.delivery_receipt("steer-1")["status"], "delivered")
        entry["future"].set_exception(RuntimeError("late conflicting error"))
        self.runtime.steer_result("steer-1", self.native_receipt("steer-1"), entry["future"])
        self.assertEqual(self.runtime.delivery_receipt("steer-1")["status"], "delivered")
        self.assertEqual(self.runtime.agent(a["id"])["status"], "completed")

    def native_receipt(self, key):
        with self.runtime.db() as db:
            return json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (key,)).fetchone()[0])["native"]

    def test_steer_wrong_turn_or_old_connection_cannot_confirm(self):
        a = self.lead()
        _, entry = self.steer(a)
        entry["future"].set_result({"turnId": "wrong-turn"})
        self.runtime.steer_result("steer-1", self.native_receipt("steer-1"), entry["future"])
        self.assertEqual(self.runtime.delivery_receipt("steer-1")["status"], "uncertain")
        self.runtime.connection_ids["default"] = "replacement"
        self.runtime.steer_accepted("steer-1", self.native_receipt("steer-1"), {"turnId": a["turnId"]})
        self.assertEqual(self.runtime.delivery_receipt("steer-1")["status"], "uncertain")

    def test_steer_write_failure_keeps_exact_uncertain_receipt(self):
        a = self.lead()
        with patch.object(self.server, "submit", side_effect=OSError("write disconnected")):
            with self.assertRaisesRegex(RuntimeError, "outcome unknown"):
                self.runtime.send(a["id"], "Correction", "steer-write", delivery="steer")
        self.assertEqual(self.runtime.delivery_receipt("steer-write")["status"], "uncertain")
        self.runtime.send(a["id"], "Correction", "steer-write", delivery="steer")
        self.assertEqual(self.count("turn/steer"), 0)

    def test_native_review_late_ack_preserves_completed_turn(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.server.hold.add("review/start")
        result = self.runtime.native_action(a["id"], "review")
        self.assertTrue(result["pending"])
        turn = {"id": "review-turn", "status": "inProgress"}
        self.server.notify({"method": "turn/started", "params": {"threadId": a["threadId"], "turn": turn}})
        self.server.notify({"method": "item/started", "params": {"threadId": a["threadId"], "turnId": turn["id"],
            "item": {"id": "review-user", "type": "userMessage", "clientId": None, "content": []}}})
        self.server.complete(a["threadId"], turn["id"])
        entry = next(e for e in self.server.delayed if e["method"] == "review/start")
        entry["future"].set_result({"turn": turn})
        self.runtime.native_action_result(a["id"], dict(self.runtime.agent(a["id"])["startAttempt"]), entry["future"])
        self.assertEqual(self.runtime.agent(a["id"])["status"], "completed")
        self.assertFalse(self.runtime.agent(a["id"])["inFlight"])

    def test_stop_during_native_action_preparation_prevents_submission(self):
        a = self.lead()
        self.server.complete(a["threadId"], a["turnId"])
        self.runtime.loaded.discard(a["id"])
        self.server.hold.add("thread/resume")
        result = self.runtime.native_action(a["id"], "review")
        self.assertTrue(result["pending"])
        self.runtime.stop(a["id"])
        self.accept_prepare(self.server.delayed[0])
        eventually(lambda: not self.runtime.agent(a["id"])["inFlight"])
        self.assertEqual(self.count("review/start"), 0)
        self.assertEqual(self.runtime.agent(a["id"])["status"], "paused")


if __name__ == "__main__":
    unittest.main()
