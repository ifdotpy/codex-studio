#!/usr/bin/env python3
"""Federation messaging boundary: absent trusted-origin adapter refuses without writes."""
from __future__ import annotations
from test_isolation import isolate_supervisor_environment
import os
from pathlib import Path
import sqlite3
import sys
import unittest

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class FederationMessagingHTTP(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls)

    def test_signed_routes_fail_closed_without_a_trusted_origin_adapter(self):
        backend = self.backend
        def state():
            with sqlite3.connect(backend.state / "canvas.sqlite3") as db:
                return {name: db.execute(f'SELECT * FROM "{name}" ORDER BY rowid').fetchall()
                        for (name,) in db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'runtime_federation_%'")}
        before = state()
        for action, body in (("pull", {"protocol": 1, "acks": []}), ("status", {"protocol": 1})):
            response = backend.request("POST", f"/api/federation/v1/{action}", body=body)
            expected = 501 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 403
            self.assertEqual(response.status, expected)
            if expected == 501:
                self.assertEqual(response.json()["error"], "Not implemented: requires federation/trusted-origin-observation")
            self.assertEqual(state(), before)
        self.assertEqual(backend.request("GET", "/api/federation/v1/message").status, 404)


if __name__ == "__main__":
    unittest.main()
