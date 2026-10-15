#!/usr/bin/env python3
"""Federation signed HTTP behavior against disposable fake peer identities."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_federation import _crypto, _message_bytes, _request_bytes, _sign
from http_backend_harness import BackendProcess, TimedHTTPContractTest, start_contract_backend


class FederationMessagingHTTP(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        original_environment = BackendProcess._environment

        def fixture_environment(backend):
            environment = original_environment(backend)
            environment["CODEX_CANVAS_PUBLIC_ORIGIN"] = "https://studio.ts.net"
            return environment

        with patch.object(BackendProcess, "_environment", fixture_environment):
            cls.backend = start_contract_backend(cls)
        cls.local = _crypto("generate")
        cls.remote = _crypto("generate")
        cls.peer_id = "22222222-2222-4222-8222-222222222222"
        with sqlite3.connect(cls.backend.state / "canvas.sqlite3") as database:
            database.execute("INSERT OR REPLACE INTO runtime_federation_settings VALUES('global',?)",
                             ('{"enabled":true}',))
            database.execute("INSERT OR REPLACE INTO runtime_federation_identity VALUES('local',?)", (
                json.dumps({"stateId": "11111111-1111-4111-8111-111111111111",
                            "label": "Local test server", **cls.local}),
            ))
            database.execute("INSERT OR REPLACE INTO runtime_federation_peers VALUES(?,?)", (
                cls.peer_id,
                json.dumps({"stateId": cls.peer_id, "label": "Fake peer",
                            "origin": "https://peer.ts.net", "publicKey": cls.remote["publicKey"],
                            "status": "approved", "localApproved": True,
                            "remoteApproved": True, "whoisStatus": "missing",
                            "whoisUser": None, "created": 1, "updated": 1}),
            ))

    def headers(self, action, body, nonce, peer_id=None):
        timestamp = int(time.time())
        raw = json.dumps(body, separators=(",", ":")).encode()
        peer_id = peer_id or self.peer_id
        signature = _sign(
            self.remote["privateKey"],
            _request_bytes("POST", f"/api/federation/v1/{action}", timestamp, nonce, raw),
        )
        return {
            "X-Studio-Federation-State": peer_id,
            "X-Studio-Federation-Time": str(timestamp),
            "X-Studio-Federation-Nonce": nonce,
            "X-Studio-Federation-Signature": signature,
        }

    def local_headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_signed_status_and_pull_return_protocol_responses(self):
        for action, body in (("status", {"protocol": 1}),
                             ("pull", {"protocol": 1, "acks": []})):
            response = self.backend.request(
                "POST", f"/api/federation/v1/{action}", body=body,
                headers=self.headers(action, body, f"fake-peer-{action}-nonce-12345"),
            )
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.json()["result"]["protocol"], 1)

    def test_disabled_federation_refuses_before_signed_header_parsing(self):
        probes = (
            ("message", {"protocol": 1, "envelope": {
                "protocol": 1, "senderServer": "ledger-server", "id": "ledger-message",
                "kind": "chat-message", "room": "ledger-room", "payload": {},
                "created": 1, "timestamp": 1, "nonce": "ledger-probe-nonce",
                "signature": "ledger-signature",
            }}),
            ("pair", {
                "protocol": 1, "inviteId": "fixture-invite", "token": "fixture-token",
                "stateId": "fixture-state", "label": "Ledger",
                "origin": "https://ledger.example", "publicKey": "fixture-key",
            }),
            ("pull", {"protocol": 1, "acks": []}),
            ("status", {"protocol": 1}),
        )
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("INSERT OR REPLACE INTO runtime_federation_settings VALUES('global',?)",
                             ('{"enabled":false}',))
        try:
            for action, body in probes:
                response = self.backend.request(
                    "POST", f"/api/federation/v1/{action}", body=body,
                )
                self.assertEqual(response.status, 403, response.body[:1000])
        finally:
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
                database.execute("INSERT OR REPLACE INTO runtime_federation_settings VALUES('global',?)",
                                 ('{"enabled":true}',))

    def test_invitation_stores_python_compatible_private_key_without_returning_it(self):
        from codex_federation import _sign, _verify

        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("INSERT OR REPLACE INTO runtime_federation_settings VALUES('global',?)",
                             ('{"enabled":true}',))
        response = self.backend.request("POST", "/api/federation", body={
            "action": "create_invite", "label": "Python compatible key",
        }, headers=self.local_headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        invitation = response.json()["invitation"]
        self.assertEqual(invitation["origin"], "https://studio.ts.net")
        self.assertNotIn("privateKey", invitation)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            identity = json.loads(database.execute(
                "SELECT record FROM runtime_federation_identity WHERE id='local'"
            ).fetchone()[0])
            invite = json.loads(database.execute(
                "SELECT record FROM runtime_federation_invites WHERE id=?", (invitation["inviteId"],)
            ).fetchone()[0])
        self.assertEqual(identity["publicKey"], invitation["publicKey"])
        self.assertEqual(invite["tokenHash"], hashlib.sha256(invitation["token"].encode()).hexdigest())
        signed = b"Python verifies the generated federation identity"
        self.assertTrue(_verify(identity["publicKey"], signed, _sign(identity["privateKey"], signed)))

    def test_message_and_pair_routes_reject_bad_protocol_content_without_refusal(self):
        message = {"protocol": 1, "envelope": {"protocol": 1}}
        response = self.backend.request(
            "POST", "/api/federation/v1/message", body=message,
            headers=self.headers("message", message, "fake-peer-message-nonce-12345"),
        )
        self.assertIn(response.status, (400, 422), response.body[:1000])
        token = "fixture-pair-token"
        invite_id = "33333333-3333-4333-8333-333333333333"
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("INSERT OR REPLACE INTO runtime_federation_invites VALUES(?,?)", (
                invite_id,
                json.dumps({"id": invite_id, "tokenHash": hashlib.sha256(token.encode()).hexdigest(),
                            "created": time.time(), "expires": int(time.time()) + 900,
                            "status": "open", "expectedUser": None}),
            ))
        pair = {
            "protocol": 1,
            "inviteId": invite_id,
            "token": token,
            "stateId": self.peer_id,
            "label": "Fake peer",
            "origin": "https://peer.ts.net",
            "publicKey": self.remote["publicKey"],
            "tailscaleUser": None,
            "tailscaleStatus": "unavailable",
            "unexpected": True,
        }
        response = self.backend.request(
            "POST", "/api/federation/v1/pair", body=pair,
            headers=self.headers("pair", pair, "fake-peer-pair-nonce-12345"),
        )
        self.assertIn(response.status, (400, 422), response.body[:1000])
        self.assertNotIn(b"Not implemented", response.body)
        self.assertEqual(self.backend.request("GET", "/api/federation/v1/message").status, 404)

    def test_signed_message_and_pair_routes_accept_fake_peer_requests(self):
        room_id = "federated:fake-room-invite"
        now = int(time.time())
        envelope = {
            "protocol": 1,
            "senderServer": self.peer_id,
            "id": "fake-peer-room-invite",
            "kind": "room-invite",
            "room": room_id,
            "payload": {
                "room": room_id,
                "participants": [{"id": "fake-remote-lead", "role": "lead", "name": "Fake lead"}],
                "shareNames": False,
                "shareStatus": False,
            },
            "created": now,
            "timestamp": now,
            "nonce": "fake-peer-inner-message-nonce-12345",
        }
        envelope["signature"] = _sign(self.remote["privateKey"], _message_bytes(envelope))
        message = {"protocol": 1, "envelope": envelope}
        response = self.backend.request(
            "POST", "/api/federation/v1/message", body=message,
            headers=self.headers("message", message, "fake-peer-message-request-12345"),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["result"]["messageId"], envelope["id"])
        self.assertTrue(response.json()["result"]["accepted"])
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            self.assertEqual(database.execute(
                "SELECT count(*) FROM runtime_federation_inbox WHERE message_id=?", (envelope["id"],)
            ).fetchone()[0], 1)
            self.assertEqual(database.execute(
                "SELECT json_extract(record,'$.remoteMembers[0].id') FROM runtime_federation_rooms WHERE id=?",
                (room_id,),
            ).fetchone()[0], "fake-remote-lead")

        invite = self.backend.request(
            "POST", "/api/federation", body={
                "action": "create_invite", "label": "Fake receiver",
            }, headers=self.local_headers(),
        )
        self.assertEqual(invite.status, 200, invite.body[:1000])
        invitation = invite.json()["invitation"]
        pair_peer_id = "44444444-4444-4444-8444-444444444444"
        pair = {
            "protocol": 1,
            "inviteId": invitation["inviteId"],
            "token": invitation["token"],
            "stateId": pair_peer_id,
            "label": "Fake pairing peer",
            "origin": "https://pair-peer.ts.net",
            "publicKey": self.remote["publicKey"],
        }
        paired = self.backend.request(
            "POST", "/api/federation/v1/pair", body=pair,
            headers=self.headers("pair", pair, "fake-pair-request-nonce-12345", peer_id=pair_peer_id),
        )
        self.assertEqual(paired.status, 200, paired.body[:1000])
        self.assertEqual(paired.json()["result"]["protocol"], 1)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            stored_peer = database.execute(
                "SELECT record FROM runtime_federation_peers WHERE id=?", (pair_peer_id,)
            ).fetchone()
            stored_invite = database.execute(
                "SELECT record FROM runtime_federation_invites WHERE id=?", (invitation["inviteId"],)
            ).fetchone()
        self.assertIsNotNone(stored_peer)
        self.assertEqual(json.loads(stored_peer[0])["publicKey"], self.remote["publicKey"])
        self.assertEqual(json.loads(stored_invite[0])["usedBy"], pair_peer_id)


if __name__ == "__main__":
    unittest.main()
