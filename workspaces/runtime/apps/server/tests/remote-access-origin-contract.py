#!/usr/bin/env python3
"""A backend takes its Serve origin from remote-access.json, as codex_remote.py does.

Run it against another backend with STUDIO_HTTP_BACKEND_COMMAND. The test never
changes Tailscale Serve: it sends the forwarded headers that Serve would send.
"""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_remote import REMOTE_ACCESS_FILE
from http_backend_harness import BackendProcess, TimedHTTPContractTest


ORIGIN = "https://studio.example.ts.net"


class RemoteAccessOriginContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        scratch = os.environ.get("TMPDIR")
        if not scratch:
            raise RuntimeError("The server runner must provide a memory-backed TMPDIR")
        cls.backend = BackendProcess(Path(scratch))
        cls.addClassCleanup(cls.backend.close)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        cls.backend.start()
        # The owner enables Serve access in an existing state directory, as
        # codex-mobile.py does. The fixture environment has no origin override,
        # so the restarted server reads the file.
        cls.configure(enabled=True, origin=ORIGIN + "/")
        cls.backend.stop()
        cls.backend.start()

    @classmethod
    def configure(cls, *, enabled: bool, origin: str = ORIGIN) -> None:
        (cls.backend.state / REMOTE_ACCESS_FILE).write_text(
            json.dumps({"enabled": enabled, "origin": origin})
        )

    def setUp(self) -> None:
        super().setUp()
        self.configure(enabled=True)

    def serve_request(self, path: str) -> int:
        return self.backend.request("GET", path, headers={
            "Origin": ORIGIN,
            "X-Forwarded-Host": "studio.example.ts.net",
            "X-Forwarded-Proto": "https",
            "X-Forwarded-For": "100.100.12.34",
        }).status

    def test_the_identity_names_the_configured_origin(self) -> None:
        response = self.backend.request("GET", "/api/multi-server")
        self.assertEqual(response.status, 200, response.body)
        self.assertEqual(json.loads(response.body)["identity"]["origin"], ORIGIN)

    def test_serve_requests_use_the_configured_origin(self) -> None:
        self.assertEqual(self.serve_request("/api/session"), 200)

    def test_disable_takes_effect_without_restart(self) -> None:
        self.configure(enabled=False)
        self.assertEqual(self.serve_request("/api/session"), 403)
        self.assertEqual(self.backend.request("GET", "/api/session").status, 200)
        self.configure(enabled=True)
        self.assertEqual(self.serve_request("/api/session"), 200)


if __name__ == "__main__":
    unittest.main()
