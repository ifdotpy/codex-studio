"""Owner access tests through the real ASGI application and SQLite receipts."""
from __future__ import annotations

import asyncio
import base64
from collections.abc import Callable, Iterator
from contextlib import contextmanager
import json
import io
from pathlib import Path
import secrets
import runpy
import sqlite3
import sys
import tempfile
import threading
import time
from typing import Any, ClassVar, cast
import unittest
from unittest.mock import patch
from urllib.parse import urlencode, urlsplit

from fastapi import Request
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient
import httpx
from starlette.types import Message, Receive, Scope, Send

from codex_federation import _crypto, _sign
from codex_multi_server import AccessError, MultiServerService, PAIR_PATH, _peer_login, request_bytes
from codex_remote import RemoteAccess
from studio_api.app import create_app
from studio_api.context import ApiContext
from studio_api.multi_server.boundary import MultiServerBoundary
from studio_api.sync.resources.hub import ResourceHub


class FixtureRuntime:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.db_path = root / "canvas.sqlite3"
        self.lock = threading.RLock()
        self.closed = True
        self.service = MultiServerService(self)
        self.calls: list[tuple[str, dict[str, Any]]] = []
        with self.db() as db:
            db.execute("CREATE TABLE IF NOT EXISTS effects(id INTEGER PRIMARY KEY, body TEXT)")

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(self.db_path, timeout=2)
        connection.row_factory = sqlite3.Row
        try:
            with connection:
                yield connection
        finally:
            connection.close()

    @contextmanager
    def read_db(self) -> Iterator[sqlite3.Connection]:
        with self.db() as connection:
            connection.execute("PRAGMA query_only=ON")
            yield connection

    def paired_access(self) -> MultiServerService:
        return self.service

    def multi_server(self) -> FixtureRuntime:
        return self

    def receive(self, server_id: str, envelope: dict[str, Any]) -> dict[str, Any]:
        if (server_id, envelope) not in self.calls:
            self.calls.append((server_id, envelope))
        return {"requestId": envelope["requestId"], "outcome": "applied", "value": {"ok": True}}


class FixtureCanvas:
    def __init__(self, runtime: FixtureRuntime) -> None:
        self.runtime = runtime
        self.root = runtime.root
        self.db = runtime.db_path

    def connect(self) -> Any:
        return self.runtime.db()

    def transcript(self, agent_id: str) -> dict[str, object]:
        return {}


