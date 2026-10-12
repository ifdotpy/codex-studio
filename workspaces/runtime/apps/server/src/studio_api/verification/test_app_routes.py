"""Check the real FastAPI application's route and OpenAPI boundaries."""
from __future__ import annotations

import inspect
import json
from pathlib import Path
import unittest
from collections.abc import Iterable
from typing import ClassVar, cast

from fastapi import FastAPI
from fastapi.routing import APIRoute
from fastapi.testclient import TestClient

from studio_api.app import create_app
from studio_api.context import ApiContext, HeaderCollection
from studio_api.core_models import SessionResponse


OPENAPI_DOCUMENT = Path(__file__).resolve().parents[6] / "runtime/packages/runtime-http/openapi.json"
HTTP_METHODS = frozenset({"get", "put", "post", "delete", "options", "head", "patch", "trace"})


def authoritative_api_operations() -> set[tuple[str, str]]:
    document = json.loads(OPENAPI_DOCUMENT.read_text(encoding="utf-8"))
    return {
        (method.upper(), path)
        for path, item in document["paths"].items()
        if path.startswith("/api/")
        for method in item
        if method in HTTP_METHODS
    }


STREAM_OR_FILE_ROUTES = frozenset(
    {
        ("GET", "/api/sync/stream"),
        ("GET", "/api/monitor/log"),
    }
)
HIDDEN_COMPATIBILITY_ROUTES = frozenset({("POST", "/api/voice/{action:path}")})


def effective_route_path(path: str) -> str:
    """Match FastAPI's path_format, which omits converter suffixes."""
    return path.replace("{action:path}", "{action}")


def route_inventory_difference(
    declared: set[tuple[str, str]], expected: set[tuple[str, str]]
) -> tuple[set[tuple[str, str]], set[tuple[str, str]]]:
    """Return missing schema routes and undeclared concrete API routes."""
    return expected - declared, declared - expected


class VerifierRemote:
    """Local-only origin policy for disposable TestClient requests."""

    def origin(self) -> str | None:
        return "http://testserver"

    def request_origin(self, headers: HeaderCollection, peer: str, port: int) -> str | None:
        return "http://testserver"


def application_api_routes(app: FastAPI) -> tuple[tuple[str, APIRoute], ...]:
    """Expand FastAPI's lazy included routers into effective API route paths."""
    expanded: list[tuple[str, APIRoute]] = []
    for entry in app.routes:
        if isinstance(entry, APIRoute):
            expanded.append((entry.path, entry))
            continue
        get_contexts = getattr(entry, "effective_route_contexts", None)
        if not callable(get_contexts):
            continue
        for context in cast(Iterable[object], get_contexts()):
            route = getattr(context, "original_route", None)
            path = getattr(context, "path_format", None)
            if isinstance(route, APIRoute) and isinstance(path, str):
                expanded.append((path, route))
    return tuple(expanded)


def query_parameter_names(document: dict[str, object], path: str) -> set[str]:
    """Read query names from generated OpenAPI's deliberately open schema."""
    paths = cast(dict[str, object], document["paths"])
    operations = cast(dict[str, object], paths[path])
    get_operation = cast(dict[str, object], operations["get"])
    parameters = cast(list[dict[str, str]], get_operation.get("parameters", []))
    return {parameter["name"] for parameter in parameters if parameter["in"] == "query"}


