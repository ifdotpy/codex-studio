#!/usr/bin/env python3
"""Steer retries distinguish an unsent request from an unknown outcome."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_native_errors import NativeRpcError
from codex_runtime import Runtime, SubmissionRejected

spec = importlib.util.spec_from_file_location(
    "steer_fixture", Path(__file__).with_name("runtime-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class SteerServer(fixture.FakeServer):
    rejection = None
    interrupt_error = None

    def call(self, method, params, timeout=60):
        if method == "turn/interrupt" and self.interrupt_error:
            self.calls.append((method, params))
            raise NativeRpcError(self.interrupt_error)
        if method == "turn/steer":
            self.calls.append((method, params))
            if self.rejection:
                raise NativeRpcError(self.rejection)
            return {"turnId": params["expectedTurnId"]}
        return super().call(method, params, timeout)


class CriticalSteerContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, SteerServer)
        self.server = self.runtime.connect()

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def lead(self):
        agent = self.runtime.create(
            {"name": "Lead", "cwd": str(self.root), "prompt": "Task"},
            defer=True,
        )
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent["id"], db)
            current.update(
                status="running",
                autoWake=True,
                threadId="thread-1",
                turnId="turn-1",
                inFlight=True,
            )
            self.runtime.put(db, "agents", current)
        return agent["id"]

    def metadata(self, message_id):
        with self.runtime.db() as db:
            return json.loads(
                db.execute(
                    "SELECT record FROM runtime_event_meta WHERE id=?",
                    (message_id,),
                ).fetchone()[0]
            )

    def transcript_count(self, agent, message_id):
        with self.runtime.db() as db:
            return db.execute(
                "SELECT COUNT(*) FROM runtime_items WHERE id=? AND agent=?",
                (agent + ":" + message_id, agent),
            ).fetchone()[0]

    def test_promote_queue_keeps_identity_and_retries_without_resend(self):
        agent = self.lead()
        self.runtime.send(agent, "Queued", "promote-one", delivery="queue")
        body = {"action": "steer", "id": "promote-one", "expectedText": "Queued",
                "request_id": "promote-request", "expected_revision": self.runtime.queue_action(agent)["revision"]}
        result = self.runtime.queue_action(agent, body)
        self.assertEqual(result["status"], "delivered")
        self.assertEqual(self.runtime.queue_action(agent, body), result)
        calls = [p for m, p in self.server.calls if m == "turn/steer"]
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]["clientUserMessageId"], "promote-one")
        self.assertEqual(self.runtime.queue_action(agent)["items"], [])

    def test_async_answer_steers_active_turn_once(self):
        agent = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(agent, db)
            self.runtime.put(db, "requests", {"id": "async-question", "agent": agent,
                "epoch": a["epoch"], "method": "agent/asyncQuestion", "status": "pending",
                "params": {"questions": [{"id": "q", "question": "Which file?"}]}})
        body = {"answers": {"q": {"answers": ["main.py"]}}}
        self.runtime.answer("async-question", body)
        self.runtime.answer("async-question", body)
        calls = [p for m, p in self.server.calls if m == "turn/steer"]
        self.assertEqual(len(calls), 1)
        self.assertIn("main.py", calls[0]["input"][0]["text"])

    def test_after_tool_resolves_at_server_and_replay_keeps_original_route(self):
        agent = self.lead()
        result = self.runtime.send(agent, "Correction", "enter-active", delivery="after_tool")
        self.assertEqual(result["status"], "delivered")
        self.assertEqual(self.metadata("enter-active")["delivery"], "steer")
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(agent, db)
            a.update(inFlight=False, turnId=None, status="idle")
            self.runtime.put(db, "agents", a)
        with patch.object(self.server, "submit") as submit:
            self.assertEqual(self.runtime.send(agent, "Correction", "enter-active", delivery="after_tool")["status"], "delivered")
            submit.assert_not_called()
        result = self.runtime.send(agent, "Start next", "enter-idle", delivery="after_tool")
        self.assertEqual(result["status"], "queued")
        self.assertEqual(self.metadata("enter-idle")["delivery"], "queue")
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(agent, db)
            a.update(inFlight=True, turnId="later-turn", status="running")
            self.runtime.put(db, "agents", a)
        with patch.object(self.server, "submit") as submit:
            self.runtime.send(agent, "Start next", "enter-idle", delivery="after_tool")
            submit.assert_not_called()
        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.send(agent, "Start next", "enter-idle", delivery="queue")

    def test_after_tool_unknown_delivery_never_becomes_a_second_message(self):
        agent = self.lead()
        with patch.object(self.server, "submit", side_effect=OSError("pipe disconnected")):
            with self.assertRaisesRegex(RuntimeError, "outcome unknown"):
                self.runtime.send(agent, "Correction", "enter-unknown", delivery="after_tool")
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(agent, db)
            a.update(inFlight=False, turnId=None, status="idle")
            self.runtime.put(db, "agents", a)
        with patch.object(self.server, "submit") as submit:
            self.assertEqual(self.runtime.send(agent, "Correction", "enter-unknown", delivery="after_tool")["status"], "uncertain")
            submit.assert_not_called()
        self.assertEqual(self.runtime.queue_action(agent)["items"], [])

    def test_unsent_steer_retry_preserves_identity_and_transcript(self):
        agent = self.lead()
        message_id = "steer-unsent"
        with patch.object(
            self.server,
            "submit",
            side_effect=SubmissionRejected("input busy; request was not submitted"),
        ):
            with self.assertRaises(SubmissionRejected):
                self.runtime.send(agent, "Correction", message_id, delivery="steer")

        receipt = self.runtime.delivery_receipt(message_id)
        self.assertEqual(receipt["status"], "failed")
        self.assertTrue(self.metadata(message_id)["notSubmitted"])
        self.assertEqual(self.transcript_count(agent, message_id), 1)

        with self.assertRaisesRegex(ValueError, "different content"):
            self.runtime.send(agent, "Changed", message_id, delivery="steer")

        retry = self.runtime.send(agent, "Correction", message_id, delivery="steer")
        self.assertEqual(retry["status"], "delivered")
        self.assertEqual(
            len([method for method, _ in self.server.calls if method == "turn/steer"]),
            1,
        )
        self.assertEqual(self.transcript_count(agent, message_id), 1)
        self.assertFalse(self.metadata(message_id).get("notSubmitted", False))

    def test_offline_steer_is_not_submitted_and_can_retry(self):
        agent = self.lead()
        message_id = "steer-offline"
        with patch.object(self.server, "submit", side_effect=RuntimeError("Codex app-server is offline")):
            with self.assertRaisesRegex(RuntimeError, "offline"):
                self.runtime.send(agent, "Correction", message_id, delivery="steer")
        receipt = self.runtime.delivery_receipt(message_id)
        self.assertEqual(receipt["status"], "failed")
        self.assertTrue(self.metadata(message_id)["notSubmitted"])
        retry = self.runtime.send(agent, "Correction", message_id, delivery="steer")
        self.assertEqual(retry["status"], "delivered")
        self.assertEqual(self.transcript_count(agent, message_id), 1)

    def test_saved_offline_steer_uncertainty_is_settled_by_repair(self):
        from codex_context_repair import _settle_offline_steer
        agent = self.lead()
        message_id = "steer-saved-offline"
        with patch.object(self.server, "submit", side_effect=RuntimeError("write failed")):
            with self.assertRaises(RuntimeError):
                self.runtime.send(agent, "Correction", message_id, delivery="steer")
        self.assertEqual(self.runtime.delivery_receipt(message_id)["status"], "uncertain")
        with self.runtime.lock, self.runtime.db() as db:
            # Older code saved an offline steer as uncertain; unrelated errors stay uncertain.
            row = db.execute("SELECT e.*,m.record AS metadata FROM runtime_events e LEFT JOIN runtime_event_meta m "
                             "ON m.id=e.id WHERE e.id=?", (message_id,)).fetchone()
            self.assertFalse(_settle_offline_steer(db, row))
            db.execute("UPDATE runtime_events SET error='Codex app-server is offline' WHERE id=?", (message_id,))
            row = db.execute("SELECT e.*,m.record AS metadata FROM runtime_events e LEFT JOIN runtime_event_meta m "
                             "ON m.id=e.id WHERE e.id=?", (message_id,)).fetchone()
            self.assertTrue(_settle_offline_steer(db, row))
        self.assertEqual(self.runtime.delivery_receipt(message_id)["status"], "failed")
        self.assertTrue(self.metadata(message_id)["notSubmitted"])

    def test_unsent_retry_rechecks_turn_identity_without_project_restrictions(self):
        agent = self.lead()
        message_id = "steer-permission"
        with patch.object(
            self.server,
            "submit",
            side_effect=SubmissionRejected("input busy; request was not submitted"),
        ):
            with self.assertRaises(SubmissionRejected):
                self.runtime.send(agent, "Correction", message_id, delivery="steer")

        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent, db)
            current["inFlight"] = False
            self.runtime.put(db, "agents", current)
        with self.assertRaisesRegex(ValueError, "no active turn"):
            self.runtime.send(agent, "Correction", message_id, delivery="steer")
        self.assertEqual(self.runtime.delivery_receipt(message_id)["status"], "failed")

        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent, db)
            current["inFlight"] = True
            self.runtime.put(db, "agents", current)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent, db)
            current["turnId"] = "turn-2"
            self.runtime.put(db, "agents", current)
        with self.assertRaisesRegex(ValueError, "earlier turn"):
            self.runtime.send(agent, "Correction", message_id, delivery="steer")

        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent, db)
            current["turnId"] = "turn-1"
            self.runtime.put(db, "agents", current)
        self.runtime.accounts.data["accounts"]["default"]["projectRules"] = {
            "allowedProjects": [], "revision": 1,
        }
        result = self.runtime.send(agent, "Correction", message_id, delivery="steer")
        self.assertEqual(result["status"], "delivered")
        self.assertEqual(
            len([method for method, _ in self.server.calls if method == "turn/steer"]),
            1,
        )
        self.assertEqual(self.transcript_count(agent, message_id), 1)

    def test_steer_after_the_turn_ended_waits_in_the_queue(self):
        rejections = [
            {"code": -32000, "message": "The steer belongs to a different Claude turn"},
            {"code": -32000, "message": "Claude finished before this steer could be submitted"},
            {"code": -32600, "message": "no active turn to steer"},
            {"code": -32600, "message": "expected active turn id `turn-1` but found `turn-2`"},
        ]
        agent = self.lead()
        for index, rejection in enumerate(rejections):
            message_id = "steer-ended-" + str(index)
            self.server.rejection = rejection
            result = self.runtime.send(agent, "Correction " + str(index), message_id, delivery="steer")
            self.assertEqual(result, {"id": message_id, "status": "queued", "delivery": "queue"})
            receipt = self.runtime.delivery_receipt(message_id)
            self.assertEqual((receipt["status"], receipt["error"]), ("pending", None))
            meta = self.metadata(message_id)
            self.assertEqual(meta["delivery"], "queue")
            self.assertNotIn("transcriptItemId", meta)
            self.assertEqual(self.transcript_count(agent, message_id), 0)
            with self.runtime.db() as db:
                self.assertIsNone(db.execute("SELECT turn_id FROM runtime_events WHERE id=?",
                                             (message_id,)).fetchone()[0])
            # The same identity does not submit or queue the message again.
            self.assertEqual(self.runtime.send(agent, "Correction " + str(index), message_id,
                                               delivery="steer")["status"], "pending")
        self.assertEqual(len([m for m, _ in self.server.calls if m == "turn/steer"]), len(rejections))
        queued = [item["id"] for item in self.runtime.queue_action(agent)["items"]]
        self.assertEqual(queued, ["steer-ended-" + str(index) for index in range(len(rejections))])

    def test_other_steer_rejections_still_fail(self):
        agent = self.lead()
        self.server.rejection = {"code": -32600, "message": "input must not be empty"}
        with self.assertRaises(NativeRpcError):
            self.runtime.send(agent, "Correction", "steer-invalid", delivery="steer")
        self.assertEqual(self.runtime.delivery_receipt("steer-invalid")["status"], "failed")

    def test_stop_without_a_native_turn_releases_the_agent(self):
        agent = self.lead()
        self.server.interrupt_error = {"code": -32600, "message": "no active turn to interrupt"}
        self.runtime.stop(agent)
        stopped = self.runtime.agent(agent)
        self.assertEqual((stopped["status"], stopped["inFlight"], stopped["turnId"]), ("paused", False, None))
        self.assertEqual(stopped["error"], "Stopped. Codex reported no active turn.")

    def test_stop_with_an_unknown_interrupt_outcome_keeps_the_turn(self):
        agent = self.lead()
        self.server.interrupt_error = {"code": -32603, "message": "internal error"}
        self.runtime.stop(agent)
        stopped = self.runtime.agent(agent)
        self.assertEqual((stopped["inFlight"], stopped["turnId"]), (True, "turn-1"))
        self.assertIn("interrupt acknowledgement unavailable", stopped["error"])

    def test_long_turn_gets_one_notice_about_waiting_inputs(self):
        import time
        agent = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(agent, db)
            self.runtime.enqueue(db, current, "monitor_exit", json.dumps({"id": "m1"}), "monitor:first")
            self.runtime.enqueue(db, current, "monitor_exit", json.dumps({"id": "m"}), "monitor:waiting")
        steers = lambda: [p for m, p in self.server.calls if m == "turn/steer"]
        self.runtime.dispatch()
        self.runtime.pool.submit(lambda: None).result(5)
        self.assertEqual(steers(), [])
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET created=? WHERE id IN ('monitor:first','monitor:waiting')",
                       (time.time() - 1000,))
        self.runtime.dispatch()
        fixture.eventually(lambda: len(steers()) == 1)
        text = steers()[0]["input"][0]["text"]
        self.assertIn("2 inputs wait for your next turn (2 monitor results)", text)
        self.assertEqual(steers()[0]["expectedTurnId"], "turn-1")
        self.runtime.dispatch()
        self.runtime.pool.submit(lambda: None).result(5)
        self.assertEqual(len(steers()), 1)
        with self.runtime.db() as db:
            statuses = [r[0] for r in db.execute(
                "SELECT status FROM runtime_events WHERE id IN ('monitor:first','monitor:waiting')")]
        self.assertEqual(statuses, ["pending", "pending"])

    def test_unknown_write_stays_nonretryable(self):
        agent = self.lead()
        message_id = "steer-unknown"
        with patch.object(self.server, "submit", side_effect=OSError("pipe disconnected")):
            with self.assertRaisesRegex(RuntimeError, "outcome unknown"):
                self.runtime.send(agent, "Correction", message_id, delivery="steer")

        self.assertEqual(self.runtime.delivery_receipt(message_id)["status"], "uncertain")
        self.assertNotIn("notSubmitted", self.metadata(message_id))
        with patch.object(self.server, "submit") as submit:
            retry = self.runtime.send(agent, "Correction", message_id, delivery="steer")
        self.assertEqual(retry["status"], "uncertain")
        submit.assert_not_called()


if __name__ == "__main__":
    unittest.main()
