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
    "runtime_access_discovered",
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

    def setUp(self) -> None:
        super().setUp()
        # Seed records in the established Python format before exercising either backend.
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            for table in ("runtime_federation_outbox", "runtime_federation_rooms", "runtime_federation_invites", "runtime_federation_peers", "runtime_federation_identity", "runtime_federation_settings"):
                database.execute(f'DELETE FROM "{table}"')
            database.execute("INSERT INTO runtime_federation_settings VALUES('global', ?)", ('{"enabled":true,"updated":1}',))
            database.execute("INSERT INTO runtime_federation_identity VALUES('local', ?)", ('{"stateId":"11111111-1111-4111-8111-111111111111","label":"Local","publicKey":"public-key","privateKey":"must-not-appear"}',))
            database.execute("INSERT INTO runtime_federation_peers VALUES('peer-a', ?)", ('{"stateId":"peer-a","label":"Peer A","origin":"https://peer.example","publicKey":"peer-key","status":"approved","whoisStatus":"verified","whoisUser":"owner@example","localApproved":true,"remoteApproved":true,"created":2,"updated":3,"inviteToken":"secret"}',))
            database.execute("INSERT INTO runtime_federation_invites VALUES('invite-a', ?)", ('{"id":"invite-a","status":"open","created":2,"expires":9,"expectedUser":null,"tokenHash":"secret"}',))
            database.execute("INSERT INTO runtime_federation_rooms VALUES('room-a', ?)", ('{"id":"room-a","peerId":"peer-a","peerLabel":"Peer A","name":"Room","status":"approved","localMembers":["lead-a"],"remoteMembers":[],"shareNames":false,"shareStatus":false,"created":3,"updated":4}',))
            database.execute("INSERT INTO runtime_federation_rooms VALUES('room-pending', ?)", ('{"id":"room-pending","peerId":"peer-a","peerLabel":"Peer A","name":"Inbound room","status":"pending","localApproved":false,"remoteApproved":true,"localMembers":[],"remoteMembers":[],"shareNames":false,"shareStatus":false,"created":3,"updated":4}',))
            database.execute("INSERT INTO runtime_federation_outbox VALUES('outbox-a','peer-a', ?)", ('{"id":"outbox-a","kind":"room-invite","room":"room-a","payload":{},"created":4,"status":"queued","attempts":0,"nextAt":0,"error":null}',))
            database.execute("INSERT OR REPLACE INTO runtime_agents(id,record) VALUES('lead-a',?)", ('{"id":"lead-a","rootId":"lead-a","isLead":true,"name":"Fixture Lead","status":"idle"}',))
            database.execute("INSERT OR REPLACE INTO runtime_access_settings VALUES(1,1)")
            database.execute("DELETE FROM runtime_access_clients")
            database.execute("DELETE FROM runtime_access_discovered")
            database.execute("DELETE FROM runtime_access_audit")
            database.execute("DELETE FROM runtime_access_local_alias")
            database.execute("INSERT INTO runtime_access_clients VALUES('client-a',?)", ('{"id":"client-a","clientId":"client-a","serverId":"server-a","label":"Server A","kind":"server","origin":"https://server-a.example","publicKey":"client-key","tailscaleUser":"owner@example","status":"paired","created":1.0,"lastAccess":2.0,"revoked":null,"alias":"SRV"}',))
            database.execute("INSERT INTO runtime_access_discovered VALUES('discovered-a',?)", ('{"id":"discovered-a","clientId":"discovered-a","serverId":"discovered-a","label":"Discovered server","kind":"server","origin":"https://discovered.example","publicKey":"discovered-key","tailscaleUser":"owner@example","status":"discovered","created":2.0,"lastAccess":null,"revoked":null,"lastSeen":3.0,"autoPair":false}',))
        import json
        if self.rust_backend:
            identity_path = self.backend.state / "multi-server" / "identity.json"
            identity_path.parent.mkdir(mode=0o700, exist_ok=True)
            identity_path.write_text(json.dumps({"serverId":"local-server","label":"Local server","publicKey":"local-key","privateKey":"private-sentinel"}), encoding="utf-8")
            identity_path.chmod(0o600)

    def test_federation_snapshot_reads_python_written_local_configuration(self) -> None:
        response = self.backend.request(
            "POST", "/api/federation", body={"action": "status"}, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["version"], 1)
        self.assertTrue(response.json()["enabled"])
        self.assertEqual(response.json()["identity"]["stateId"], "11111111-1111-4111-8111-111111111111")
        self.assertEqual(response.json()["identity"]["label"], "Local")
        self.assertNotIn("privateKey", response.body.decode())
        self.assertNotIn("inviteToken", response.body.decode())
        self.assertEqual(response.json()["peers"][0]["stateId"], "peer-a")
        self.assertNotIn("inviteToken", response.json()["peers"][0])
        self.assertEqual(response.json()["invites"][0]["id"], "invite-a")
        self.assertNotIn("tokenHash", response.json()["invites"][0])
        self.assertEqual(response.json()["rooms"][0]["id"], "room-a")
        self.assertEqual(response.json()["queued"], 1)

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
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            import json
            settings = json.loads(database.execute("SELECT record FROM runtime_federation_settings WHERE id='global'").fetchone()[0])
        self.assertFalse(settings["enabled"])
        self.assertIn("updated", settings)

    def test_federation_response_failure_rolls_back_setting_mutation(self) -> None:
        if not self.rust_backend:
            self.skipTest("rollback-on-response-validation is the Rust transaction contract")
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("UPDATE runtime_federation_settings SET record=? WHERE id='global'", ('{"enabled":true,"updated":1}',))
            database.execute("UPDATE runtime_federation_peers SET record=? WHERE id='peer-a'", ('{"stateId":"peer-a","label":42}',))
        response = self.backend.request("POST", "/api/federation", body={"action":"set_enabled","enabled":False}, headers=self.headers())
        self.assertEqual(response.status, 500, response.body[:1000])
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            settings = __import__("json").loads(database.execute("SELECT record FROM runtime_federation_settings WHERE id='global'").fetchone()[0])
        self.assertTrue(settings["enabled"])

    def test_federation_local_peer_room_mutations_persist(self) -> None:
        approved = self.backend.request("POST", "/api/federation", body={
            "action":"approve_peer", "state_id":"peer-a"}, headers=self.headers())
        self.assertEqual(approved.status, 200, approved.body[:1000])
        options = self.backend.request("POST", "/api/federation", body={
            "action":"room_options", "room_id":"room-a", "disclosure":{"shareNames":True}}, headers=self.headers())
        self.assertEqual(options.status, 200, options.body[:1000])
        self.assertTrue(next(r for r in options.json()["rooms"] if r["id"]=="room-a")["shareNames"])
        revoked = self.backend.request("POST", "/api/federation", body={
            "action":"revoke_peer", "state_id":"peer-a"}, headers=self.headers())
        self.assertEqual(revoked.status, 200, revoked.body[:1000])
        self.assertEqual(next(r for r in revoked.json()["rooms"] if r["id"]=="room-a")["status"], "revoked")
        self.assertEqual(revoked.json()["queued"], 0)

    def test_federation_room_queue_is_unsigned_and_post_commit(self) -> None:
        created=self.backend.request("POST","/api/federation",body={"action":"create_room","peer_id":"peer-a","local_members":["lead-a"]},headers=self.headers())
        self.assertEqual(created.status,200,created.body[:1000])
        self.assertEqual(created.json()["queued"],2)
        with sqlite3.connect(self.backend.state/"canvas.sqlite3") as database:
            outbox=database.execute("SELECT record FROM runtime_federation_outbox WHERE id!='outbox-a'").fetchone()[0]
        self.assertEqual(__import__("json").loads(outbox)["kind"],"room-invite")
        self.assertNotIn("signature",outbox)
        approved=self.backend.request("POST","/api/federation",body={"action":"approve_room","room_id":"room-pending","local_members":["lead-a"]},headers=self.headers())
        self.assertEqual(approved.status,200,approved.body[:1000])
        self.assertEqual(approved.json()["queued"],3)

    def test_federation_unknown_peer_refusal_does_not_write(self) -> None:
        before = table_snapshot(self.backend.state, FEDERATION_TABLES)
        response = self.backend.request(
            "POST",
            "/api/federation",
            body={"action": "revoke_peer", "state_id": "missing-peer"},
            headers=self.headers(),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 400, response.body[:1000])
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
            self.assertIn("tailscale-identity", response.json()["error"])
        else:
            self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(table_snapshot(self.backend.state, FEDERATION_TABLES), before)

    def test_multi_server_audit_reads_empty_local_state(self) -> None:
        response = self.backend.request(
            "GET", "/api/multi-server/audit", headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json(), {"records": []})

    def test_multi_server_snapshot_uses_public_local_identity_and_populated_rows(self) -> None:
        response=self.backend.request("GET","/api/multi-server",headers=self.headers())
        self.assertEqual(response.status,200,response.body[:1000])
        body=response.json()
        if self.rust_backend:
            self.assertEqual(body["identity"]["serverId"],"local-server")
        else:
            self.assertTrue(body["identity"]["serverId"])
        self.assertEqual(body["clients"][0]["id"],"client-a")
        self.assertIn("discovered-a",[peer["id"] for peer in body["servers"]])
        self.assertEqual(body["settings"]["autoPair"],True)
        self.assertNotIn("privateKey",response.body.decode())

    def test_multi_server_local_actions_commit_replay_and_conflict(self) -> None:
        revoked=self.backend.request("POST","/api/multi-server",body={"action":"revoke","requestId":"revoke-a","clientId":"client-a"},headers=self.headers())
        self.assertEqual(revoked.status,200,revoked.body[:1000])
        self.assertEqual(revoked.json()["clients"][0]["status"],"revoked")
        unrevoked=self.backend.request("POST","/api/multi-server",body={"action":"unrevoke","requestId":"unrevoke-a","clientId":"client-a"},headers=self.headers())
        self.assertEqual(unrevoked.status,200,unrevoked.body[:1000])
        self.assertNotIn("client-a",[client["id"] for client in unrevoked.json()["clients"]])
        replay_unrevoke=self.backend.request("POST","/api/multi-server",body={"action":"unrevoke","requestId":"unrevoke-a","clientId":"client-a"},headers=self.headers())
        self.assertEqual(replay_unrevoke.status,200,replay_unrevoke.body[:1000])
        conflict_unrevoke=self.backend.request("POST","/api/multi-server",body={"action":"unrevoke","requestId":"unrevoke-a","clientId":"other-client"},headers=self.headers())
        self.assertEqual(conflict_unrevoke.status,409,conflict_unrevoke.body[:1000])
        settings=self.backend.request("POST","/api/multi-server",body={"action":"settings","autoPair":False,"requestId":"settings-a"},headers=self.headers())
        self.assertEqual(settings.status,200,settings.body[:1000])
        self.assertFalse(settings.json()["settings"]["autoPair"])
        alias=self.backend.request("POST","/api/multi-server",body={"action":"alias","requestId":"alias-a","serverId":"local","alias":"HQ"},headers=self.headers())
        self.assertEqual(alias.status,200,alias.body[:1000])
        self.assertEqual(alias.json()["settings"]["aliases"]["local"],"HQ")
        replay=self.backend.request("POST","/api/multi-server",body={"action":"alias","requestId":"alias-a","serverId":"local","alias":"HQ"},headers=self.headers())
        self.assertEqual(replay.status,200,replay.body[:1000])
        conflict=self.backend.request("POST","/api/multi-server",body={"action":"alias","requestId":"alias-a","serverId":"local","alias":"ZZ"},headers=self.headers())
        self.assertEqual(conflict.status,409,conflict.body[:1000])

    def test_multi_server_name_requires_identity_writer_without_writes(self) -> None:
        if not self.rust_backend:
            self.skipTest("Python runtime owns the existing identity writer")
        identity_path = self.backend.state / "multi-server" / "identity.json"
        identity_before = identity_path.read_bytes()
        state_before = table_snapshot(self.backend.state, ACCESS_TABLES)
        for path, body in (
            (
                "/api/multi-server",
                {"action": "name", "requestId": "name-a", "serverId": "local", "label": "Renamed locally"},
            ),
            (
                "/api/multi-server/v1/name",
                {"requestId": "name-a", "label": "Renamed locally"},
            ),
        ):
            response = self.backend.request("POST", path, body=body, headers=self.headers())
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn("remote-transport/identity-write", response.json()["error"])
            self.assertEqual(table_snapshot(self.backend.state, ACCESS_TABLES), state_before)
            self.assertEqual(identity_path.read_bytes(), identity_before)

    def test_multi_server_invite_refusal_names_missing_dependencies_without_writes(self) -> None:
        if not self.rust_backend:
            self.skipTest("Python runtime owns the existing invite writer")
        before = table_snapshot(self.backend.state, ACCESS_TABLES)
        response = self.backend.request(
            "POST",
            "/api/multi-server",
            body={"action": "create_invite", "requestId": "invite-a", "label": "Disposable test"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 501, response.body[:1000])
        self.assertIn("federation-credentials/private-key", response.json()["error"])
        self.assertEqual(table_snapshot(self.backend.state, ACCESS_TABLES), before)

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
            self.assertIn("signed-server-requests",response.json()["error"])
        else:
            self.assertEqual(response.status, 403, response.body[:1000])
        self.assertEqual(table_snapshot(self.backend.state, ACCESS_TABLES), before)


if __name__ == "__main__":
    unittest.main()
