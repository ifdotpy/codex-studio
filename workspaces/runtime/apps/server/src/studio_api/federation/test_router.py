"""HTTP boundary tests for the federation FastAPI router."""

from __future__ import annotations

import tempfile
import unittest
from email.message import Message
from pathlib import Path
from typing import TYPE_CHECKING, cast
from unittest.mock import patch

from fastapi import FastAPI
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from httpx import Response
from pydantic import TypeAdapter, ValidationError
from starlette.middleware.base import RequestResponseEndpoint
from starlette.requests import Request
from starlette.responses import Response as StarletteResponse
from studio_api.context import ApiContext
from studio_api.federation.models import CreateInviteResponse, FederationSnapshot
from studio_api.middleware import RequestBoundary
from studio_api.models import ErrorResponse
from studio_api.responses import install_error_response_docs

if TYPE_CHECKING:
    from codex_canvas import Canvas
    from codex_remote import RemoteAccess
from studio_api.federation.router import FEDERATION_PATHS, MAX_BODY_BYTES, create_router


class FakeService:
    def __init__(self) -> None:
        self.calls: list[tuple[str, bytes]] = []
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
        self.calls.append((action, raw))
        if raw.startswith(b"{\n"):
            raise PermissionError("Invalid federation request signature")
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

    def test_invalid_signature_passes_exact_raw_bytes_without_service_write(self) -> None:
        raw = b'{\n  "protocol": 1\n}'
        response = self.post_signed(raw)
        self.assertEqual(response.status_code, 403)
        self.assertEqual(response.json(), {"error": "Invalid federation request signature"})
        self.assertEqual(self.runtime.service.calls, [("status", raw)])
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
        self.assertEqual(response.json(), {"error": "JSON required"})
        self.assertEqual(self.runtime.service.calls, [])

    def test_management_content_type_rejected_before_service(self) -> None:
        response = self.client.post(
            "/api/federation",
            content=b'{"action":"status"}',
            headers={"content-type": "text/plain"},
        )
        self.assertEqual(response.status_code, 415)
        self.assertEqual(response.json(), {"error": "JSON required"})
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

    def test_invalid_service_response_does_not_mark_completed_write_as_retryable(self) -> None:
        self.runtime.service.result = {"result": {"approved": True}}
        response = self.post_signed(b'{"protocol":1}')
        self.assertEqual(response.status_code, 500)
        self.assertNotIn("outcome", response.json())
        self.assertEqual(self.runtime.service.writes, 1)

    def test_oversize_body_rejected_before_service(self) -> None:
        response = self.post_signed(b" " * (MAX_BODY_BYTES + 1))
        self.assertEqual(response.status_code, 413)
        self.assertEqual(response.json(), {"error": "Invalid request size"})
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
        self.assertTrue(all("get" not in schema["paths"][path] for path in ("/api/federation", *FEDERATION_PATHS)))

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

        components = schema["components"]["schemas"]
        self.assertIn("PairRequest", components)
        self.assertIn("SignedEnvelope", components)
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
            self.assertTrue(reference.startswith("#/components/schemas/"), reference)
            self.assertIn(reference.removeprefix("#/components/schemas/"), components)

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
