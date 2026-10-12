#!/usr/bin/env python3
"""Tool request endpoint contracts and explicit Rust capability gaps."""

from __future__ import annotations

import os
import sqlite3
import sys
import unittest
from pathlib import Path
from urllib.parse import urlencode

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parent))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def request_state_snapshot(state: Path):
    with sqlite3.connect(f"file:{state / 'canvas.sqlite3'}?mode=ro", uri=True) as database:
        tables = database.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN "
            "('runtime_tool_requests', 'runtime_tool_request_aliases') ORDER BY name"
        ).fetchall()
        return {
            name: database.execute(f'SELECT * FROM "{name}" ORDER BY rowid').fetchall()
            for (name,) in tables
        }


class ToolRequestsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        if cls.rust_backend:
            cls.lead_id = "tool-requests-contract-agent"
            return
        cls.lead = cls.backend.request(
            "POST", "/api/leads",
            body={"cwd": str(cls.backend.cwd), "workspaceMode": "worktree"},
            headers=cls.headers(),
        )
        if cls.lead.status != 200:
            raise AssertionError(f"Lead fixture failed: HTTP {cls.lead.status}")
        cls.lead_id = cls.lead.json()["id"]

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    @classmethod
    def headers(cls):
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def test_request_listing_is_explicit_about_missing_rust_drivers(self):
        before = request_state_snapshot(self.backend.state)
        response = self.backend.request(
            "GET", "/api/tool-requests?" + urlencode({"agent": self.lead_id}),
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn("payload-store/tool-request-list", response.json()["error"])
            self.assertIn("worker-driver/connection-state", response.json()["error"])
        else:
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.json()["requests"], [])
        self.assertEqual(request_state_snapshot(self.backend.state), before)

    def test_cancel_refuses_cleanly_until_lifecycle_driver_exists(self):
        before = request_state_snapshot(self.backend.state)
        response = self.backend.request(
            "POST", "/api/tool-requests/cancel",
            body={"agent": self.lead_id, "request_id": "missing-request"},
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn("tool-driver/request-lifecycle", response.json()["error"])
        else:
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.json()["stage"], "not_found")
            self.assertEqual(response.json()["outcome"], "unknown")
        self.assertEqual(request_state_snapshot(self.backend.state), before)

    def test_invalid_cancel_body_is_rejected_before_dependency(self):
        response = self.backend.request(
            "POST", "/api/tool-requests/cancel", body={"agent": self.lead_id},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])


if __name__ == "__main__":
    unittest.main()
