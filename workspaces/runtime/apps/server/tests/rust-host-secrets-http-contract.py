#!/usr/bin/env python3
"""HTTP contracts for Linux guest project secrets and Mac-only vault actions."""

from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import socketserver
import sys
import threading
import unittest

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from http_backend_harness import TimedHTTPContractTest, no_write_snapshot, start_contract_backend


class GuestHandler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        line = self.rfile.readline()
        if not line:
            return
        request = json.loads(line)
        self.server.requests.append(request)
        response = {"id": request["id"], "result": {"names": ["TOKEN"]}}
        self.wfile.write(json.dumps(response).encode() + b"\n")


class GuestServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, path: Path):
        self.requests: list[dict] = []
        super().__init__(str(path), GuestHandler)


class HostSecretsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        # Rust backends use the harness's short external VM path to stay below
        # sockaddr_un limits; Python keeps its VM socket under the runtime state.
        guest_dir = cls.backend.vm_state if cls.rust_backend else cls.backend.state / "linux-vm"
        guest_dir.mkdir(parents=True, exist_ok=True)
        cls.guest = GuestServer(guest_dir / "guest.sock")
        cls.guest_thread = threading.Thread(target=cls.guest.serve_forever, daemon=True)
        cls.guest_thread.start()
        cls.addClassCleanup(cls.stop_guest)

    @classmethod
    def stop_guest(cls) -> None:
        cls.guest.shutdown()
        cls.guest_thread.join(timeout=2)
        cls.guest.server_close()

    @classmethod
    def headers(cls):
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def request(self, path: str, body: dict):
        return self.backend.request("POST", path, body=body, headers=self.headers())

    def test_project_secret_routes_use_guest_and_never_return_values(self):
        listed = self.request("/api/project-secrets/list", {"projectId": "project-1"})
        self.assertEqual(listed.status, 200, listed.body[:1000])
        self.assertEqual(listed.json(), {"projectId": "project-1", "names": ["TOKEN"]})

        set_result = self.request("/api/project-secrets/set", {
            "projectId": "project-1", "name": "TOKEN", "value": "fixture-secret-value",
            "requestId": "secret-set-1",
        })
        self.assertEqual(set_result.status, 200, set_result.body[:1000])
        self.assertEqual(set_result.json(), {"projectId": "project-1", "names": ["TOKEN"]})
        self.assertNotIn(b"fixture-secret-value", set_result.body)

        removed = self.request("/api/project-secrets/remove", {
            "projectId": "project-1", "name": "TOKEN", "requestId": "secret-remove-1",
        })
        self.assertEqual(removed.status, 200, removed.body[:1000])
        self.assertEqual(removed.json(), {"projectId": "project-1", "names": ["TOKEN"]})

        self.assertEqual([call["method"] for call in self.guest.requests], [
            "secrets.list", "secrets.set", "secrets.remove",
        ])
        self.assertEqual(self.guest.requests[1]["id"], "secrets-set:secret-set-1")
        self.assertEqual(self.guest.requests[2]["id"], "secrets-remove:secret-remove-1")

    def test_mac_vault_operations_fail_without_mutating_backend(self):
        before = no_write_snapshot(self.backend.state)
        unlock = self.request("/api/vault/unlock", {"projectId": "project-1", "words": "fixture words"})
        self.assertNotIn(b"fixture words", unlock.body)
        if self.rust_backend:
            self.assertEqual(unlock.status, 501, unlock.body[:1000])
            self.assertIn(b"macos-platform/layr-desk", unlock.body)
        else:
            self.assertEqual(unlock.status, 400, unlock.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

        pull = self.request("/api/vault/pull", {
            "projectId": "project-1", "requestId": "pull-1", "to": None,
        })
        if self.rust_backend:
            self.assertEqual(pull.status, 501, pull.body[:1000])
            self.assertIn(b"macos-platform/layr-desk", pull.body)
        else:
            self.assertEqual(pull.status, 400, pull.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)


if __name__ == "__main__":
    unittest.main()
