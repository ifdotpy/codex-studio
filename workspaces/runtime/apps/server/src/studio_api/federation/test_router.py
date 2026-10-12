"""HTTP boundary tests for the federation FastAPI router."""

from __future__ import annotations

import hashlib
import json
import tempfile
import threading
import time
import unittest
from email.message import Message
from pathlib import Path
from typing import TYPE_CHECKING, cast
from unittest.mock import patch

from codex_federation import FederationService, _message_bytes
from codex_records import JsonObject
from fastapi import FastAPI
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from httpx import Response
from pydantic import TypeAdapter, ValidationError
from starlette.middleware.base import RequestResponseEndpoint
from starlette.requests import Request
from starlette.responses import Response as StarletteResponse
from studio_api.context import ApiContext
from studio_api.federation.models import (
    CreateInviteResponse,
    FederationSnapshot,
    MessageRequest,
    PairRequest,
    PullRequest,
    StatusRequest,
)
from studio_api.middleware import RequestBoundary
from studio_api.models import ErrorResponse
from studio_api.responses import install_error_response_docs

if TYPE_CHECKING:
    from codex_canvas import Canvas
    from codex_records import RecordStore
    from codex_remote import RemoteAccess
from studio_api.federation.router import FEDERATION_PATHS, MAX_BODY_BYTES, create_router

SIGNED_MODELS = {
    "pair": PairRequest,
    "status": StatusRequest,
    "message": MessageRequest,
    "pull": PullRequest,
}


class FakeService:
    def __init__(self) -> None:
        self.calls: list[tuple[str, bytes]] = []
        self.signed_bytes: list[tuple[str, bytes]] = []
        self.actions: list[dict[str, object]] = []
        self.writes = 0
        self.result: dict[str, object] = {
            "stateId": "local-state",
            "timestamp": 1,
            "nonce": "abcdefghijklmnop",
            "result": {"protocol": 1, "approved": True, "revoked": False},
            "signature": "signature",
        }

    def route(self, action: str, headers: object, peer: str, raw: bytes) -> dict[str, object]:
        self.signed_bytes.append((action, raw))
        if raw.startswith(b"{\n"):
            raise PermissionError("Invalid federation request signature")
        TypeAdapter(SIGNED_MODELS[action]).validate_json(raw)
        self.calls.append((action, raw))
        self.writes += 1
        return self.result

    def action(self, body: dict[str, object]) -> dict[str, object]:
        self.actions.append(body)
        if body.get("action") == "create_invite":
            return {
                "invitation": {
                    "protocol": 1,
                    "inviteId": "invite-id",
                    "token": "token",
                    "stateId": "state-id",
                    "label": "Studio",
                    "origin": "https://studio.example.ts.net",
                    "publicKey": "public-key",
                    "expires": 123,
                },
                "expires": 123,
                "warning": "Share privately.",
            }
        return {
            "version": 1,
            "enabled": False,
            "identity": None,
            "peers": [],
            "invites": [],
            "rooms": [],
            "queued": 0,
        }


class FakeRuntime:
    def __init__(self) -> None:
        self.service = FakeService()

    def federation(self) -> FakeService:
        return self.service


class FakeCanvas:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.runtime = FakeRuntime()


class FakeRemote:
    def __init__(self) -> None:
        self.calls: list[tuple[str, int]] = []

    def request_origin(self, _headers: Message, _peer: str, _port: int) -> str | None:
        self.calls.append((_peer, _port))
        return "https://studio.example.ts.net"


