#!/usr/bin/env python3
"""Local state and refusal contracts for multi-server and federation routes."""

from __future__ import annotations

import os
import sqlite3
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


FEDERATION_TABLES = (
    "runtime_federation_settings",
    "runtime_federation_identity",
    "runtime_federation_peers",
    "runtime_federation_invites",
    "runtime_federation_rooms",
    "runtime_federation_outbox",
    "runtime_federation_inbox",
    "runtime_federation_nonces",
)
ACCESS_TABLES = (
    "runtime_access_settings",
    "runtime_access_clients",
    "runtime_access_invites",
    "runtime_access_audit",
)


def table_snapshot(state: Path, tables: tuple[str, ...]) -> dict[str, list[tuple]]:
    with sqlite3.connect(state / "canvas.sqlite3") as database:
        existing = {
            row[0]
            for row in database.execute("SELECT name FROM sqlite_master WHERE type='table'")
        }
        return {
            table: database.execute(f'SELECT * FROM "{table}" ORDER BY rowid').fetchall()
            for table in tables
            if table in existing
        }


class RemoteLocalHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        if cls.rust_backend:
            cls.backend.stop()
            with patch.dict(os.environ, {"STUDIO_HTTP_BACKEND_COMMAND": ""}):
                cls.backend.start()
                seeded = cls.backend.request(
                    "POST",
                    "/api/federation",
                    body={"action": "set_enabled", "enabled": False},
                    headers=cls.headers(),
                )
                if seeded.status != 200:
                    raise AssertionError(f"Python federation state seed failed: HTTP {seeded.status}")
            cls.backend.stop()
            cls.backend.start()
            cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)
        else:
            seeded = cls.backend.request(
                "POST",
                "/api/federation",
                body={"action": "set_enabled", "enabled": False},
                headers=cls.headers(),
            )
            if seeded.status != 200:
                raise AssertionError(f"Python federation state seed failed: HTTP {seeded.status}")

    @classmethod
    def headers(cls) -> dict[str, str]:
        backend = cls.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def test_federation_snapshot_reads_python_written_local_configuration(self) -> None:
        response = self.backend.request(
            "POST", "/api/federation", body={"action": "status"}, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["version"], 1)
        self.assertFalse(response.json()["enabled"])
        self.assertIsNone(response.json()["identity"])
        self.assertEqual(response.json()["peers"], [])
        self.assertEqual(response.json()["invites"], [])
        self.assertEqual(response.json()["rooms"], [])

    def test_federation_documented_get_counterparts_remain_404(self) -> None:
        for path in (
            "/api/federation",
            "/api/federation/v1/message",
            "/api/federation/v1/pair",
            "/api/federation/v1/pull",
            "/api/federation/v1/status",
        ):
            with self.subTest(path=path):
                response = self.backend.request("GET", path, headers=self.headers())
                self.assertEqual(response.status, 404, response.body[:1000])
                self.assertEqual(response.json(), {"error": "Not found"})

    def test_federation_disable_is_a_local_state_write(self) -> None:
        response = self.backend.request(
            "POST",
            "/api/federation",
            body={"action": "set_enabled", "enabled": False},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertFalse(response.json()["enabled"])

    def test_federation_unknown_peer_refusal_does_not_write(self) -> None:
        before = table_snapshot(self.backend.state, FEDERATION_TABLES)
        response = self.backend.request(
            "POST",
            "/api/federation",
            body={"action": "revoke_peer", "state_id": "missing-peer"},
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                "Not implemented: requires federation-driver/rooms, "
                "remote-transport/federation-exchange",
            )
        else:
            self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(table_snapshot(self.backend.state, FEDERATION_TABLES), before)

    def test_federation_invitation_refusal_does_not_write_or_contact_peer(self) -> None:
        before = table_snapshot(self.backend.state, FEDERATION_TABLES)
        response = self.backend.request(
            "POST",
            "/api/federation",
            body={"action": "create_invite", "label": "Disposable test"},
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                "Not implemented: requires federation-driver/rooms, "
                "remote-transport/federation-exchange",
            )
        else:
            self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(table_snapshot(self.backend.state, FEDERATION_TABLES), before)

    def test_multi_server_audit_reads_empty_local_state(self) -> None:
        response = self.backend.request(
            "GET", "/api/multi-server/audit", headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json(), {"records": []})

    def test_multi_server_pair_refusal_does_not_write_or_contact_another_server(self) -> None:
        before = table_snapshot(self.backend.state, ACCESS_TABLES)
        response = self.backend.request(
            "POST",
            "/api/multi-server/v1/pair",
            body={
                "protocol": 1,
                "inviteId": "fixture-invite",
                "token": "fixture-token",
                "clientId": "fixture-client",
                "label": "Disposable test",
                "kind": "ui",
                "publicKey": "fixture-key",
                "requestId": "fixture-request",
            },
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                "Not implemented: requires remote-transport/multi-server",
            )
        else:
            self.assertEqual(response.status, 403, response.body[:1000])
        self.assertEqual(table_snapshot(self.backend.state, ACCESS_TABLES), before)


if __name__ == "__main__":
    unittest.main()
