"""Regression tests for core API response behavior."""
from __future__ import annotations

import gzip
import json
from pathlib import Path
import sys
from types import SimpleNamespace
from types import ModuleType
import unittest
from unittest.mock import patch

from fastapi import APIRouter
from fastapi.testclient import TestClient
from pydantic import Field, RootModel
from starlette.requests import Request

from studio_api.context import ApiContext
from studio_api.models import ContractModel, ResponseModel
from studio_api.responses import register_route_components


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


def request_for(response_model: object = None, *, accept_encoding: str = "") -> Request:
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
        "headers": [(b"accept-encoding", accept_encoding.encode())],
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

    def test_typed_root_array_keeps_its_wire_shape(self) -> None:
        request = request_for(MessageHistory)
        response = self.context.send(request, [{"id": "m1", "text": "hello"}])
        self.assertEqual(json.loads(bytes(response.body)), [{"id": "m1", "text": "hello"}])

    def test_response_model_union_validates_both_declared_shapes(self) -> None:
        for value in (
            {"paired": True},
            {"invitation": "signed", "expires": 123},
        ):
            response = self.context.send(request_for(FederationManagementResponse), value)
            self.assertEqual(response.status_code, 200)
            self.assertEqual(json.loads(bytes(response.body)), value)

    def test_output_contract_failure_does_not_claim_not_applied(self) -> None:
        response = self.context.send(request_for(MessageHistory), [{"id": 1, "text": "hello"}])
        body = json.loads(bytes(response.body))
        self.assertEqual(response.status_code, 500)
        self.assertNotIn("outcome", body)

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
                    class NativeActionRequest(ContractModel):
                        request_id: str = Field(min_length=1, max_length=200)

                    class NativeActionResponse(ResponseModel):
                        ok: bool

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


if __name__ == "__main__":
    unittest.main()