class FakeContext:
    def __init__(self, root: Path, runtime: FakeRuntime) -> None:
        canvas = FakeCanvas(root)
        canvas.runtime = runtime
        self.canvas = cast("Canvas", canvas)
        self.server_port = 8765
        self.unix_socket = False
        self.schema_only = False
        self.token = "federation-session-token"
        self.remote_fixture = FakeRemote()
        self.remote = cast("RemoteAccess", self.remote_fixture)

    async def get_api_schema_hash(self) -> str:
        return "federation-test-schema"

    def peek_api_schema_hash(self) -> str:
        return "federation-test-schema"

    def start_api_schema_hash(self) -> None:
        return None

    @property
    def runtime(self) -> FakeRuntime:
        return cast(FakeRuntime, self.canvas.runtime)

    def entity_sequence(self) -> int:
        return 0

    def send(self, request: Request, value: object, status: int = 200) -> StarletteResponse:
        request.scope.pop("studio_sync_entities_after", None)
        return ApiContext.send(cast(ApiContext, self), request, value, status=status)

    @staticmethod
    def _dump_json(value: object) -> object:
        return ApiContext._dump_json(value)

    @staticmethod
    def _accepts_gzip(value: str) -> bool:
        return ApiContext._accepts_gzip(value)

    @staticmethod
    def _response_contract(candidate: object) -> bool:
        return ApiContext._response_contract(candidate)


class FederationRouterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = FakeRuntime()
        self.context = FakeContext(Path(self.temp.name), self.runtime)
        app = FastAPI()
        app.include_router(create_router(cast(ApiContext, self.context)))
        app.add_middleware(RequestBoundary, context=cast(ApiContext, self.context))

        @app.middleware("http")
        async def bind_listener_scope(
            request: Request, call_next: RequestResponseEndpoint
        ) -> StarletteResponse:
            request.scope["server"] = ("127.0.0.1", 4321)
            return await call_next(request)

        @app.exception_handler(RequestValidationError)
        async def invalid_request(request: Request, _error: RequestValidationError) -> StarletteResponse:
            return self.context.send(request, ErrorResponse(error="Invalid request"), status=400)

        install_error_response_docs(app)
        self.client = TestClient(app, headers={"X-Canvas-Token": self.context.token})
        self.addCleanup(self.client.close)
        self.addCleanup(self.temp.cleanup)

    def post_signed(self, body: bytes, **headers: str) -> Response:
        return cast(Response, self.client.post(
            "/api/federation/v1/status",
            content=body,
            headers={"content-type": "application/json", **headers},
        ))

    def post_signed_path(self, path: str, body: bytes, **headers: str) -> Response:
        return cast(Response, self.client.post(
            path,
            content=body,
            headers={"content-type": "application/json", **headers},
        ))

    def test_valid_signed_operations_accept_schema_bodies_without_reserializing(self) -> None:
        bodies = {
            "/api/federation/v1/pair": b'{ "protocol": 1, "inviteId": "invite", "token": "secret", "stateId": "peer", "label": "Peer", "origin": "https://peer.example", "publicKey": "key" }',
            "/api/federation/v1/status": b'{ "protocol": 1 }',
            "/api/federation/v1/message": (
                b'{ "protocol": 1, "envelope": { "protocol": 1, "senderServer": "peer", '
                b'"id": "message", "kind": "chat-message", "room": "room", "payload": {}, '
                b'"created": 1, "timestamp": 1, "nonce": "abcdefghijklmnop", "signature": "sig" } }'
            ),
            "/api/federation/v1/pull": b'{ "protocol": 1, "acks": [] }',
        }
        for path, raw in bodies.items():
            self.runtime.service.calls.clear()
            self.runtime.service.signed_bytes.clear()
            response = self.post_signed_path(path, raw)
            self.assertEqual(response.status_code, 200, (path, response.text))
            action = path.rsplit("/", 1)[-1]
            self.assertEqual(self.runtime.service.calls, [(action, raw)])
            self.assertEqual(self.runtime.service.signed_bytes, [(action, raw)])

    def test_malformed_and_non_object_signed_json_use_body_location(self) -> None:
        service = FederationService(cast("RecordStore", self.runtime))
        self.runtime.service = cast(FakeService, service)
        try:
            with patch.object(service, "enabled", return_value=True):
                for path in FEDERATION_PATHS:
                    for raw in (b"{", b"null"):
                        with self.subTest(path=path, raw=raw):
                            response = self.post_signed_path(path, raw)
                            self.assertEqual(response.status_code, 400)
                            self.assertEqual(
                                response.json(),
                                {"error": "Invalid request", "location": ["body"]},
                            )
        finally:
            self.runtime.service = FakeService()

    def test_actual_service_validates_models_after_signature_checks(self) -> None:
        class Database:
            def __init__(self, writes: list[tuple[str, object]]) -> None:
                self.writes = writes

            class Cursor:
                def __init__(self, row: tuple[str] | None) -> None:
                    self.row = row

                def fetchone(self) -> tuple[str] | None:
                    return self.row

            def execute(self, query: str, parameters: object = ()) -> Database.Cursor:
                if "runtime_federation_invites" in query:
                    return self.Cursor((json.dumps({
                        "expires": time.time() + 60,
                        "tokenHash": hashlib.sha256(b"token").hexdigest(),
                        "status": "open",
                    }),))
                if "runtime_federation_identity" in query:
                    return self.Cursor((json.dumps({"stateId": "local", "privateKey": "key"}),))
                if "INSERT INTO runtime_federation_peers" in query:
                    self.writes.append((query, parameters))
                return self.Cursor(None)

            def __enter__(self) -> Database:
                return self

            def __exit__(self, *_args: object) -> None:
                return None

        class Runtime:
            lock = threading.RLock()

            def __init__(self) -> None:
                self.writes: list[tuple[str, object]] = []

            def db(self) -> Database:
                return Database(self.writes)

        peer_id = "12345678-1234-4234-8234-123456789012"
        headers = {
            "X-Studio-Federation-State": peer_id,
            "X-Studio-Federation-Time": str(int(time.time())),
            "X-Studio-Federation-Nonce": "abcdefghijklmnop",
            "X-Studio-Federation-Signature": "signature",
        }
        base_bodies: dict[str, dict[str, object]] = {
            "status": {"protocol": 1},
            "pull": {"protocol": 1, "acks": []},
            "message": {
                "protocol": 1,
                "envelope": {
                    "protocol": 1,
                    "senderServer": "peer",
                    "id": "message",
                    "kind": "chat-message",
                    "room": "room",
                    "payload": {},
                    "created": 1,
                    "timestamp": 1,
                    "nonce": "abcdefghijklmnop",
                    "signature": "signature",
                },
            },
            "pair": {
                "protocol": 1,
                "inviteId": "invite",
                "token": "token",
                "stateId": peer_id,
                "label": "Peer",
                "origin": "https://peer.example",
                "publicKey": "public-key",
            },
        }
        fake_runtime = Runtime()
        service = FederationService(cast("RecordStore", fake_runtime))
        cast(FakeRuntime, self.context.canvas.runtime).service = cast(FakeService, service)
        with (
            patch.object(service, "enabled", return_value=True),
            patch.object(service, "_verified_peer", return_value={"status": "approved"}) as verify_peer,
            patch.object(service, "_validate_peer_origin"),
            patch("codex_federation._request_bytes", return_value=b"signature-input") as request_bytes,
            patch("codex_federation._verify", return_value=True) as verify_pair,
            patch("codex_remote.validate_origin", return_value="https://peer.example"),
            patch.object(service, "_consume_nonce") as consume_nonce,
            patch.object(service, "signed_response") as signed_response,
            patch.object(service, "receive_envelope") as receive_envelope,
        ):
            signed_response.side_effect = lambda result: {
                "stateId": "local-state",
                "timestamp": 1,
                "nonce": "abcdefghijklmnop",
                "result": result,
                "signature": "signature",
            }
            for action, base in base_bodies.items():
                for mutation in ("boolean", "float", "extra"):
                    body = dict(base)
                    if mutation == "boolean":
                        body["protocol"] = True
                    elif mutation == "float":
                        body["protocol"] = 1.0
                    elif action == "message":
                        cast(dict[str, object], body["envelope"])["unexpected"] = "placeholder"
                    else:
                        body["unexpected"] = "placeholder"
                    raw = (" {\n  " + json.dumps(body)[1:-1] + "\n} ").encode()
                    path = f"/api/federation/v1/{action}"
                    response = self.post_signed_path(path, raw, **headers)
                    self.assertEqual(response.status_code, 400, (action, mutation, response.text))
                    expected_location = (
                        ["body", "envelope", "unexpected"]
                        if action == "message" and mutation == "extra"
                        else ["body", "unexpected"] if mutation == "extra"
                        else ["body", "protocol"]
                    )
                    self.assertEqual(
                        response.json(),
                        {"error": "Invalid request", "location": expected_location},
                    )
                    if action == "pair":
                        self.assertEqual(request_bytes.call_args.args[4], raw)
                        verify_pair.assert_called_with("public-key", b"signature-input", "signature")
                    else:
                        self.assertEqual(verify_peer.call_args.args[5], raw)
                    consume_nonce.assert_not_called()
                    signed_response.assert_not_called()
                    receive_envelope.assert_not_called()

            raw = b'{ "protocol": 1 }'
            response = self.post_signed_path(
                "/api/federation/v1/status",
                raw,
                **headers,
            )
            self.assertEqual(response.status_code, 200, response.text)
            self.assertEqual(verify_peer.call_args.args[5], raw)

            received: list[JsonObject] = []
            for created in (1.25, 1):
                envelope: JsonObject = {
                    "protocol": 1,
                    "senderServer": peer_id,
                    "id": f"message-{created}",
                    "kind": "chat-message",
                    "room": "room",
                    "payload": {"nested": {"items": [1, {"ok": True}]}, "created": created},
                    "created": created,
                    "timestamp": 1,
                    "nonce": "abcdefghijklmnop",
                    "signature": "envelope-signature",
                }
                body = {"protocol": 1, "envelope": envelope}
                raw = json.dumps(body, separators=(",", ":")).encode()
                def receive(_peer: object, candidate: JsonObject) -> dict[str, object]:
                    verify_pair("public-key", _message_bytes(candidate), str(candidate["signature"]))
                    received.append(candidate.copy())
                    return {
                        "protocol": 1,
                        "messageId": envelope["id"],
                        "accepted": True,
                        "room": "room",
                        "sequence": 1,
                    }

                with patch.object(
                    service,
                    "receive_envelope",
                    side_effect=receive,
                ):
                    response = self.post_signed_path("/api/federation/v1/message", raw, **headers)
                self.assertEqual(response.status_code, 200, response.text)
                self.assertEqual(received[-1], envelope)
                self.assertEqual(service._hash_envelope(received[-1]), service._hash_envelope(envelope))
                verify_pair.assert_called_with("public-key", _message_bytes(envelope), "envelope-signature")

            pair_body = {
                "protocol": 1,
                "inviteId": "invite",
                "token": "token",
                "stateId": peer_id,
                "label": "Peer",
                "origin": "https://peer.example",
                "publicKey": "public-key",
            }
            pair_raw = json.dumps(pair_body, separators=(",", ":")).encode()
            with (
                patch.object(service, "_validate_whois", return_value="missing"),
                patch.object(service, "_observed_login", return_value=None),
            ):
                response = self.post_signed_path("/api/federation/v1/pair", pair_raw, **headers)
            self.assertEqual(response.status_code, 200, response.text)
            peer_write = next(parameters for query, parameters in fake_runtime.writes if "runtime_federation_peers" in query)
            peer_parameters = cast(tuple[object, str], peer_write)
            stored_peer = json.loads(peer_parameters[1])
            for optional in ("leadName", "tailscaleUser", "tailscaleStatus"):
                self.assertNotIn(optional, stored_peer)

    def test_invalid_signature_passes_exact_raw_bytes_without_service_write(self) -> None:
        raw = b'{\n  "protocol": 1\n}'
        response = self.post_signed(raw)
        self.assertEqual(response.status_code, 403)
        self.assertEqual(response.json(), {"error": "Invalid federation request signature"})
        self.assertEqual(self.runtime.service.signed_bytes, [("status", raw)])
        self.assertEqual(self.runtime.service.calls, [])
        self.assertEqual(self.runtime.service.writes, 0)

    def test_public_origin_gate_runs_before_service(self) -> None:
        with patch(
            "studio_api.federation.test_router.FakeRemote.request_origin",
            side_effect=("https://studio.example.ts.net", None),
        ):
            response = self.post_signed(b'{"protocol":1}')
        self.assertEqual(response.status_code, 403)
        self.assertEqual(response.json(), {"error": "Tailscale Serve origin required"})
        self.assertEqual(self.runtime.service.calls, [])
        self.assertEqual(self.runtime.service.writes, 0)

    def test_listener_port_comes_from_asgi_scope_not_host_header(self) -> None:
        response = self.client.post(
            "/api/federation/v1/status",
            content=b'{"protocol":1}',
            headers={"content-type": "application/json", "host": "spoofed.example:1234"},
        )
        self.assertEqual(response.status_code, 200)
        self.assertTrue(self.context.remote_fixture.calls)
        self.assertTrue(all(port == 4321 for _, port in self.context.remote_fixture.calls))
        self.assertNotIn(1234, [port for _, port in self.context.remote_fixture.calls])

    def test_get_has_no_api_counterpart_and_returns_legacy_not_found(self) -> None:
        response = self.client.get("/api/federation/v1/status")
        self.assertEqual(response.status_code, 404)
        self.assertEqual(response.json(), {"error": "Not found"})
        self.assertEqual(self.runtime.service.calls, [])

        management = self.client.get("/api/federation")
        self.assertEqual(management.status_code, 404)
        self.assertEqual(management.json(), {"error": "Not found"})

    def test_management_session_token_gate_runs_before_service(self) -> None:
        response = self.client.post(
            "/api/federation",
            content=b'{"action":"status"}',
            headers={"X-Canvas-Token": ""},
        )
        self.assertEqual(response.status_code, 403)
        self.assertEqual(self.runtime.service.actions, [])

    def test_content_type_rejected_before_service(self) -> None:
        response = self.client.post(
            "/api/federation/v1/status",
            content=b'{"protocol":1}',
            headers={"content-type": "text/plain"},
        )
        self.assertEqual(response.status_code, 415)
        self.assertEqual(response.json(), {"error": "JSON required", "location": ["body"]})
        self.assertEqual(self.runtime.service.calls, [])

    def test_management_content_type_rejected_before_service(self) -> None:
        response = self.client.post(
            "/api/federation",
            content=b'{"action":"status"}',
            headers={"content-type": "text/plain"},
        )
        self.assertEqual(response.status_code, 415)
        self.assertEqual(response.json(), {"error": "JSON required", "location": ["body"]})
        self.assertEqual(self.runtime.service.actions, [])

    def test_invalid_management_enum_is_rejected_before_service(self) -> None:
        response = self.client.post(
            "/api/federation",
            content=b'{"action":"invented-action"}',
            headers={"content-type": "application/json"},
        )
        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.json(), {"error": "Invalid request"})
        self.assertEqual(self.runtime.service.actions, [])

    def test_valid_management_action_keeps_legacy_body_keys(self) -> None:
        response = self.client.post(
            "/api/federation",
            content=b'{"action":"status"}',
            headers={"content-type": "application/json"},
        )
        self.assertEqual(response.status_code, 200)
        self.assertEqual(self.runtime.service.actions, [{"action": "status"}])

    def test_create_invite_route_validates_alternate_legacy_response(self) -> None:
        response = self.client.post(
            "/api/federation",
            content=b'{"action":"create_invite"}',
            headers={"content-type": "application/json"},
        )
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["invitation"]["inviteId"], "invite-id")
        self.assertEqual(self.runtime.service.actions, [{"action": "create_invite"}])

    def test_signed_status_route_validates_wrapped_service_response(self) -> None:
        raw = b'{"protocol":1}'
        response = self.post_signed(raw)
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["result"], {"protocol": 1, "approved": True, "revoked": False})
        self.assertEqual(self.runtime.service.calls, [("status", raw)])

    def test_invalid_service_response_still_reports_the_completed_write(self) -> None:
        self.runtime.service.result = {"result": {"approved": True}}
        with self.assertLogs("studio_api.context", level="ERROR") as logs:
            response = self.post_signed(b'{"protocol":1}')
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json(), {"result": {"approved": True}})
        self.assertIn("Response contract mismatch for POST", logs.output[0])
        self.assertEqual(self.runtime.service.writes, 1)

    def test_oversize_body_rejected_before_service(self) -> None:
        response = self.post_signed(b" " * (MAX_BODY_BYTES + 1))
        self.assertEqual(response.status_code, 413)
        self.assertEqual(
            response.json(),
            {"error": "Invalid request size", "location": ["body"]},
        )
        self.assertEqual(self.runtime.service.calls, [])
        self.assertEqual(self.runtime.service.writes, 0)

    def test_raw_path_is_only_the_four_signed_posts(self) -> None:
        routes = {
            (method, path)
            for route in create_router(cast(ApiContext, self.context)).routes
            if hasattr(route, "path")
            for path in (getattr(route, "path"),)
            for method in getattr(route, "methods", set()) or set()
        }
        expected = {
            ("POST", "/api/federation/v1/pair"),
            ("POST", "/api/federation/v1/status"),
            ("POST", "/api/federation/v1/message"),
            ("POST", "/api/federation/v1/pull"),
        }
        self.assertTrue(expected <= routes)
        schema = self.client.get("/openapi.json").json()
        for path in ("/api/federation", *FEDERATION_PATHS):
            self.assertIn("get", schema["paths"][path])
            self.assertIn("404", schema["paths"][path]["get"]["responses"])

    def test_get_counterparts_keep_the_existing_not_found_response(self) -> None:
        for path in ("/api/federation", *FEDERATION_PATHS):
            with self.subTest(path=path):
                response = self.client.get(path)
                self.assertEqual(404, response.status_code)
                self.assertEqual({"error": "Not found"}, response.json())

    def test_openapi_documents_typed_signed_bodies(self) -> None:
        schema = self.client.get("/openapi.json").json()
        management = schema["paths"]["/api/federation"]["post"]["requestBody"]
        self.assertIn("oneOf", management["content"]["application/json"]["schema"])
        self.assertEqual(
            management["content"]["application/json"]["schema"]["discriminator"]["propertyName"],
            "action",
        )
        pair = schema["paths"]["/api/federation/v1/pair"]["post"]["requestBody"]
        self.assertEqual(pair["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/PairRequest")
        pull = schema["paths"]["/api/federation/v1/pull"]["post"]["requestBody"]
        self.assertEqual(pull["content"]["application/json"]["schema"]["$ref"], "#/components/schemas/PullRequest")

        components = schema["components"]
        schemas = components["schemas"]
        responses = components["responses"]
        headers = components["headers"]
        self.assertIn("PairRequest", schemas)
        self.assertIn("SignedEnvelope", schemas)
        self.assertNotIn("$defs", str(schema))
        references: list[str] = []

        def collect_references(value: object) -> None:
            if isinstance(value, dict):
                reference = value.get("$ref")
                if isinstance(reference, str):
                    references.append(reference)
                for child in value.values():
                    collect_references(child)
            elif isinstance(value, list):
                for child in value:
                    collect_references(child)

        collect_references(schema)
        for reference in references:
            if reference.startswith("#/components/schemas/"):
                self.assertIn(reference.removeprefix("#/components/schemas/"), schemas)
            elif reference.startswith("#/components/responses/"):
                self.assertIn(reference.removeprefix("#/components/responses/"), responses)
            else:
                self.assertTrue(reference.startswith("#/components/headers/"), reference)
                self.assertIn(reference.removeprefix("#/components/headers/"), headers)

    def test_management_response_union_accepts_both_legacy_shapes(self) -> None:
        response: TypeAdapter[FederationSnapshot | CreateInviteResponse] = TypeAdapter(
            FederationSnapshot | CreateInviteResponse
        )
        snapshot: dict[str, object] = {
            "version": 1,
            "enabled": False,
            "identity": None,
            "peers": [],
            "invites": [],
            "rooms": [],
            "queued": 0,
        }
        invite = {
            "invitation": {
                "protocol": 1,
                "inviteId": "invite-id",
                "token": "token",
                "stateId": "state-id",
                "label": "Studio",
                "origin": "https://studio.example.ts.net",
                "publicKey": "public-key",
                "expires": 123,
            },
            "expires": 123,
            "warning": "Share privately.",
        }
        self.assertIsInstance(response.validate_python(snapshot), FederationSnapshot)
        self.assertIsInstance(response.validate_python(invite), CreateInviteResponse)
        with self.assertRaises(ValidationError):
            response.validate_python({**snapshot, "enabled": "false"})


if __name__ == "__main__":
    unittest.main()
