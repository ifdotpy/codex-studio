#!/usr/bin/env python3
"""Claude session controls use a live retained session and durable receipts."""
from __future__ import annotations

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
from http_backend_harness import (
    TimedHTTPContractTest,
    no_write_snapshot,
    start_contract_backend,
)


class ClaudeControlsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls)

    def headers(self):
        return {
            "Origin": self.backend.origin,
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_missing_agent_is_rejected_without_writes(self):
        # No agents or events exist, so no preparation or dispatch can race the
        # snapshot. Both backends validate the actor before any provider call.
        for action, arguments in (
            ("state", {}),
            ("commands", {}),
            ("command", {"command": "/help", "request_id": "p08-command"}),
            ("settings", {"settings": {}, "request_id": "p08-settings"}),
            ("rollback", {"turn_id": "turn", "request_id": "p08-rollback"}),
            ("stop_task", {"task_id": "task"}),
        ):
            with self.subTest(action=action):
                before = no_write_snapshot(self.backend.state)
                response = self.backend.request(
                    "POST", "/api/claude/session",
                    body={"id": "missing", "action": action, **arguments},
                    headers=self.headers(),
                )
                self.assertEqual(response.status, 400, response.body)
                self.assertEqual(response.json(), {"error": "Unknown managed agent"})
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_invalid_action_is_located_before_the_dependency(self):
        before = no_write_snapshot(self.backend.state)
        response = self.backend.request(
            "POST", "/api/claude/session",
            body={"id": "missing", "action": "unknown"}, headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body)
        self.assertIn("location", response.json())
        self.assertEqual(no_write_snapshot(self.backend.state), before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
