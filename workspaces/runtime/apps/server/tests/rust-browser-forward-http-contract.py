#!/usr/bin/env python3
"""Keep browser forward behavior on Python and prove the Rust named refusal is read-only."""

from __future__ import annotations

import os
import socket
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from http_backend_harness import TimedHTTPContractTest, no_write_snapshot, start_contract_backend


class BrowserForwardHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)

    @property
    def backend(self):
        return type(self).backend

    def test_browser_forward_backend_contract_and_no_write(self) -> None:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        request = {
            "server": "paired-test-peer",
            "bridgeId": "0123456789abcdef0123456789abcdef",
            "host": "127.0.0.1",
            "port": port,
            "path": "/oauth/callback",
            "expiresAt": 2_000_000_000.0,
            "token": "0123456789abcdef",
        }
        before = no_write_snapshot(self.backend.state)
        response = self.backend.request(
            "POST", "/api/browser/forward", body=request,
            headers={
                "Origin": self.backend.origin or "",
                "X-Canvas-Token": self.backend.session["token"],
                "X-Studio-API-Schema": self.backend.schema_hash,
            },
        )
        backend_command = os.environ.get("STUDIO_HTTP_BACKEND_COMMAND", "")
        if backend_command:
            self.assertEqual(response.status, 501, response.body[:500])
            self.assertIn(b"multi-server-runtime/orchestration", response.body)
        else:
            self.assertEqual(response.status, 200, response.body[:500])
            self.assertEqual(response.json(), {"status": "forwarding"})
        self.assertEqual(no_write_snapshot(self.backend.state), before)


if __name__ == "__main__":
    unittest.main()
