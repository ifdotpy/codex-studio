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
from codex_runtime import Runtime, SubmissionRejected

spec = importlib.util.spec_from_file_location(
    "steer_fixture", Path(__file__).with_name("runtime-contract.py")
)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class SteerServer(fixture.FakeServer):
    def call(self, method, params, timeout=60):
        if method == "turn/steer":
            self.calls.append((method, params))
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

    def test_unsent_retry_rechecks_current_turn_and_project(self):
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
        rules = self.runtime.accounts.get("default")["projectRules"]
        self.runtime.accounts.set_project_rules("default", [], rules["revision"])
        with self.assertRaisesRegex(ValueError, "cannot use project"):
            self.runtime.send(agent, "Correction", message_id, delivery="steer")
        self.assertEqual(self.runtime.delivery_receipt(message_id)["status"], "failed")
        self.assertEqual(
            len([method for method, _ in self.server.calls if method == "turn/steer"]),
            0,
        )

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
