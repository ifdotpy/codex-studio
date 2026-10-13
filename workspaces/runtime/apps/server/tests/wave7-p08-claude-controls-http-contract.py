#!/usr/bin/env python3
"""The retained-session dependency stays explicit until the worker consumes it."""
from __future__ import annotations

import os
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

    def test_every_unwired_action_refuses_without_writes(self):
        # No agents or events exist, so no preparation or dispatch can race the
        # snapshot. The shared helper owns all background-field exclusions.
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
                if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
                    self.assertEqual(response.status, 501, response.body)
                    self.assertEqual(response.json(), {
                        "error": "Not implemented: requires provider-session/claude-controls"
                    })
                else:
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
