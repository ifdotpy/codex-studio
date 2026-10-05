"""Regression tests for core API response behavior."""
from __future__ import annotations

from contextlib import closing

import gzip
import hashlib
import json
import os
import sqlite3
from pathlib import Path
import tempfile
import sys
import asyncio
from types import SimpleNamespace
from types import ModuleType
from typing import cast
import threading
import unittest
from unittest.mock import patch

from fastapi import APIRouter, FastAPI, Request
from fastapi.testclient import TestClient
import httpx
from pydantic import BaseModel, Field, RootModel, TypeAdapter
from starlette.requests import Request
from starlette.responses import Response

from studio_api.context import ApiContext
from studio_api.middleware import HttpTraceMiddleware, RequestBoundary
from studio_api.models import ContractModel, JsonValue, ResponseModel
from studio_api.responses import register_route_components
from studio_api.server import _run_maintenance
from studio_api.schema import (
    API_SCHEMA_HASH_HEADER,
    API_SCHEMA_MISMATCH_HEADER,
    api_schema_hash,
)


class MessageRecord(ContractModel):
    id: str
    text: str


class MessageHistory(RootModel[list[MessageRecord]]):
    pass


class FederationSnapshot(ResponseModel):
    paired: bool


class CreateInviteResponse(ResponseModel):
    invitation: str
    expires: int
    warning: str | None = None


FederationManagementResponse = FederationSnapshot | CreateInviteResponse


class NativeActionRequest(ContractModel):
    request_id: str = Field(min_length=1, max_length=200)


class NativeActionResponse(ResponseModel):
    ok: bool


class NumericResponse(ResponseModel):
    value: float
    integer: int = 0


class WeakTagResponse(ResponseModel):
    name: str
    at: float
    metadata: dict[str, JsonValue] | None = None


class UncheckedLegacyResponse(BaseModel):
    status: str


def request_for(
    response_model: object = None,
    *,
    accept_encoding: str = "",
    if_none_match: str = "",
) -> Request:
    scope = {
        "type": "http",
        "asgi": {"version": "3.0"},
        "http_version": "1.1",
        "method": "GET",
        "scheme": "http",
        "path": "/api/messages",
        "raw_path": b"/api/messages",
        "query_string": b"",
        "root_path": "",
        "headers": [
            (b"accept-encoding", accept_encoding.encode()),
            (b"if-none-match", if_none_match.encode()),
        ],
        "client": ("127.0.0.1", 50000),
        "server": ("127.0.0.1", 46000),
        "route": SimpleNamespace(response_model=response_model),
    }
    return Request(scope)


class CoreResponseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.context = ApiContext.for_schema()

    def test_runtime_property_is_inert_before_runtime_attachment(self) -> None:
        self.assertIsNone(self.context.runtime)

    def test_static_bytes_are_not_treated_as_json(self) -> None:
        response = self.context.send(request_for(), b"image-bytes", content_type="image/png")
        self.assertEqual(response.body, b"image-bytes")
        self.assertEqual(response.headers["content-type"], "image/png")
        self.assert_security_headers(response)

    def test_actual_static_html_route_restores_legacy_security_headers(self) -> None:
        from studio_api.app import create_app

        with tempfile.TemporaryDirectory(prefix="studio-static-header-test-") as directory:
            web_root = Path(directory)
            page = "<!doctype html><html><body><img src='https://example.test/image.png'></body></html>"
            (web_root / "index.html").write_text(page, encoding="utf-8")
            context = ApiContext.for_schema()
            context.remote = SimpleNamespace(
                request_origin=lambda _headers, _peer, _port: "http://testserver",
            )
            app = create_app(context)
            with patch("codex_canvas.WEB", web_root), TestClient(app) as client:
                response = client.get("/")

        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.text, page)
        self.assertEqual(response.headers["content-type"], "text/html; charset=utf-8")
        self.assert_security_headers(response)

    def test_gzipped_validation_error_has_consistent_headers(self) -> None:
        message = "x" * 4096
        response = self.context.send(
            request_for(accept_encoding="gzip"),
            {"error": "Invalid request", "details": [{"message": message}]},
            status=400,
        )
        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.headers.get("content-encoding"), "gzip")
        body = json.loads(gzip.decompress(bytes(response.body)))
        self.assertEqual(body["details"][0]["message"], message)
        self.assertEqual(int(response.headers["content-length"]), len(response.body))
        self.assert_security_headers(response)

    def test_etag_not_modified_keeps_cache_contract_without_security_policy(self) -> None:
        value = {"status": "unchanged"}
        first = self.context.send(request_for(JsonValue), value, etag=True)
        not_modified = self.context.send(
            request_for(JsonValue, if_none_match=first.headers["etag"]),
            value,
            etag=True,
        )

        self.assertEqual(not_modified.status_code, 304)
        self.assertEqual(not_modified.body, b"")
        self.assertEqual(not_modified.headers["etag"], first.headers["etag"])
        self.assertEqual(not_modified.headers["cache-control"], "no-store")
        self.assertEqual(not_modified.headers["content-length"], "0")
        self.assertNotIn("content-security-policy", not_modified.headers)
        self.assertNotIn("referrer-policy", not_modified.headers)
        self.assertEqual(not_modified.headers["x-content-type-options"], "nosniff")

    def test_direct_json_encoding_preserves_validated_shape_and_uses_json_float_spelling(self) -> None:
        response = self.context.send(request_for(NumericResponse), {"value": 1e-5}, etag=True)

        self.assertEqual(response.status_code, 200)
        self.assertEqual(bytes(response.body), b'{"value":0.00001}')
        self.assertEqual(response.headers["etag"], f'"{hashlib.sha256(response.body).hexdigest()}"')

    def test_float_nan_and_infinity_encode_as_null_but_jsonvalue_stays_strict(self) -> None:
        for value in (float("nan"), float("inf"), float("-inf")):
            response = self.context.send(request_for(NumericResponse), {"value": value})
            self.assertEqual(response.status_code, 200)
            self.assertEqual(bytes(response.body), b'{"value":null}')

        rejected = self.context.send(request_for(JsonValue), float("nan"))
        self.assertEqual(rejected.status_code, 500)
        self.assertEqual(json.loads(bytes(rejected.body)), {"error": "The server could not validate its response"})

    def test_direct_json_encoding_accepts_integers_beyond_python_string_limit(self) -> None:
        value = 10 ** 4_300
        response = self.context.send(request_for(NumericResponse), {"value": 0.0, "integer": value})

        self.assertEqual(response.status_code, 200)
        self.assertIn(b"1" + b"0" * 4_300, bytes(response.body))

    def test_weak_etag_still_ignores_configured_fields(self) -> None:
        huge_integer = 10 ** 4_300
        first = self.context.send(
            request_for(WeakTagResponse),
            {
                "name": "runtime",
                "at": 1e-5,
                "metadata": {"z": {"second": 2, "first": 1}, "a": huge_integer, "long": "x" * 2_048},
            },
            etag=True,
            weak_etag_fields=("at",),
        )
        second = self.context.send(
            request_for(WeakTagResponse, accept_encoding="gzip"),
            {
                "name": "runtime",
                "at": 2e-5,
                "metadata": {"long": "x" * 2_048, "a": huge_integer, "z": {"first": 1, "second": 2}},
            },
            etag=True,
            weak_etag_fields=("at",),
        )
        not_modified = self.context.send(
            request_for(WeakTagResponse, accept_encoding="gzip", if_none_match=first.headers["etag"]),
            {
                "name": "runtime",
                "at": 3e-5,
                "metadata": {"z": {"first": 1, "second": 2}, "a": huge_integer, "long": "x" * 2_048},
            },
            etag=True,
            weak_etag_fields=("at",),
        )

        self.assertEqual(first.status_code, 200)
        self.assertEqual(second.status_code, 200)
        self.assertEqual(second.headers["content-encoding"], "gzip")
        self.assertEqual(not_modified.status_code, 304)
        self.assertEqual(not_modified.body, b"")
        self.assertEqual(first.headers["etag"], second.headers["etag"])
        self.assertEqual(first.headers["etag"], not_modified.headers["etag"])
        self.assertTrue(first.headers["etag"].startswith("W/\""))

    def test_typed_root_array_keeps_its_wire_shape(self) -> None:
        request = request_for(MessageHistory)
        response = self.context.send(request, [{"id": "m1", "text": "hello"}])
        self.assertEqual(json.loads(bytes(response.body)), [{"id": "m1", "text": "hello"}])

    def test_actual_jsonvalue_routes_accept_scalar_null_and_root_model(self) -> None:
        app = FastAPI()
        context = ApiContext.for_schema()

        @app.get("/scalar", response_model=JsonValue)
        def scalar(request: Request) -> Response:
            return context.send(request, "provider response")

        @app.get("/null", response_model=JsonValue)
        def null_value(request: Request) -> Response:
            return context.send(request, None)

        @app.get("/messages", response_model=MessageHistory)
        def messages(request: Request) -> Response:
            return context.send(request, MessageHistory([MessageRecord(id="m1", text="hello")]))

        with TestClient(app) as client:
            self.assertEqual(client.get("/scalar").json(), "provider response")
            self.assertIsNone(client.get("/null").json())
            self.assertEqual(client.get("/messages").json(), [{"id": "m1", "text": "hello"}])

    def test_upload_timeout_is_total_not_reset_for_each_chunk(self) -> None:
        context = ApiContext.for_schema()
        context.remote = SimpleNamespace(request_origin=lambda _headers, _peer, _port: "http://local")
        delegated: list[bool] = []

        async def delegate(*_args: object) -> None:
            delegated.append(True)

        boundary = RequestBoundary(delegate, context)
        sent: list[dict[str, object]] = []

        async def exercise() -> None:
            chunks = iter((
                {"type": "http.request", "body": b"{", "more_body": True},
                {"type": "http.request", "body": b"}", "more_body": False},
            ))
            calls = 0

            async def receive() -> dict[str, object]:
                nonlocal calls
                calls += 1
                if calls > 1:
                    await asyncio.sleep(0.04)
                return next(chunks)

            async def send(message: dict[str, object]) -> None:
                sent.append(message)

            scope = {
                "type": "http", "method": "POST", "path": "/api/action",
                "headers": [
                    (b"content-type", b"application/json"), (b"content-length", b"2"),
                    (b"x-canvas-token", context.token.encode()),
                ],
                "client": ("127.0.0.1", 1234), "server": ("127.0.0.1", 4567),
            }
            await boundary(scope, receive, send)  # type: ignore[arg-type]

        with patch("studio_api.middleware.REQUEST_READ_TIMEOUT_SECONDS", 0.02):
            asyncio.run(exercise())
        self.assertEqual(sent[0]["status"], 408)
        self.assertEqual(delegated, [])

    def test_schema_hash_gate_only_rejects_present_mismatches(self) -> None:
        context = ApiContext.for_schema()
        context.api_schema_hash = "server-schema"
        context.remote = SimpleNamespace(request_origin=lambda _headers, _peer, _port: "http://local")
        delegated: list[str] = []

        async def delegate(scope: object, _receive: object, send: object) -> None:
            delegated.append(str(scope["path"]))  # type: ignore[index]
            await send({"type": "http.response.start", "status": 200, "headers": []})  # type: ignore[operator]
            await send({"type": "http.response.body", "body": b"{}"})  # type: ignore[operator]

        boundary = RequestBoundary(delegate, context)

        async def invoke(
            method: str,
            path: str,
            schema_hash: str | None,
            query: bytes = b"",
            include_token: bool = True,
        ) -> list[dict[str, object]]:
            body = b"{}" if method == "POST" else b""
            raw_headers = (
                [(b"x-canvas-token", context.token.encode())] if include_token else []
            )
            if schema_hash is not None:
                raw_headers.append((b"x-studio-api-schema", schema_hash.encode()))
            if body:
                raw_headers.extend(((b"content-type", b"application/json"), (b"content-length", b"2")))
            sent: list[dict[str, object]] = []

            async def receive() -> dict[str, object]:
                return {"type": "http.request", "body": body, "more_body": False}

            async def send(message: dict[str, object]) -> None:
                sent.append(message)

            scope = {
                "type": "http", "method": method, "path": path, "query_string": query,
                "headers": raw_headers, "client": ("127.0.0.1", 1), "server": ("127.0.0.1", 2),
            }
            await boundary(scope, receive, send)  # type: ignore[arg-type]
            return sent

        mismatch_post = asyncio.run(invoke("POST", "/api/messages", "foreign-schema"))
        self.assertEqual(mismatch_post[0]["status"], 426)
        self.assertIn(
            (API_SCHEMA_HASH_HEADER.lower().encode(), b"server-schema"),
            mismatch_post[0]["headers"],
        )
        self.assertIn(
            (API_SCHEMA_MISMATCH_HEADER.lower().encode(), b"1"),
            mismatch_post[0]["headers"],
        )
        self.assertIn(b"Reload", mismatch_post[1]["body"])
        self.assertNotIn("/api/messages", delegated)
        for value in ("", "é"):
            with self.subTest(schema_hash=value):
                response = asyncio.run(invoke("POST", "/api/messages", value))
                self.assertEqual(response[0]["status"], 426)
                self.assertIn(
                    (API_SCHEMA_MISMATCH_HEADER.lower().encode(), b"1"),
                    response[0]["headers"],
                )
        self.assertEqual(
            asyncio.run(
                invoke("POST", "/api/messages", "foreign-schema", include_token=False)
            )[0]["status"],
            403,
        )
        self.assertEqual(asyncio.run(invoke("POST", "/api/messages", None))[0]["status"], 200)
        self.assertEqual(asyncio.run(invoke("POST", "/api/messages", "server-schema"))[0]["status"], 200)
        self.assertEqual(asyncio.run(invoke("GET", "/api/sync/stream", None, b"apiSchema=foreign-schema"))[0]["status"], 200)
        self.assertEqual(asyncio.run(invoke("GET", "/api/sync/stream", None, b"apiSchema="))[0]["status"], 200)
        self.assertEqual(asyncio.run(invoke("GET", "/api/sync/stream", None, "apiSchema=é".encode("utf-8")))[0]["status"], 200)
        self.assertEqual(asyncio.run(invoke("GET", "/api/sync/stream", None))[0]["status"], 200)
        self.assertEqual(asyncio.run(invoke("GET", "/api/sync/stream", None, b"apiSchema=server-schema"))[0]["status"], 200)

    def test_schema_only_context_computes_a_real_hash_lazily(self) -> None:
        context = ApiContext.for_schema()
        self.assertTrue(context.schema_only)
        self.assertEqual(context.api_schema_hash, api_schema_hash())
        self.assertTrue(context.api_schema_hash)

    def test_schema_hash_ignores_documentation_but_tracks_wire_shape(self) -> None:
        document: JsonValue = {
            "paths": {
                "/thing": {
                    "get": {
                        "summary": "First summary",
                        "description": "First description",
                        "externalDocs": {"url": "https://example.test"},
                        "responses": {
                            "200": {
                                "content": {
                                    "application/json": {
                                        "schema": {"type": "string", "title": "Thing"}
                                    }
                                }
                            }
                        },
                    }
                }
            }
        }
        original = api_schema_hash(cast(dict[str, JsonValue], document))
        documented = json.loads(json.dumps(document))
        documented["paths"]["/thing"]["get"]["summary"] = "Changed summary"
        documented["paths"]["/thing"]["get"]["description"] = "Changed description"
        documented["paths"]["/thing"]["get"]["externalDocs"]["url"] = "https://other.test"
        self.assertEqual(original, api_schema_hash(cast(dict[str, JsonValue], documented)))
        documented["paths"]["/thing"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]["type"] = "integer"
        self.assertNotEqual(original, api_schema_hash(cast(dict[str, JsonValue], documented)))

    def test_hourly_maintenance_runs_once_for_shared_listener_context(self) -> None:
        context = ApiContext.for_schema()
        context.canvas.runtime = object()
        maintenance_calls: list[object] = []
        prune_calls: list[Path] = []
        execution = ModuleType("codex_execution")
        execution.maintenance = maintenance_calls.append  # type: ignore[attr-defined]
        voice = ModuleType("codex_voice")
        voice.prune_audio = prune_calls.append  # type: ignore[attr-defined]
        with patch.dict(sys.modules, {"codex_execution": execution, "codex_voice": voice}), patch(
            "studio_api.server.time.monotonic", return_value=3601.0,
        ):
            _run_maintenance(context)
            _run_maintenance(context)
        self.assertEqual(maintenance_calls, [context.canvas.runtime])
        self.assertEqual(prune_calls, [context.canvas.root])

    def test_maintenance_timer_waits_for_runtime_attachment(self) -> None:
        context = ApiContext.for_schema()
        with patch("studio_api.server.time.monotonic", return_value=3601.0):
            _run_maintenance(context)
        self.assertEqual(context._maintenance_last, 0.0)

    def test_http_trace_covers_response_without_recording_request_contents(self) -> None:
        trace_events: list[tuple[object, ...]] = []
        traces = ModuleType("codex_http_traces")

        def begin(method: str, target: str) -> str:
            trace_events.append(("begin", method, target))
            return "trace-1"

        def finish(trace_id: object, status: object, outcome: str) -> None:
            trace_events.append(("finish", trace_id, status, outcome))

        traces.begin = begin  # type: ignore[attr-defined]
        traces.finish = finish  # type: ignore[attr-defined]
        traces.report_failure = lambda _error: None  # type: ignore[attr-defined]
        traces.discard = lambda _trace_id: None  # type: ignore[attr-defined]
        sent: list[dict[str, object]] = []

        async def endpoint(_scope: object, _receive: object, send: object) -> None:
            await send({"type": "http.response.start", "status": 200})  # type: ignore[operator]
            await send({"type": "http.response.body", "body": b"ok"})  # type: ignore[operator]

        async def receive() -> dict[str, object]:
            return {"type": "http.disconnect"}

        async def send(message: dict[str, object]) -> None:
            sent.append(message)

        scope = {
            "type": "http", "method": "GET", "path": "/api/sync/pull",
            "query_string": b"scope=state%3Aentities%3Av1&token=secret",
        }
        with patch.dict(sys.modules, {"codex_http_traces": traces}):
            asyncio.run(HttpTraceMiddleware(endpoint)(scope, receive, send))  # type: ignore[arg-type]
        self.assertEqual(trace_events[0], ("begin", "GET", "/api/sync/pull?scope=state%3Aentities%3Av1&token=secret"))
        self.assertEqual(trace_events[1], ("finish", "trace-1", 200, "complete"))
        self.assertEqual(len(sent), 2)

    def test_response_model_union_validates_both_declared_shapes(self) -> None:
        for value in (
            {"paired": True},
            {"invitation": "signed", "expires": 123},
        ):
            response = self.context.send(request_for(FederationManagementResponse), value)
            self.assertEqual(response.status_code, 200)
            self.assertEqual(json.loads(bytes(response.body)), value)

    def test_cached_union_still_validates_each_response(self) -> None:
        ApiContext._response_adapter.cache_clear()
        self.addCleanup(ApiContext._response_adapter.cache_clear)
        request = request_for(FederationManagementResponse)
        with patch("studio_api.context.TypeAdapter", wraps=TypeAdapter) as factory:
            for value, status in (
                ({"paired": True}, 200),
                ({"invitation": "signed", "expires": 123, "warning": None}, 200),
                ({"paired": "true"}, 500),
                ({"paired": True, "unexpected": 1}, 500),
                ({"paired": False}, 200),
            ):
                response = self.context.send(request, value)
                self.assertEqual(response.status_code, status)
                if status == 200:
                    self.assertEqual(bytes(response.body), json.dumps(value, separators=(",", ":")).encode())
                else:
                    self.assertNotIn("outcome", json.loads(bytes(response.body)))
            self.assertEqual(factory.call_count, 1)

    def test_sync_envelope_does_not_mutate_or_leak_between_responses(self) -> None:
        value = {"paired": True}
        request = request_for(FederationSnapshot)
        request.scope["studio_sync_entities_after"] = 0
        with closing(sqlite3.connect(":memory:")) as db:
            db.execute("CREATE TABLE sync_entities(collection, id, seq, payload, deleted)")
            db.execute("INSERT INTO sync_entities VALUES ('agent', 'a1', 1, '{}', 0)")
            with patch.object(self.context, "sync", return_value=SimpleNamespace(connect=lambda: db)):
                first = self.context.send(request, value)
                db.execute("DELETE FROM sync_entities")
                second = self.context.send(request, value)
        self.assertEqual(first.status_code, 200)
        self.assertEqual(json.loads(bytes(first.body)), {
            "paired": True,
            "_syncEntities": [{"id": "entity:agent:a1", "seq": 1, "payload": "{}", "_deleted": False}],
        })
        self.assertEqual(bytes(second.body), b'{"paired":true}')
        self.assertEqual(value, {"paired": True})

    def test_cached_adapter_preserves_union_branch_order(self) -> None:
        class First(ResponseModel):
            value: str = Field(serialization_alias="first")

        class Second(ResponseModel):
            value: str = Field(serialization_alias="second")

        for model, expected in (
            (First | Second, b'{"first":"same"}'),
            (Second | First, b'{"second":"same"}'),
        ):
            response = self.context.send(request_for(model), {"value": "same"})
            self.assertEqual(response.status_code, 200)
            self.assertEqual(bytes(response.body), expected)

    def test_output_contract_failure_does_not_claim_not_applied(self) -> None:
        response = self.context.send(request_for(MessageHistory), [{"id": 1, "text": "hello"}])
        body = json.loads(bytes(response.body))
        self.assertEqual(response.status_code, 500)
        self.assertNotIn("outcome", body)
        self.assert_security_headers(response)

    def assert_security_headers(self, response: Response) -> None:
        self.assertEqual(response.headers["referrer-policy"], "no-referrer")
        self.assertEqual(
            response.headers["content-security-policy"],
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; "
            "connect-src 'self' https://api.openai.com; "
            "img-src 'self' data: blob: https: http:; "
            "media-src 'self' blob: data:; frame-src 'self' blob:; "
            "frame-ancestors 'none'; base-uri 'none'",
        )

    def test_unchecked_pydantic_model_is_not_a_typed_contract(self) -> None:
        self.assertFalse(ApiContext._response_contract(list[UncheckedLegacyResponse]))
        self.assertTrue(ApiContext._response_contract(list[JsonValue]))

    def test_schema_factory_assembles_all_domain_routers_without_state_io(self) -> None:
        package_names = (
            "agents", "accounts", "federation", "history", "insights", "io",
            "sync", "system", "voice", "work",
        )
        fake_modules: dict[str, ModuleType] = {}
        for domain in package_names:
            package_name = f"studio_api.{domain}"
            package = ModuleType(package_name)
            package.__path__ = []
            router_module = ModuleType(f"{package_name}.router")

            def create_router(_context: object, selected: str = domain) -> APIRouter:
                router = APIRouter()
                if selected == "federation":
                    class RawFederationBody(ContractModel):
                        request_id: str

                    @router.post(
                        "/api/federation/v1/raw",
                        openapi_extra={"requestBody": {"content": {"application/json": {"schema": {"$ref": "#/components/schemas/RawFederationBody"}}}}},
                    )
                    def raw_route() -> None:
                        return None

                    register_route_components(
                        router.routes[-1],
                        {"RawFederationBody": RawFederationBody.model_json_schema()},
                    )
                return router

            router_module.create_router = create_router  # type: ignore[attr-defined]
            fake_modules[package_name] = package
            fake_modules[router_module.__name__] = router_module

        sync_models = ModuleType("studio_api.sync.models")

        class StateSnapshot(ResponseModel):
            token: str

        sync_models.StateSnapshot = StateSnapshot  # type: ignore[attr-defined]
        fake_modules[sync_models.__name__] = sync_models
        schema_package = ModuleType("studio_api.sync")
        schema_package.__path__ = []
        fake_modules[schema_package.__name__] = schema_package

        before = Path(".").resolve()
        with patch.dict(sys.modules, fake_modules):
            from studio_api.app import create_app

            app = create_app(ApiContext.for_schema())
            paths = app.openapi()["paths"]
        self.assertIn("/api/session", paths)
        self.assertIn("/api/state", paths)
        self.assertIn("400", paths["/api/state"]["get"]["responses"])
        self.assertIn("403", paths["/api/state"]["get"]["responses"])
        self.assertIn("409", paths["/api/federation/v1/raw"]["post"]["responses"])
        self.assertIn("RawFederationBody", app.openapi()["components"]["schemas"])
        self.assertNotIn("x-studio-components", paths["/api/federation/v1/raw"]["post"])
        self.assertEqual(Path(".").resolve(), before)

    def test_action_validation_errors_are_not_applied_before_service_call(self) -> None:
        from studio_api.app import create_app

        calls: list[bool] = []
        package_names = (
            "agents", "accounts", "federation", "history", "insights", "io",
            "sync", "system", "voice", "work",
        )
        fake_modules: dict[str, ModuleType] = {}
        for domain in package_names:
            package_name = f"studio_api.{domain}"
            package = ModuleType(package_name)
            package.__path__ = []
            router_module = ModuleType(f"{package_name}.router")

            def create_router(_context: object, selected: str = domain) -> APIRouter:
                router = APIRouter()
                if selected == "agents":
                    @router.post("/api/action", response_model=NativeActionResponse)
                    def native_action(body: NativeActionRequest) -> NativeActionResponse:
                        calls.append(True)
                        return NativeActionResponse(ok=True)

                return router

            router_module.create_router = create_router  # type: ignore[attr-defined]
            fake_modules[package_name] = package
            fake_modules[router_module.__name__] = router_module

        sync_models = ModuleType("studio_api.sync.models")

        class StateSnapshot(ResponseModel):
            token: str

        sync_models.StateSnapshot = StateSnapshot  # type: ignore[attr-defined]
        sync_package = ModuleType("studio_api.sync")
        sync_package.__path__ = []
        fake_modules[sync_package.__name__] = sync_package
        fake_modules[sync_models.__name__] = sync_models

        context = ApiContext.for_schema()
        context.remote = SimpleNamespace(request_origin=lambda _headers, _peer, _port: "http://testserver")
        with patch.dict(sys.modules, fake_modules):
            app = create_app(context)
            action_schema = app.openapi()["paths"]["/api/action"]["post"]["responses"]
            with TestClient(app, raise_server_exceptions=False) as client:
                missing = client.post(
                    "/api/action", json={"id": "lead"},
                    headers={"X-Canvas-Token": context.token},
                )
                extra = client.post(
                    "/api/action", json={"request_id": "request-1", "unexpected": True},
                    headers={"X-Canvas-Token": context.token},
                )
                invalid_type = client.post(
                    "/api/action", json={"request_id": 17},
                    headers={"X-Canvas-Token": context.token},
                )
                empty = client.post(
                    "/api/action", json={"request_id": ""},
                    headers={"X-Canvas-Token": context.token},
                )
        self.assertEqual(missing.status_code, 400)
        self.assertEqual(missing.json()["outcome"], "not_applied")
        self.assertEqual(extra.status_code, 400)
        self.assertEqual(extra.json()["outcome"], "not_applied")
        self.assertEqual(invalid_type.status_code, 400)
        self.assertEqual(invalid_type.json()["outcome"], "not_applied")
        self.assertEqual(empty.status_code, 400)
        self.assertEqual(empty.json()["outcome"], "not_applied")
        self.assertEqual(calls, [])
        self.assertNotIn("422", action_schema)

    def test_prebound_tcp_and_unix_listeners_share_one_typed_app(self) -> None:
        package_names = (
            "agents", "accounts", "federation", "history", "insights", "io",
            "sync", "system", "voice", "work",
        )
        fake_modules: dict[str, ModuleType] = {}
        calls: list[bool] = []
        for domain in package_names:
            package_name = f"studio_api.{domain}"
            package = ModuleType(package_name)
            package.__path__ = []
            router_module = ModuleType(f"{package_name}.router")

            def create_router(_context: object, selected: str = domain) -> APIRouter:
                router = APIRouter()
                if selected == "agents":
                    @router.post("/api/action", response_model=NativeActionResponse)
                    def write_action(body: NativeActionRequest) -> NativeActionResponse:
                        calls.append(True)
                        return NativeActionResponse(ok=True)

                return router

            router_module.create_router = create_router  # type: ignore[attr-defined]
            fake_modules[package_name] = package
            fake_modules[router_module.__name__] = router_module

        sync_package = ModuleType("studio_api.sync")
        sync_package.__path__ = []
        sync_models = ModuleType("studio_api.sync.models")

        class StateSnapshot(ResponseModel):
            threads: list[JsonValue]
            chats: list[JsonValue]
            nodes: list[JsonValue]
            edges: list[JsonValue]
            at: float
            stateDir: str
            runtime: JsonValue | None
            token: str

        sync_models.StateSnapshot = StateSnapshot  # type: ignore[attr-defined]
        fake_modules[sync_package.__name__] = sync_package
        fake_modules[sync_models.__name__] = sync_models

        with tempfile.TemporaryDirectory(prefix="studio-api-core-") as state_dir:
            from codex_canvas import Canvas
            import codex_canvas
            from studio_api.server import make_server

            canvas = Canvas(Path(state_dir))  # type: ignore[no-untyped-call]
            web_root = Path(state_dir) / "web"
            web_root.mkdir()
            (web_root / "index.html").write_bytes(b"<html>fixture</html>")
            with patch.dict(sys.modules, fake_modules):
                server = make_server(canvas, port=0, unix_socket=True)
            assert server.unix_server is not None
            tcp_thread = threading.Thread(target=server.serve_forever, daemon=True)
            unix_thread = threading.Thread(target=server.unix_server.serve_forever, daemon=True)
            tcp_thread.start()
            unix_thread.start()
            socket_path = Path(state_dir) / "canvas.sock"
            try:
                with patch.object(codex_canvas, "WEB", web_root):
                    with httpx.Client(base_url=f"http://127.0.0.1:{server.server_port}", timeout=5) as client:
                        session = client.get("/api/session")
                        state = client.get("/api/state")
                        static = client.get("/")
                        unknown = client.get("/api/unknown")
                        mismatch = client.post(
                            "/api/action", json={"request_id": "req-1"},
                            headers={"X-Canvas-Token": session.json()["token"], "X-Canvas-Workspace": "other"},
                        )
                        tcp_write = client.post(
                            "/api/action", json={"request_id": "tcp-write"},
                            headers={"X-Canvas-Token": session.json()["token"]},
                        )
                        denied_write = client.post("/api/action", json={"request_id": "denied"})
                uds_transport = httpx.HTTPTransport(uds=str(socket_path))
                with httpx.Client(transport=uds_transport, base_url="http://localhost", timeout=5) as unix_client:
                    unix_session = unix_client.get("/api/session")
                    unix_write = unix_client.post(
                        "/api/action", json={"request_id": "unix-write"},
                        headers={"X-Canvas-Token": unix_session.json()["token"]},
                    )
                self.assertEqual(session.status_code, 200)
                self.assertEqual(state.status_code, 200)
                self.assertEqual(static.status_code, 200)
                self.assertEqual(static.content, b"<html>fixture</html>")
                self.assertEqual(unknown.status_code, 404)
                self.assertEqual(unknown.json(), {"error": "Not found"})
                self.assertEqual(mismatch.status_code, 409)
                self.assertEqual(mismatch.json()["error"], "The server workspace changed. Reload before sending.")
                self.assertEqual(tcp_write.status_code, 200, tcp_write.text)
                self.assertEqual(denied_write.status_code, 403)
                self.assertEqual(unix_session.status_code, 200)
                self.assertEqual(unix_session.json()["token"], session.json()["token"])
                self.assertEqual(unix_write.status_code, 200)
                self.assertEqual(calls, [True, True])
                self.assertEqual(os.stat(socket_path).st_mode & 0o777, 0o600)
            finally:
                server.shutdown()
                server.unix_server.shutdown()
                tcp_thread.join(timeout=5)
                unix_thread.join(timeout=5)
                server.server_close()
            self.assertFalse(socket_path.exists())


if __name__ == "__main__":
    unittest.main()
