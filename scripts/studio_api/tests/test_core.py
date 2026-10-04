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
from pydantic import RootModel
from starlette.requests import Request

from studio_api.context import ApiContext
from studio_api.models import ContractModel, ResponseModel


class MessageRecord(ContractModel):
    id: str
    text: str


class MessageHistory(RootModel[list[MessageRecord]]):
    pass


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
        body = json.loads(gzip.decompress(response.body))
        self.assertEqual(body["details"][0]["message"], message)
        self.assertEqual(int(response.headers["content-length"]), len(response.body))

    def test_typed_root_array_keeps_its_wire_shape(self) -> None:
        request = request_for(MessageHistory)
        response = self.context.send(request, [{"id": "m1", "text": "hello"}])
        self.assertEqual(json.loads(response.body), [{"id": "m1", "text": "hello"}])

    def test_output_contract_failure_does_not_claim_not_applied(self) -> None:
        response = self.context.send(request_for(MessageHistory), [{"id": 1, "text": "hello"}])
        body = json.loads(response.body)
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
            package.__path__ = []  # type: ignore[attr-defined]
            router_module = ModuleType(f"{package_name}.router")
            router_module.create_router = lambda _context: APIRouter()  # type: ignore[attr-defined]
            fake_modules[package_name] = package
            fake_modules[router_module.__name__] = router_module

        sync_models = ModuleType("studio_api.sync.models")

        class StateSnapshot(ResponseModel):
            token: str

        sync_models.StateSnapshot = StateSnapshot  # type: ignore[attr-defined]
        fake_modules[sync_models.__name__] = sync_models
        schema_package = ModuleType("studio_api.sync")
        schema_package.__path__ = []  # type: ignore[attr-defined]
        fake_modules[schema_package.__name__] = schema_package

        before = Path(".").resolve()
        with patch.dict(sys.modules, fake_modules):
            from studio_api.app import create_app

            app = create_app(ApiContext.for_schema())
            paths = app.openapi()["paths"]
        self.assertIn("/api/session", paths)
        self.assertIn("/api/state", paths)
        self.assertEqual(Path(".").resolve(), before)


if __name__ == "__main__":
    unittest.main()
