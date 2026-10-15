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
from codex_federation import _crypto, _request_bytes, _sign
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class FederationMessagingHTTP(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        with patch.dict("os.environ", {"CODEX_CANVAS_PUBLIC_ORIGIN": "https://studio.ts.net"}):
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

    def headers(self, action, body, nonce):
        timestamp = int(time.time())
        raw = json.dumps(body, separators=(",", ":")).encode()
        signature = _sign(
            self.remote["privateKey"],
            _request_bytes("POST", f"/api/federation/v1/{action}", timestamp, nonce, raw),
        )
        return {
            "X-Studio-Federation-State": self.peer_id,
            "X-Studio-Federation-Time": str(timestamp),
            "X-Studio-Federation-Nonce": nonce,
            "X-Studio-Federation-Signature": signature,
        }

    def test_signed_status_and_pull_return_protocol_responses(self):
        for action, body in (("status", {"protocol": 1}),
                             ("pull", {"protocol": 1, "acks": []})):
            response = self.backend.request(
                "POST", f"/api/federation/v1/{action}", body=body,
                headers=self.headers(action, body, f"fake-peer-{action}-nonce-12345"),
            )
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.json()["protocol"], 1)

    def test_invitation_stores_python_compatible_private_key_without_returning_it(self):
        from codex_federation import _sign, _verify

        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("INSERT OR REPLACE INTO runtime_federation_settings VALUES('global',?)",
                             ('{"enabled":true}',))
        response = self.backend.request("POST", "/api/federation", body={
            "action": "create_invite", "label": "Python compatible key",
        })
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
        pair = {"protocol": 1}
        response = self.backend.request(
            "POST", "/api/federation/v1/pair", body=pair,
            headers=self.headers("pair", pair, "fake-peer-pair-nonce-12345"),
        )
        self.assertIn(response.status, (400, 422), response.body[:1000])
        self.assertNotIn(b"Not implemented", response.body)
        self.assertEqual(self.backend.request("GET", "/api/federation/v1/message").status, 404)


if __name__ == "__main__":
    unittest.main()
