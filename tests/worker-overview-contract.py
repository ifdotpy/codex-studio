#!/usr/bin/env python3
"""Worker snapshot excerpts use completed reports, never streaming commentary."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("runtime_contract", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class WorkerOverviewContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.runtime = fixture.Runtime(self.root, fixture.FakeServer)
        self.lead = self.runtime.create({"name": "Lead", "cwd": str(self.root), "prompt": "Coordinate"}, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            self.lead.update(autoWake=True, status="waiting")
            self.runtime.put(db, "agents", self.lead)
        self.worker = self.runtime.create({"name": "Worker", "prompt": "Review the exact retry receipt", "role": "reviewer"}, parent=self.lead["id"], defer=True)

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def snapshot(self, **updates):
        with self.runtime.lock, self.runtime.db() as db:
            self.worker.update(updates)
            self.runtime.put(db, "agents", self.worker)
        return next(a for a in self.runtime.snapshot()["agents"] if a["id"] == self.worker["id"])

    def test_running_commentary_and_stale_completed_turn_are_not_results(self):
        agent = self.snapshot(status="running", inFlight=True, turnId="new", lastCompletedTurn="old", lastAnswer="I will investigate")
        self.assertEqual(agent["overview"]["task"], "Review the exact retry receipt")
        self.assertEqual(agent["overview"]["result"], "")
        self.assertIsNone(agent["overview"]["resultTurnId"])
        for field in ("prompt", "lastAnswer", "sandbox", "profile", "approvalPolicy"):
            self.assertNotIn(field, agent)
        for status in ("starting", "queued", "waiting", "failed", "interrupted", "paused"):
            self.assertEqual(self.snapshot(status=status, inFlight=False, turnId=None)["overview"]["result"], "")

    def test_completed_report_is_bounded_and_has_turn_identity(self):
        report = "The retry receipt is verified. " + "x" * 4500
        task = "Review " + "y" * 4500
        agent = self.snapshot(status="completed", inFlight=False, turnId=None, lastCompletedTurn="done", lastAnswer=report, prompt=task)
        self.assertEqual(agent["overview"]["task"], task[:4000])
        self.assertEqual(agent["overview"]["result"], report[:4000])
        self.assertTrue(agent["overview"]["taskTruncated"])
        self.assertTrue(agent["overview"]["resultTruncated"])
        self.assertEqual(agent["overview"]["resultTurnId"], "done")
        with self.runtime.db() as db:
            stored = self.runtime.agent(self.worker["id"], db)
        self.assertEqual(stored["prompt"], task)
        self.assertEqual(stored["lastAnswer"], report)
        self.assertNotIn("overview", stored)

    def test_missing_completion_has_no_report_and_lead_is_not_projected(self):
        agent = self.snapshot(status="completed", lastAnswer="Unproven result")
        self.assertEqual(agent["overview"]["result"], "")
        lead = next(a for a in self.runtime.snapshot()["agents"] if a["id"] == self.lead["id"])
        self.assertNotIn("overview", lead)


if __name__ == "__main__":
    unittest.main()