class AccessTests(unittest.TestCase):
    keys: ClassVar[dict[str, Any]]
    origin = "https://server.example.ts.net"

    @classmethod
    def setUpClass(cls) -> None:
        cls.keys = _crypto("generate")

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="studio-access-test-")
        self.addCleanup(self.directory.cleanup)
        self.runtime = FixtureRuntime(Path(self.directory.name))
        self.addCleanup(lambda: self.runtime.service.close())
        self.context = ApiContext(cast(Any, FixtureCanvas(self.runtime)), token="local-token",
                                  remote=RemoteAccess(self.runtime.root, self.origin), server_port=8765)
        self.context.api_schema_hash = "access-schema"
        self.app = create_app(self.context)

        @self.app.get("/api/probe")
        async def read_probe() -> JSONResponse:
            return JSONResponse({"ok": True})

        @self.app.post("/api/probe")
        async def write_probe(request: Request) -> JSONResponse:
            body = await request.body()
            with self.runtime.db() as db:
                db.execute("INSERT INTO effects(body) VALUES(?)", (body.decode(),))
                count = db.execute("SELECT count(*) FROM effects").fetchone()[0]
            if json.loads(body).get("failAfterEffect"):
                raise RuntimeError("The result was lost after the effect")
            return JSONResponse({"count": count})

        # Insert the fixture routes before the production static catch-all.
        self.app.router.routes.insert(0, self.app.router.routes.pop())
        self.app.router.routes.insert(0, self.app.router.routes.pop())

        self.client = TestClient(self.app, base_url="http://127.0.0.1:8765", client=("127.0.0.1", 9999))
        self.addCleanup(self.client.close)
        owner = patch("codex_multi_server._owner_login", return_value="owner@example.com")
        peer = patch("codex_multi_server._peer_login", return_value="owner@example.com")
        owner.start()
        peer.start()
        self.addCleanup(owner.stop)
        self.addCleanup(peer.stop)

    def proxy(self) -> dict[str, str]:
        return {"X-Forwarded-Host": "server.example.ts.net", "X-Forwarded-Proto": "https",
                "X-Forwarded-For": "100.64.0.12"}

    def signed(self, method: str, target: str, raw: bytes = b"", request_id: str = "read-1",
               client_id: str = "client-1", timestamp: str | None = None) -> dict[str, str]:
        timestamp = timestamp or str(int(time.time()))
        nonce = secrets.token_urlsafe(24)
        server_id = self.runtime.service.local_server_id
        proof = request_bytes(method, target, server_id, client_id, timestamp, nonce, request_id, raw)
        return {**self.proxy(), "X-Studio-Client": client_id, "X-Studio-Server": server_id,
                "X-Studio-Timestamp": timestamp, "X-Studio-Nonce": nonce,
                "X-Studio-Request-Id": request_id, "X-Studio-Signature": _sign(self.keys["privateKey"], proof),
                "Content-Type": "application/json"}

    def invitation(self, request_id: str = "invite-1") -> dict[str, Any]:
        result = self.client.post("/api/multi-server", json={"action": "create_invite", "requestId": request_id},
                                  headers={"X-Canvas-Token": "local-token"})
        self.assertEqual(result.status_code, 200, result.text)
        return cast(dict[str, Any], result.json()["invitation"])

    def pair_body(self, invitation: dict[str, Any] | None = None, kind: str = "ui",
                  client_id: str = "client-1", request_id: str = "pair-1") -> dict[str, Any]:
        invite = invitation or self.invitation()
        result = {"protocol": 1, "inviteId": invite["inviteId"], "token": invite["token"],
                  "clientId": client_id, "label": "Test device", "kind": kind,
                  "publicKey": self.keys["publicKey"], "requestId": request_id}
        if kind == "server":
            result["origin"] = "https://peer.example.ts.net"
        return result

    def pair(self, body: dict[str, Any] | None = None) -> httpx.Response:
        payload = body or self.pair_body()
        raw = json.dumps(payload).encode()
        return cast(httpx.Response, self.client.post(PAIR_PATH, content=raw,
                    headers=self.signed("POST", PAIR_PATH, raw, payload["requestId"], payload["clientId"])))

    def effects(self) -> int:
        with self.runtime.read_db() as db:
            return int(db.execute("SELECT count(*) FROM effects").fetchone()[0])

    def test_local_access_and_unsigned_remote_denial(self) -> None:
        self.assertEqual(self.client.get("/api/session").json()["token"], "local-token")
        for path in ("/api/session", "/api/probe", "/api/sync/identity", "/api/multi-server"):
            response = self.client.get(path, headers={**self.proxy(), "X-Studio-Client": "unpaired"})
            self.assertEqual(response.status_code, 401, path)
            self.assertEqual(response.json()["code"], "invalid_credentials")
        response = self.client.post("/api/probe", json={}, headers={**self.proxy(), "X-Canvas-Token": "local-token", "X-Studio-Client": "unpaired"})
        self.assertEqual(response.status_code, 401)
        self.assertEqual(self.effects(), 0)
        self.assertEqual(self.client.post(PAIR_PATH, json=self.pair_body(), headers={"X-Canvas-Token": "local-token"}).status_code, 403)

    def test_invitation_retry_hash_and_snapshot_redaction(self) -> None:
        first, second = self.invitation(), self.invitation()
        self.assertEqual(first, second)
        with self.runtime.read_db() as db:
            stored = db.execute("SELECT record FROM runtime_access_invites").fetchone()[0]
        self.assertNotIn(first["token"], stored)
        snapshot = self.client.get("/api/multi-server").text
        self.assertNotIn(first["token"], snapshot)
        self.assertNotIn("PRIVATE KEY", snapshot)
        response = self.client.post("/api/multi-server", json={"action": "create_invite", "requestId": "invite-1", "label": "Different"},
                                    headers={"X-Canvas-Token": "local-token"})
        self.assertEqual(response.status_code, 409)

    def test_pair_retry_and_signed_read(self) -> None:
        body = self.pair_body()
        first, second = self.pair(body), self.pair(body)
        self.assertEqual(first.status_code, 200, first.text)
        self.assertEqual(second.status_code, 200, second.text)
        self.assertEqual(first.json(), second.json())
        self.assertEqual(first.json()["serverId"], self.runtime.service.local_server_id)
        self.assertEqual(self.client.get("/api/probe", headers=self.signed("GET", "/api/probe")).status_code, 200)
        snapshot = self.client.get("/api/multi-server").json()
        self.assertEqual(len(snapshot["clients"]), 1)
        self.assertIsNotNone(snapshot["clients"][0]["lastAccess"])
        self.assertEqual(snapshot["clients"][0]["status"], "paired")

    def test_remote_invitation_receipt_does_not_store_the_secret(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        raw = b'{"action":"create_invite","requestId":"remote-invite"}'
        first = self.client.post("/api/multi-server", content=raw,
                                 headers=self.signed("POST", "/api/multi-server", raw, "remote-invite"))
        self.assertEqual(first.status_code, 200, first.text)
        token = first.json()["invitation"]["token"]
        with self.runtime.read_db() as db:
            saved = db.execute("SELECT record FROM runtime_access_receipts WHERE request_id='remote-invite'").fetchone()[0]
            receipt = json.loads(saved)
            self.assertNotIn(token.encode(), base64.b64decode(receipt["body"]))
            self.assertNotIn(token, saved)
            invitation = db.execute("SELECT record FROM runtime_access_invites WHERE request_id='remote-invite'").fetchone()[0]
            self.assertNotIn(token, invitation)
        self.runtime.service = MultiServerService(self.runtime)
        second = self.client.post("/api/multi-server", content=raw,
                                  headers=self.signed("POST", "/api/multi-server", raw, "remote-invite"))
        self.assertEqual(first.content, second.content)

    def test_invalid_server_origin_does_not_consume_the_invitation(self) -> None:
        body = self.pair_body(kind="server")
        body["origin"] = "http://localhost:1234"
        failed = self.pair(body)
        self.assertEqual(failed.status_code, 400, failed.text)
        self.assertEqual(failed.json()["code"], "invalid_origin")
        body["origin"] = "https://peer.example.ts.net"
        body["requestId"] = "corrected-pair"
        self.assertEqual(self.pair(body).status_code, 200)

    def test_single_use_invite_and_owner_check(self) -> None:
        body = self.pair_body()
        with patch("codex_multi_server._peer_login", return_value="other@example.com"):
            self.assertEqual(self.pair(body).status_code, 403)
        self.assertEqual(self.pair(body).status_code, 200)
        changed = {**body, "clientId": "another-client", "requestId": "pair-other"}
        response = self.pair(changed)
        self.assertEqual(response.status_code, 403)
        self.assertEqual(response.json()["code"], "invite_unavailable")

    def test_tamper_query_replay_clock_and_duplicate_headers(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        target = "/api/probe?a=%2F&b=2"
        signed = self.signed("GET", target)
        first = self.client.get(target, headers=signed)
        self.assertEqual(first.status_code, 200, first.text)
        replay = self.client.get(target, headers=signed)
        self.assertEqual(replay.json()["code"], "nonce_replay")
        changed = self.client.get("/api/probe?b=2&a=%2F", headers=self.signed("GET", target))
        self.assertEqual(changed.json()["code"], "invalid_signature")
        expired = self.client.get("/api/probe", headers=self.signed("GET", "/api/probe", timestamp=str(int(time.time())-301)))
        self.assertEqual(expired.json()["code"], "clock_difference")
        duplicate = list(self.signed("GET", "/api/probe").items()) + [("X-Studio-Nonce", "duplicate")]
        self.assertEqual(self.client.get("/api/probe", headers=duplicate).status_code, 401)
        raw = b'{"value":1}'
        changed_body = self.client.post("/api/probe", content=b'{"value":2}', headers=self.signed("POST", "/api/probe", raw, "tamper"))
        self.assertEqual(changed_body.json()["code"], "invalid_signature")
        self.assertEqual(self.effects(), 0)

    def test_mutation_receipt_retry_conflict_and_restart(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        raw = b'{"requestId":"write-1","value":1}'
        first = self.client.post("/api/probe", content=raw, headers=self.signed("POST", "/api/probe", raw, "write-1"))
        self.assertEqual(first.status_code, 200, first.text)
        self.assertEqual(first.json(), {"count": 1})
        self.runtime.service = MultiServerService(self.runtime)
        retry = self.client.post("/api/probe", content=raw, headers=self.signed("POST", "/api/probe", raw, "write-1"))
        self.assertEqual(retry.json(), {"count": 1})
        self.assertEqual(retry.headers["X-Studio-Server"], self.runtime.service.local_server_id)
        changed = b'{"requestId":"write-1","value":2}'
        conflict = self.client.post("/api/probe", content=changed, headers=self.signed("POST", "/api/probe", changed, "write-1"))
        self.assertEqual(conflict.json()["code"], "request_conflict")
        self.assertEqual(self.effects(), 1)

    def test_pending_unknown_and_atomic_pairing_recovery(self) -> None:
        payload = self.pair_body()
        raw = json.dumps(payload).encode()
        self.runtime.service.public_origin = self.origin
        from studio_api.middleware import HeaderView
        signed = self.signed("POST", PAIR_PATH, raw, "pair-1")
        headers = HeaderView([(key.lower().encode(), value.encode()) for key, value in signed.items()])
        principal = self.runtime.service.authenticate(headers, "POST", PAIR_PATH, raw)
        self.runtime.service.reserve("client-1", "pair-1", "POST", PAIR_PATH, raw)
        result = self.runtime.service.pair(principal, payload)
        # Crash after the pairing commit, before HTTP response persistence.
        self.runtime.service = MultiServerService(self.runtime)
        recovered = self.pair(payload)
        self.assertEqual(recovered.status_code, 200, recovered.text)
        self.assertEqual(recovered.json(), result)
        pending_raw = b'{"requestId":"unknown-1"}'
        self.runtime.service.reserve("client-1", "unknown-1", "POST", "/api/probe", pending_raw)
        concurrent = self.client.post("/api/probe", content=pending_raw, headers=self.signed("POST", "/api/probe", pending_raw, "unknown-1"))
        self.assertEqual(concurrent.json()["code"], "request_pending")
        self.runtime.service = MultiServerService(self.runtime)
        unknown = self.client.post("/api/probe", content=pending_raw, headers=self.signed("POST", "/api/probe", pending_raw, "unknown-1"))
        self.assertEqual(unknown.json()["code"], "outcome_unknown")
        self.assertEqual(self.effects(), 0)

    def test_revocation_blocks_read_write_pair_receipt_and_stream(self) -> None:
        body = self.pair_body()
        self.assertEqual(self.pair(body).status_code, 200)
        response = self.client.post("/api/multi-server", json={"action": "revoke", "clientId": "client-1", "requestId": "revoke-1"},
                                    headers={"X-Canvas-Token": "local-token"})
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(self.client.get("/api/probe", headers=self.signed("GET", "/api/probe")).status_code, 403)
        raw = b'{}'
        self.assertEqual(self.client.post("/api/probe", content=raw, headers=self.signed("POST", "/api/probe", raw, "revoked-write")).status_code, 403)
        self.assertEqual(self.pair(body).status_code, 403)
        self.assertEqual(self.effects(), 0)

    def test_cors_preflight_and_proxy_gate(self) -> None:
        headers = {**self.proxy(), "Origin": "https://ui.example.ts.net", "Access-Control-Request-Method": "POST",
                   "Access-Control-Request-Headers": "X-Studio-Client,Content-Type,X-Studio-API-Schema,Last-Event-ID"}
        result = self.client.options("/api/probe", headers=headers)
        self.assertEqual(result.status_code, 204)
        self.assertEqual(result.headers["Access-Control-Allow-Origin"], "*")
        self.assertNotIn("Access-Control-Allow-Credentials", result.headers)
        self.assertIn("X-Studio-API-Schema", result.headers["Access-Control-Expose-Headers"])
        invalid = self.client.get("/api/session", headers={**self.proxy(), "X-Forwarded-Host": "other.example.ts.net"})
        self.assertEqual(invalid.status_code, 403)
        duplicate = list(self.proxy().items()) + [("X-Forwarded-For", "100.64.0.99")]
        self.assertEqual(self.client.get("/api/session", headers=duplicate).status_code, 403)

    def test_real_sync_identity_and_stream(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        identity = self.client.get("/api/sync/identity", headers=self.signed("GET", "/api/sync/identity"))
        self.assertEqual(identity.status_code, 200, identity.text)
        self.context._resource_hub = ResourceHub(identity.json()["workspaceId"])
        target = "/api/sync/stream?" + urlencode({"protocol": "3", "resources": json.dumps([{"kind": "state"}])})
        response = self.client.get(target, headers=self.signed("GET", target))
        self.assertEqual(response.status_code, 200, response.text)
        self.assertIn("event: resources", response.text)
        self.assertIn("event: token-rates", response.text)
        self.context.close()

    def test_server_principal_gate_and_actual_orchestration_caller(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        raw = b'{"requestId":"remote-1","action":"projects","payload":{}}'
        refused = self.client.post("/api/servers/orchestration", content=raw,
                                   headers=self.signed("POST", "/api/servers/orchestration", raw, "remote-1"))
        self.assertEqual(refused.status_code, 403, refused.text)
        self.assertEqual(self.runtime.calls, [])
        payload = self.pair_body(self.invitation("invite-server"), "server", "source-server", "pair-server")
        self.assertEqual(self.pair(payload).status_code, 200)
        for _ in range(2):
            result = self.client.post("/api/servers/orchestration", content=raw,
                                      headers=self.signed("POST", "/api/servers/orchestration", raw, "remote-1", "source-server"))
            self.assertEqual(result.status_code, 200, result.text)
        self.assertEqual(len(self.runtime.calls), 1)
        self.assertEqual(self.runtime.calls[0][0], "source-server")
        self.assertEqual(self.runtime.service.servers()[0]["id"], "source-server")

    def test_key_permissions_and_stable_server_identity(self) -> None:
        server_id = self.runtime.service.local_server_id
        self.assertEqual((self.runtime.root / "multi-server" / "identity.json").stat().st_mode & 0o777, 0o600)
        self.assertEqual(MultiServerService(self.runtime).local_server_id, server_id)
        (self.runtime.root / "multi-server" / "identity.json").chmod(0o644)
        with self.assertRaises(AccessError):
            _ = MultiServerService(self.runtime).local_server_id

    def test_native_pair_transport_and_lost_response_retry(self) -> None:
        invitation = self.invitation()
        with tempfile.TemporaryDirectory(prefix="studio-access-peer-") as peer_dir:
            peer = FixtureRuntime(Path(peer_dir))
            self.addCleanup(lambda: peer.service.close())
            peer.service.public_origin = "https://peer.example.ts.net"
            wire_calls: list[dict[str, str]] = []
            lose_response = False

            def exchange(url: str, method: str, headers: dict[str, str], raw: bytes, timeout: float) -> dict[str, Any]:
                nonlocal lose_response
                self.assertEqual(timeout, 15)
                self.assertEqual(urlsplit(url).netloc, "server.example.ts.net")
                target = urlsplit(url).path
                if urlsplit(url).query:
                    target += "?" + urlsplit(url).query
                wire_calls.append(headers)
                response = self.client.request(method, target, content=raw, headers={**headers, **self.proxy()})
                if lose_response:
                    lose_response = False
                    raise AccessError(504, "remote_timeout", "The response was lost")
                return {"status": response.status_code, "url": url, "body": base64.b64encode(response.content).decode()}

            with patch("codex_multi_server._exchange", side_effect=exchange):
                snapshot = peer.service.accept_invite(invitation, "accept-1")
                self.assertEqual(snapshot["servers"][0]["id"], invitation["serverId"])
                self.assertEqual(self.runtime.service.servers()[0]["id"], peer.service.local_server_id)
                envelope = {"requestId": "native-op-1", "action": "projects", "payload": {}}
                lose_response = True
                with self.assertRaises(AccessError) as lost:
                    peer.service.request(invitation["serverId"], "POST", "/api/servers/orchestration", envelope, "native-op-1")
                self.assertEqual(lost.exception.code, "remote_timeout")
                peer.service = MultiServerService(peer)
                result = peer.service.request(invitation["serverId"], "POST", "/api/servers/orchestration", envelope, "native-op-1")
                self.assertEqual(result["outcome"], "applied")
                self.assertEqual(len(self.runtime.calls), 1)
                self.assertNotEqual(wire_calls[-1]["X-Studio-Nonce"], wire_calls[-2]["X-Studio-Nonce"])
                call_count = len(wire_calls)
                peer.service.request(invitation["serverId"], "POST", "/api/servers/orchestration", envelope, "native-op-1")
                self.assertEqual(len(wire_calls), call_count + 1)
                self.assertEqual(len(self.runtime.calls), 1)
                peer.service.revoke(invitation["serverId"])
                with self.assertRaises(AccessError) as revoked:
                    peer.service.request(invitation["serverId"], "GET", "/api/probe", None, "revoked-server")
                self.assertEqual(revoked.exception.code, "server_not_paired")

    def test_transport_refuses_redirect_and_retries_unknown_result(self) -> None:
        invitation = self.invitation()
        service = self.runtime.service
        reply = {"status": 302, "url": self.origin + "/api/probe", "body": base64.b64encode(b'{}').decode()}
        with patch("codex_multi_server._exchange", return_value=reply):
            with self.assertRaises(AccessError) as redirect:
                service._request(self.origin, invitation["serverId"], "GET", "/api/probe", None, "redirect", 15)
            self.assertEqual(redirect.exception.code, "redirect_refused")
        states = [{"requestId": "admit", "outcome": "unknown"}, {"requestId": "admit", "outcome": "applied", "value": {"ok": True}}]
        replies = [{"status": 200, "url": self.origin + "/api/servers/orchestration",
                    "body": base64.b64encode(json.dumps(value).encode()).decode()} for value in states]
        body = {"requestId": "admit", "action": "admission", "payload": {}}
        with patch("codex_multi_server._exchange", side_effect=replies) as transport:
            first = service._request(self.origin, invitation["serverId"], "POST", "/api/servers/orchestration", body, "admit", 15)
            second = service._request(self.origin, invitation["serverId"], "POST", "/api/servers/orchestration", body, "admit", 15)
            self.assertEqual(first["outcome"], "unknown")
            self.assertEqual(second["outcome"], "applied")
            self.assertEqual(transport.call_count, 2)

    def test_signature_identity_body_ids_and_audit(self) -> None:
        body = self.pair_body()
        self.assertEqual(self.pair(body).status_code, 200)
        headers = self.signed("GET", "/api/probe")
        headers["X-Studio-Server"] = "different-server"
        response = self.client.get("/api/probe", headers=headers)
        self.assertEqual(response.json()["code"], "server_mismatch")
        raw = b'{"request_id":"different-id"}'
        mismatch = self.client.post("/api/probe", content=raw, headers=self.signed("POST", "/api/probe", raw, "signed-id"))
        self.assertEqual(mismatch.json()["code"], "request_conflict")
        with patch.object(self.runtime.service.crypto, "call", side_effect=RuntimeError("private error")):
            # Build the proof before the verifier fails.
            verification = self.client.get("/api/probe", headers=headers)
            self.assertEqual(verification.json()["code"], "server_mismatch")
        signed = self.signed("GET", "/api/probe", request_id="crypto-check")
        with patch.object(self.runtime.service.crypto, "call", side_effect=RuntimeError("private error")):
            unavailable = self.client.get("/api/probe", headers=signed)
        self.assertEqual(unavailable.json()["code"], "crypto_unavailable")
        self.assertNotIn("private error", unavailable.text)
        audit = self.client.get("/api/multi-server/audit", headers=self.signed("GET", "/api/multi-server/audit", request_id="audit-1"))
        self.assertEqual(audit.status_code, 200, audit.text)
        self.assertEqual(audit.json()["records"][0]["actorId"], "client-1")
        self.assertEqual(audit.json()["records"][0]["action"], "pair")
        self.assertNotIn(body["token"], audit.text)
        self.assertEqual(self.effects(), 0)


    def test_review_mobile_probe_and_token_session_remain_supported(self) -> None:
        self.context.canvas.runtime = None
        desktop = self.client.get("/api/desktop", headers=self.proxy())
        self.assertEqual(desktop.status_code, 200, desktop.text)
        self.assertEqual(desktop.json()["publicOrigin"], self.origin)
        self.assertEqual(desktop.json()["mobileProtocol"], 1)
        session = self.client.get("/api/session", headers={**self.proxy(), "Origin": self.origin})
        self.assertEqual(session.json()["token"], "local-token")
        wrote = self.client.post("/api/probe", json={"value": 1},
                                 headers={**self.proxy(), "Origin": self.origin, "X-Canvas-Token": "local-token"})
        self.assertEqual(wrote.status_code, 200, wrote.text)
        refused = self.client.post("/api/probe", json={}, headers={**self.proxy(), "Origin": "https://evil.example"})
        self.assertEqual(refused.status_code, 403)

    def test_review_cors_exposes_remote_export_headers(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        response = self.client.get("/api/probe", headers=self.signed("GET", "/api/probe"))
        exposed = {header.strip().lower() for header in response.headers["Access-Control-Expose-Headers"].split(",")}
        self.assertIn("content-disposition", exposed)
        self.assertIn("x-log-truncated", exposed)

    def test_review_mobile_setup_and_https_probe_use_only_disposable_state(self) -> None:
        self.context.canvas.runtime = None
        entrypoint = Path(__file__).resolve().parents[2] / "codex-mobile.py"
        namespace = runpy.run_path(str(entrypoint), run_name="mobile_setup_fixture")

        def urlopen(url: str, **kwargs: object) -> io.BytesIO:
            remote = url.startswith("https://")
            result = self.client.get("/api/desktop", headers=self.proxy() if remote else {})
            self.assertEqual(result.status_code, 200, result.text)
            return io.BytesIO(result.content)

        status = {"BackendState": "Running", "Self": {"DNSName": "server.example.ts.net."}}
        with patch.object(sys, "argv", ["codex-mobile.py", "--port", "8765"]), \
             patch("shutil.which", return_value="/fixture/tailscale"), \
             patch("subprocess.check_output", side_effect=[json.dumps(status), "{}"]) as read, \
             patch("subprocess.run") as configure, \
             patch("urllib.request.urlopen", side_effect=urlopen), patch("builtins.print"):
            namespace["main"]()
        configure.assert_called_once_with(["/fixture/tailscale", "serve", "--bg", "--https=443", "http://127.0.0.1:8765"],
                                          check=True, timeout=60)
        self.assertEqual(read.call_count, 2)
        self.assertEqual(json.loads((self.runtime.root / "remote-access.json").read_text()), {"enabled": True, "origin": self.origin})

    def test_review_schema_rejection_can_retry_with_the_current_schema(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        raw = b'{"requestId":"schema-retry"}'
        headers = self.signed("POST", "/api/probe", raw, "schema-retry")
        headers["X-Studio-API-Schema"] = "obsolete"
        self.assertEqual(self.client.post("/api/probe", content=raw, headers=headers).status_code, 426)
        headers = self.signed("POST", "/api/probe", raw, "schema-retry")
        headers["X-Studio-API-Schema"] = "access-schema"
        self.assertEqual(self.client.post("/api/probe", content=raw, headers=headers).status_code, 200)
        self.assertEqual(self.effects(), 1)

    def test_review_workspace_rejection_can_retry_with_the_current_workspace(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        raw = b'{"requestId":"workspace-retry"}'
        with patch.object(self.context, "workspace_id", return_value="workspace-current"):
            headers = self.signed("POST", "/api/probe", raw, "workspace-retry")
            headers["X-Canvas-Workspace"] = "workspace-old"
            self.assertEqual(self.client.post("/api/probe", content=raw, headers=headers).status_code, 409)
            headers = self.signed("POST", "/api/probe", raw, "workspace-retry")
            headers["X-Canvas-Workspace"] = "workspace-current"
            self.assertEqual(self.client.post("/api/probe", content=raw, headers=headers).status_code, 200)
        self.assertEqual(self.effects(), 1)

    def test_review_nested_timeout_retries_the_same_outbound_pairing(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        invitation = {**self.invitation("target-invite"), "serverId": "peer-id", "origin": "https://peer.example.ts.net"}
        result = {"protocol": 1, "serverId": "peer-id", "origin": invitation["origin"], "label": "Peer",
                  "publicKey": invitation["publicKey"], "tailscaleUser": "owner@example.com",
                  "clientId": self.runtime.service.local_server_id, "paired": True}
        reply = {"status": 200, "url": invitation["origin"] + PAIR_PATH,
                 "body": base64.b64encode(json.dumps(result).encode()).decode()}
        raw = json.dumps({"action": "accept_invite", "invitation": invitation, "requestId": "nested-retry"}).encode()
        with patch("codex_multi_server._exchange", side_effect=[AccessError(504, "remote_timeout", "Timed out"), reply]) as transport:
            first = self.client.post("/api/multi-server", content=raw,
                                     headers=self.signed("POST", "/api/multi-server", raw, "nested-retry"))
            self.assertEqual(first.status_code, 504, first.text)
            second = self.client.post("/api/multi-server", content=raw,
                                      headers=self.signed("POST", "/api/multi-server", raw, "nested-retry"))
            self.assertEqual(second.status_code, 200, second.text)
            self.assertEqual(transport.call_count, 2)

    def test_review_unknown_failed_mutation_is_not_repeated(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        raw = b'{"requestId":"lost-effect","failAfterEffect":true}'
        first = self.client.post("/api/probe", content=raw,
                                 headers=self.signed("POST", "/api/probe", raw, "lost-effect"))
        self.assertEqual(first.status_code, 503)
        retry = self.client.post("/api/probe", content=raw,
                                 headers=self.signed("POST", "/api/probe", raw, "lost-effect"))
        self.assertEqual(retry.json()["code"], "outcome_unknown")
        self.assertEqual(self.effects(), 1)

    def test_review_failed_pairing_has_no_durable_receipt(self) -> None:
        body = self.pair_body()
        body["label"] = ""
        self.assertEqual(self.pair(body).status_code, 400)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_receipts").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_nonces").fetchone()[0], 0)

    def test_review_response_identity_must_match_the_registered_peer(self) -> None:
        invitation = self.invitation()
        with self.runtime.db() as db:
            db.execute("INSERT INTO runtime_access_clients VALUES(?,?)", ("peer-id", json.dumps({
                "id": "peer-id", "clientId": "peer-id", "serverId": "peer-id", "kind": "server", "status": "paired",
                "origin": self.origin, "publicKey": invitation["publicKey"]})))
        for index, identity in enumerate(({"serverId": "wrong-id"}, {"publicKey": self.keys["publicKey"]})):
            reply = {"status": 200, "url": self.origin + "/api/probe", "body": base64.b64encode(json.dumps(identity).encode()).decode()}
            with patch("codex_multi_server._exchange", return_value=reply):
                with self.assertRaises(AccessError) as refused:
                    self.runtime.service.request("peer-id", "GET", "/api/probe", None, f"pin-{index}")
                self.assertEqual(refused.exception.code, "server_mismatch")

    def test_review_invalid_invitations_create_no_durable_attempt_state(self) -> None:
        invitation = self.invitation()
        invalid = {**invitation, "inviteId": "missing-invite"}
        body = self.pair_body(invalid)
        self.assertEqual(self.pair(body).status_code, 403)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_nonces").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_receipts").fetchone()[0], 0)

    def test_review_identity_checks_use_a_short_cache(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        with patch("codex_multi_server._owner_login", return_value="owner@example.com") as owner, \
             patch("codex_multi_server._peer_login", return_value="owner@example.com") as peer:
            for index in range(4):
                headers = {**self.signed("GET", "/api/probe", request_id=f"cache-{index}"),
                           "Tailscale-User-Login": "owner@example.com"}
                self.assertEqual(self.client.get("/api/probe", headers=headers).status_code, 200)
            self.assertLessEqual(owner.call_count, 1)
            self.assertLessEqual(peer.call_count, 1)

    def test_review_missing_serve_login_rechecks_changed_peer_identity(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        for identity in ({"Node": {}, "UserProfile": {"LoginName": "other@example.com"}},
                         {"Node": {"Tags": ["tag:server"]}, "UserProfile": {"LoginName": "owner@example.com"}}):
            with self.subTest(identity=identity):
                self.runtime.service.identity_cache["100.64.0.12"] = (time.monotonic(), "owner@example.com")
                with patch("codex_multi_server._peer_login", side_effect=_peer_login), \
                     patch("codex_multi_server._tailscale_json", return_value=identity) as whois:
                    headers = self.signed("GET", "/api/probe", request_id="changed-peer")
                    self.assertNotIn("Tailscale-User-Login", headers)
                    response = self.client.get("/api/probe", headers=headers)
                self.assertEqual(response.status_code, 403, response.text)
                whois.assert_called_once_with("whois", "--json", "100.64.0.12")
                self.assertEqual(self.runtime.service.identity_cache, {})

    def test_review_orchestration_responses_are_not_persisted_in_the_generic_cache(self) -> None:
        invitation = self.invitation()
        body = {"requestId": "chunk-read", "action": "chunk", "payload": {"exportId": "export", "offset": 0}}
        reply = {"status": 200, "url": self.origin + "/api/servers/orchestration",
                 "body": base64.b64encode(json.dumps({"outcome": "applied", "value": {"data": "BINARY-DATA"}}).encode()).decode()}
        with patch("codex_multi_server._exchange", return_value=reply) as transport:
            for _ in range(2):
                self.runtime.service._request(self.origin, invitation["serverId"], "POST", "/api/servers/orchestration", body, "chunk-read", 15)
            self.assertEqual(transport.call_count, 2)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_outbound").fetchone()[0], 0)

    def test_review_identity_cache_expires_and_clears_after_an_error(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        self.runtime.service.identity_cache.clear()
        with patch("codex_multi_server.IDENTITY_CACHE_SECONDS", 0), \
             patch("codex_multi_server._owner_login", side_effect=AccessError(503, "identity_unavailable", "Unavailable")):
            failed = self.client.get("/api/probe", headers=self.signed("GET", "/api/probe", request_id="cache-expired"))
            self.assertEqual(failed.status_code, 503)
            self.assertEqual(self.runtime.service.identity_cache, {})
        with patch("codex_multi_server._owner_login", return_value="owner@example.com") as owner:
            retried = self.client.get("/api/probe", headers=self.signed("GET", "/api/probe", request_id="cache-expired"))
            self.assertEqual(retried.status_code, 200)
            self.assertEqual(owner.call_count, 1)

    def test_review_global_pairing_limit_has_no_durable_invalid_rows(self) -> None:
        body = self.pair_body({**self.invitation(), "inviteId": "missing"})
        raw = json.dumps(body).encode()
        headers = self.signed("POST", PAIR_PATH, raw, "pair-1")
        for _ in range(129):
            response = self.client.post(PAIR_PATH, content=raw, headers=headers)
        self.assertEqual(response.status_code, 429)
        self.assertEqual(response.json()["code"], "pairing_limit")
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_nonces").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_receipts").fetchone()[0], 0)

    def test_review_old_receipts_compact_and_nonces_expire(self) -> None:
        self.invitation()
        service = self.runtime.service
        old = time.time() - 8 * 86400
        raw = b'{"requestId":"old-op"}'
        service.reserve("old-client", "old-op", "POST", "/api/probe", raw)
        service.complete("old-client", "old-op", 200, [], b'{"large":"response"}')
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_access_receipts SET record=json_set(record,'$.completed',?)", (old,))
            db.execute("INSERT INTO runtime_access_nonces VALUES('old-client','old-nonce',?)", (int(old),))
            db.execute("INSERT INTO runtime_access_outbound VALUES('peer','old-out','hash',?)", (json.dumps({
                "outcome": "complete", "value": {"large": "response"}, "completed": old}),))
        service.create_invite({"requestId": "maintenance"})
        with service.runtime.read_db() as db:
            inbound = json.loads(db.execute("SELECT record FROM runtime_access_receipts").fetchone()[0])
            outbound = json.loads(db.execute("SELECT record FROM runtime_access_outbound").fetchone()[0])
            self.assertNotIn("body", inbound)
            self.assertNotIn("value", outbound)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_nonces").fetchone()[0], 0)
            self.assertLess(len(json.dumps(inbound)), 150)
            self.assertLess(len(json.dumps(outbound)), 150)
        with self.assertRaises(AccessError) as expired:
            service.reserve("old-client", "old-op", "POST", "/api/probe", raw)
        self.assertEqual(expired.exception.code, "receipt_expired")
        with self.assertRaises(AccessError) as conflict:
            service.reserve("old-client", "old-op", "POST", "/api/probe", b'{"changed":true}')
        self.assertEqual(conflict.exception.code, "request_conflict")

    def test_review_retained_response_bytes_stay_bounded_by_age(self) -> None:
        self.invitation()
        old = time.time() - 8 * 86400
        large = json.dumps({"completed": old, "status": 200, "headers": [], "body": base64.b64encode(b"x" * 4096).decode()})
        with self.runtime.db() as db:
            db.executemany("INSERT INTO runtime_access_receipts VALUES(?,?,?,'old','complete',?)",
                           [("old-client", f"old-{index}", f"hash-{index}", large) for index in range(300)])
            db.execute("INSERT INTO runtime_access_receipts VALUES('old-client','uncertain','hash','old','pending','{}')")
            before = db.execute("SELECT sum(length(record)) FROM runtime_access_receipts").fetchone()[0]
        self.runtime.service.create_invite({"requestId": "compact-old"})
        with self.runtime.read_db() as db:
            after = db.execute("SELECT sum(length(record)) FROM runtime_access_receipts").fetchone()[0]
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_receipts WHERE status='tombstone'").fetchone()[0], 300)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_access_receipts WHERE status='complete'").fetchone()[0], 0)
            self.assertEqual(db.execute("SELECT status FROM runtime_access_receipts WHERE request_id='uncertain'").fetchone()[0], "pending")
        self.assertLess(after, before / 20)

    def test_active_stream_closes_after_revocation(self) -> None:
        self._assert_stream_closes(lambda: self.runtime.service.revoke("client-1"))

    def test_review_disabling_remote_access_closes_the_stream(self) -> None:
        self._assert_stream_closes(lambda: setattr(self.context.remote, "override", None))

    def test_review_changing_the_remote_origin_closes_the_stream(self) -> None:
        self._assert_stream_closes(lambda: setattr(self.context.remote, "override", "https://new.example.ts.net"))

    def test_review_unsigned_phone_stream_closes_when_remote_access_is_disabled(self) -> None:
        self._assert_real_unsigned_stream_policy({"enabled": False, "origin": self.origin})

    def test_review_unsigned_phone_stream_closes_when_remote_origin_changes(self) -> None:
        self._assert_real_unsigned_stream_policy({"enabled": True, "origin": "https://new.example.ts.net"})

    def test_review_local_stream_survives_remote_access_disable(self) -> None:
        self._assert_real_unsigned_stream_policy({"enabled": False, "origin": self.origin}, local=True)

    def _assert_real_unsigned_stream_policy(self, updated: dict[str, Any], *, local: bool = False) -> None:
        self.runtime.closed = False
        remote = cast(RemoteAccess, self.context.remote)
        remote.override = None
        remote.path.write_text(json.dumps({"enabled": True, "origin": self.origin}))
        identity = self.client.get("/api/sync/identity")
        self.assertEqual(identity.status_code, 200, identity.text)
        hub = ResourceHub(identity.json()["workspaceId"])
        self.context._resource_hub = hub
        self.addCleanup(self.context.close)
        query = urlencode({"protocol": "3", "resources": json.dumps([{"kind": "state"}])}).encode()
        messages: list[Message] = []

        async def run() -> None:
            ready, disconnected, changed = asyncio.Event(), asyncio.Event(), asyncio.Event()
            consumed = False
            headers = {} if local else {**self.proxy(), "Origin": self.origin}
            scope: Scope = {"type": "http", "http_version": "1.1", "scheme": "http", "root_path": "",
                            "path": "/api/sync/stream", "raw_path": b"/api/sync/stream", "query_string": query,
                            "method": "GET", "client": ("127.0.0.1", 9), "server": ("127.0.0.1", 8765),
                            "headers": [(b"host", b"127.0.0.1:8765"),
                                        *[(key.lower().encode(), value.encode()) for key, value in headers.items()]]}

            async def receive() -> Message:
                nonlocal consumed
                if not consumed:
                    consumed = True
                    return {"type": "http.request", "body": b"", "more_body": False}
                await disconnected.wait()
                return {"type": "http.disconnect"}

            async def send(message: Message) -> None:
                messages.append(message)
                if message["type"] == "http.response.body":
                    if b"event: token-rates" in message.get("body", b""):
                        ready.set()
                    if b'"reason":"overflow"' in message.get("body", b""):
                        changed.set()

            request = asyncio.create_task(self.app(scope, receive, send))
            try:
                await asyncio.wait_for(ready.wait(), 3)
                remote.path.write_text(json.dumps(updated))
                if local:
                    # Wait through several policy checks, then verify live delivery.
                    await asyncio.sleep(0.05)
                    self.assertFalse(request.done())
                    hub.publish_overflow()
                    await asyncio.wait_for(changed.wait(), 3)
                    disconnected.set()
                await asyncio.wait_for(asyncio.shield(request), 1)
                self.assertFalse(hub._subscriptions)
                if not local:
                    count = len(messages)
                    hub.publish_overflow()
                    await asyncio.sleep(0)
                    self.assertEqual(len(messages), count)
            finally:
                request.cancel()
                await asyncio.gather(request, return_exceptions=True)

        with patch("studio_api.multi_server.boundary.STREAM_RECHECK_SECONDS", 0.01):
            asyncio.run(run())
        self.assertEqual(messages[0]["status"], 200)
        if not local:
            self.assertFalse(messages[-1].get("more_body", False))

    def _assert_stream_closes(self, change: Callable[[], object]) -> None:
        self.assertEqual(self.pair().status_code, 200)
        target = "/api/sync/stream"
        signed = self.signed("GET", target)
        messages: list[Message] = []
        started = asyncio.Event()

        async def stream_app(scope: Scope, receive: Receive, send: Send) -> None:
            await send({"type": "http.response.start", "status": 200, "headers": []})
            await send({"type": "http.response.body", "body": b"event: heartbeat\n\n", "more_body": True})
            started.set()
            await asyncio.Event().wait()

        async def run() -> None:
            scope: Scope = {"type": "http", "path": target, "raw_path": target.encode(), "query_string": b"",
                                     "method": "GET", "client": ("127.0.0.1", 9), "server": ("127.0.0.1", 8765),
                                     "headers": [(b"host", b"127.0.0.1:8765"), *[(key.lower().encode(), value.encode()) for key, value in signed.items()]]}

            async def receive() -> Message:
                return {"type": "http.request", "body": b""}

            async def send(message: Message) -> None:
                messages.append(message)

            boundary = MultiServerBoundary(stream_app, self.context)
            request = asyncio.create_task(boundary(scope, receive, send))
            await asyncio.wait_for(started.wait(), 5)
            change()
            await asyncio.wait_for(request, 3)

        with patch("studio_api.multi_server.boundary.STREAM_RECHECK_SECONDS", 0.01):
            asyncio.run(run())
        self.assertEqual(messages[-1]["type"], "http.response.body")
        self.assertFalse(messages[-1].get("more_body", False))

    def test_stream_error_closes_without_a_second_response_start(self) -> None:
        self.assertEqual(self.pair().status_code, 200)
        target = "/api/sync/stream"
        signed = self.signed("GET", target)
        messages: list[Message] = []

        async def stream_app(scope: Scope, receive: Receive, send: Send) -> None:
            await send({"type": "http.response.start", "status": 200, "headers": []})
            await send({"type": "http.response.body", "body": b"event: heartbeat\n\n", "more_body": True})
            raise RuntimeError("The stream failed")

        async def run() -> None:
            scope: Scope = {"type": "http", "path": target, "raw_path": target.encode(), "query_string": b"",
                            "method": "GET", "client": ("127.0.0.1", 9), "server": ("127.0.0.1", 8765),
                            "headers": [(b"host", b"127.0.0.1:8765"), *[(key.lower().encode(), value.encode()) for key, value in signed.items()]]}

            async def receive() -> Message:
                return {"type": "http.request", "body": b""}

            async def send(message: Message) -> None:
                messages.append(message)

            await MultiServerBoundary(stream_app, self.context)(scope, receive, send)

        asyncio.run(run())
        self.assertEqual(sum(message["type"] == "http.response.start" for message in messages), 1)
        self.assertEqual(messages[-1]["type"], "http.response.body")
        self.assertFalse(messages[-1].get("more_body", False))


if __name__ == "__main__":
    unittest.main()
