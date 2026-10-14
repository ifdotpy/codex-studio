#!/usr/bin/env python3
"""Recovery validates requests and refuses without recovery-owned writes."""
import os
from pathlib import Path
import sqlite3
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def recovery_rows(state):
    # Empty fixture: no agent can dispatch. Only recovery-owned tables are
    # observed, avoiding startup account/catalog bookkeeping.
    with sqlite3.connect(f"file:{state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
        return {name: db.execute(f'SELECT * FROM "{name}" ORDER BY 1').fetchall()
                for name in ("runtime_agents", "runtime_events", "runtime_tasks", "runtime_items", "runtime_completed_turns")}


class RecoveryHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls)

    def headers(self):
        return {"Origin": self.backend.origin, "X-Canvas-Token": self.backend.session["token"]}

    def test_refusal_preserves_all_recovery_rows(self):
        before = recovery_rows(self.backend.state)
        response = self.backend.request("POST", "/api/connection-recovery", body={"id": "missing"}, headers=self.headers())
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(response.status, 501, response.body)
            self.assertEqual(response.json(), {"error": "Not implemented: requires account-worker/recovery-history"})
        else:
            self.assertEqual(response.status, 400, response.body)
            self.assertEqual(response.json(), {"error": "Unknown managed agent"})
        self.assertEqual(recovery_rows(self.backend.state), before)

    def test_invalid_body_is_located_before_dependency(self):
        before = recovery_rows(self.backend.state)
        response = self.backend.request("POST", "/api/connection-recovery", body={"id": 7}, headers=self.headers())
        self.assertEqual(response.status, 400, response.body)
        self.assertIn("location", response.json())
        self.assertEqual(recovery_rows(self.backend.state), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
