#!/usr/bin/env python3
"""Worker snapshot excerpts use completed reports, never streaming commentary."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("runtime_contract", Path(__file__).with_name("runtime-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from studio_api.testing import read_runtime_state


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

    def legacy_overview(self, **updates):
        with self.runtime.lock, self.runtime.db() as db:
            self.worker.update(updates)
            self.runtime.put(db, "agents", self.worker)
        snapshot_agents = read_runtime_state(self.runtime)["agents"]
        return next(agent for agent in snapshot_agents if agent["id"] == self.worker["id"])["overview"]

    def test_running_commentary_and_stale_completed_turn_are_not_results(self):
        agent = self.legacy_overview(status="running", inFlight=True, turnId="new", lastCompletedTurn="old", lastAnswer="I will investigate")
        self.assertEqual(agent["task"], "Review the exact retry receipt")
        self.assertEqual(agent["result"], "")
        self.assertIsNone(agent["resultTurnId"])
        for field in ("prompt", "lastAnswer", "sandbox", "profile", "approvalPolicy"):
            self.assertNotIn(field, agent)
        for status in ("starting", "queued", "waiting", "failed", "interrupted", "paused"):
            self.assertEqual(self.legacy_overview(status=status, inFlight=False, turnId=None)["result"], "")

    def test_completed_report_is_bounded_and_has_turn_identity(self):
        report = "The retry receipt is verified. " + "x" * 4500
        task = "Review " + "y" * 4500
        agent = self.legacy_overview(status="completed", inFlight=False, turnId=None, lastCompletedTurn="done", lastAnswer=report, prompt=task)
        self.assertEqual(agent["task"], task[:4000])
        self.assertEqual(agent["result"], report[:4000])
        self.assertTrue(agent["taskTruncated"])
        self.assertTrue(agent["resultTruncated"])
        self.assertEqual(agent["resultTurnId"], "done")
        with self.runtime.db() as db:
            stored = self.runtime.agent(self.worker["id"], db)
        self.assertEqual(stored["prompt"], task)
        self.assertEqual(stored["lastAnswer"], report)
        self.assertNotIn("overview", stored)

    def test_missing_completion_has_no_report_and_lead_is_not_projected(self):
        agent = self.legacy_overview(status="completed", lastAnswer="Unproven result")
        self.assertEqual(agent["result"], "")
        lead = next(a for a in read_runtime_state(self.runtime)["agents"] if a["id"] == self.lead["id"])
        agents = read_runtime_state(self.runtime)["agents"]
        self.assertNotIn("overview", lead)
        with self.runtime.db() as db:
            entity_lead = self.runtime.agent_entity_view(db, self.runtime.agent(lead["id"], db))
        self.assertNotIn("overview", entity_lead)


if __name__ == "__main__":
    unittest.main()