class ApplicationRouteContract(unittest.TestCase):
    context: ClassVar[ApiContext]
    app: ClassVar[FastAPI]

    @classmethod
    def setUpClass(cls) -> None:
        cls.context = ApiContext.for_schema()
        cls.app = create_app(cls.context)

    def test_all_legacy_api_method_paths_have_concrete_routes(self) -> None:
        declared: set[tuple[str, str]] = set()
        for path, route in application_api_routes(self.app):
            if not path.startswith("/api/"):
                continue
            declared.update((method, path) for method in route.methods or ())
        expected = authoritative_api_operations() | {
            (method, effective_route_path(path))
            for method, path in HIDDEN_COMPATIBILITY_ROUTES
        }
        missing, extra = route_inventory_difference(declared, expected)
        self.assertEqual((missing, extra), (set(), set()))

    def test_route_without_schema_entry_is_rejected_by_inventory(self) -> None:
        expected = authoritative_api_operations() | {
            (method, effective_route_path(path))
            for method, path in HIDDEN_COMPATIBILITY_ROUTES
        }
        declared = expected | {("GET", "/api/added-without-schema")}
        missing, extra = route_inventory_difference(declared, expected)
        self.assertEqual(missing, set())
        self.assertEqual(extra, {("GET", "/api/added-without-schema")})

    def test_api_routes_are_explicit_component_routes_with_response_models(self) -> None:
        for path, route in application_api_routes(self.app):
            if not path.startswith("/api/"):
                continue
            with self.subTest(path=path):
                self.assertTrue(
                    route.endpoint.__module__.startswith("studio_api."),
                    "API endpoint must be owned by a concrete studio_api module",
                )
                self.assertNotIn("{path:path}", route.path)
                methods = route.methods or set()
                self.assertTrue(methods.issubset({"GET", "POST", "HEAD", "OPTIONS"}))
                effective_keys = {(method, path) for method in methods}
                if route.path == "/api/voice/{action:path}":
                    self.assertEqual(methods, {"POST"})
                    self.assertFalse(route.include_in_schema)
                if not any(key in STREAM_OR_FILE_ROUTES for key in effective_keys):
                    self.assertIsNotNone(route.response_model)
                    self.assertTrue(
                        ApiContext._response_contract(route.response_model),
                        "JSON API route response model must be an ApiContext contract",
                    )

    def test_openapi_exposes_existing_query_parameters(self) -> None:
        document = cast(dict[str, object], self.app.openapi())
        query_names = {
            path: query_parameter_names(document, path)
            for path in ("/api/sync/pull", "/api/agent-chat")
        }
        self.assertTrue({"scope", "after", "limit"}.issubset(query_names["/api/sync/pull"]))
        self.assertTrue({"room", "before", "after", "limit"}.issubset(query_names["/api/agent-chat"]))

    def test_session_keeps_typed_wire_response_without_threadpool_dispatch(self) -> None:
        context = ApiContext(
            self.context.canvas,
            token="session-contract-token",
            remote=VerifierRemote(),
            schema_only=True,
        )
        app = create_app(context)
        route = next(
            route
            for path, route in application_api_routes(app)
            if path == "/api/session"
        )

        self.assertTrue(inspect.iscoroutinefunction(route.endpoint))
        self.assertIs(route.response_model, SessionResponse)
        session_paths = cast(dict[str, object], app.openapi()["paths"])
        session_path = cast(dict[str, object], session_paths["/api/session"])
        get_session = cast(dict[str, object], session_path["get"])
        responses = cast(dict[str, object], get_session["responses"])
        self.assertTrue({"200", "400", "401", "403", "429", "500", "503"}.issubset(responses))
        success = cast(dict[str, object], responses["200"])
        content = cast(dict[str, object], success["content"])
        media_type = cast(dict[str, object], content["application/json"])
        self.assertEqual(
            media_type["schema"],
            {"$ref": "#/components/schemas/SessionResponse"},
        )

        with TestClient(app) as client:
            response = client.get("/api/session")

        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.content, b'{"token":"session-contract-token"}')
        self.assertEqual(response.headers["content-type"], "application/json")
        self.assertEqual(response.headers["cache-control"], "no-store")
        self.assertEqual(response.headers["x-content-type-options"], "nosniff")

    def test_application_has_no_generic_api_catchall(self) -> None:
        dynamic_routes: set[tuple[str, str]] = set()
        for path, route in application_api_routes(self.app):
            if path.startswith("/api/"):
                methods = getattr(route, "methods", None) or set()
                if "{" in path or "{" in route.path:
                    dynamic_routes.update((method, route.path) for method in methods)
        self.assertEqual(dynamic_routes, HIDDEN_COMPATIBILITY_ROUTES)

    def test_openapi_has_no_dynamic_api_contracts(self) -> None:
        document = cast(dict[str, object], self.app.openapi())
        paths = cast(dict[str, object], document["paths"])
        dynamic_paths = {
            path for path in paths
            if path.startswith("/api/") and "{" in path
        }
        self.assertEqual(dynamic_paths, set())

    def test_unknown_api_route_keeps_legacy_not_found_response(self) -> None:
        schema_context = ApiContext.for_schema()
        http_context = ApiContext(
            schema_context.canvas,
            token=schema_context.token,
            remote=VerifierRemote(),
            schema_only=True,
        )
        http_app = create_app(http_context)
        with TestClient(http_app) as client:
            for path in (
                "/api/migration-contract-unknown",
                "/api/state",
                "/api/sync/generations",
                "/api/transcript/stream",
            ):
                with self.subTest(path=path):
                    response = client.get(path)
                    self.assertEqual(response.status_code, 404)
                    self.assertEqual(response.json(), {"error": "Not found"})


if __name__ == "__main__":
    unittest.main(verbosity=2)
