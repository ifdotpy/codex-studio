"""Check the real FastAPI application's route and OpenAPI boundaries."""
from __future__ import annotations

import inspect
import unittest
from collections.abc import Iterable
from typing import ClassVar, cast

from fastapi import FastAPI
from fastapi.routing import APIRoute
from fastapi.testclient import TestClient

from studio_api.app import create_app
from studio_api.context import ApiContext, HeaderCollection
from studio_api.core_models import SessionResponse


GET_PATHS = frozenset(
    {
        "/api/multi-server",
        "/api/multi-server/audit",
        "/api/multi-server/v1/identity",
        "/api/session",
        "/api/sync/identity",
        "/api/sync/protocol",
        "/api/sync/pull",
        "/api/sync/stream",
        "/api/costs",
        "/api/session-cost",
        "/api/desktop",
        "/api/diagnostics",
        "/api/terminals",
        "/api/terminals/output",
        "/api/directories",
        "/api/tool-requests",
        "/api/analytics",
        "/api/accounts/claude/login",
        "/api/accounts",
        "/api/projects",
        "/api/questions",
        "/api/workspace",
        "/api/workspace/tasks",
        "/api/work",
        "/api/queue",
        "/api/messages/receipts",
        "/api/changes",
        "/api/plan",
        "/api/transcript",
        "/api/transcript/page",
        "/api/transcript/item",
        "/api/transcript/search",
        "/api/search",
        "/api/search/item",
        "/api/checkpoints",
        "/api/capabilities",
        "/api/skills",
        "/api/panel",
        "/api/profiles",
        "/api/rules",
        "/api/monitor/log",
        "/api/file-info",
        "/api/file",
        "/api/limits",
        "/api/task",
        "/api/complaint",
        "/api/agent-chat",
        "/api/models",
        "/api/import",
        "/api/messages",
    }
)

POST_PATHS = frozenset(
    {
        "/api/multi-server",
        "/api/multi-server/v1/pair",
        "/api/multi-server/v1/auto-pair",
        "/api/servers/orchestration",
        "/api/federation/v1/pair",
        "/api/federation/v1/status",
        "/api/federation/v1/message",
        "/api/federation/v1/pull",
        "/api/sync/drafts",
        "/api/voice/status",
        "/api/voice/start",
        "/api/voice/end",
        "/api/voice/record",
        "/api/voice/records",
        "/api/voice/speech",
        "/api/voice/submit",
        "/api/voice/audio",
        "/api/voice/approvals",
        "/api/voice/approval_speech",
        "/api/voice/approve",
        "/api/limits/reset",
        "/api/terminals/create",
        "/api/terminals/input",
        "/api/terminals/resize",
        "/api/terminals/rename",
        "/api/terminals/close",
        "/api/federation",
        "/api/panel/layout",
        "/api/peer-teams",
        "/api/projects",
        "/api/accounts/claude/login",
        "/api/accounts/claude/add",
        "/api/accounts/claude/login/code",
        "/api/accounts/claude/login/cancel",
        "/api/accounts/discover",
        "/api/accounts/register",
        "/api/accounts/default",
        "/api/accounts/login/cancel",
        "/api/accounts/delete",
        "/api/accounts/disconnect",
        "/api/accounts/reconnect",
        "/api/accounts/login",
        "/api/claude/profiles",
        "/api/claude/session",
        "/api/agents/account-transfer",
        "/api/agents/account",
        "/api/work",
        "/api/queue",
        "/api/plan",
        "/api/annotation",
        "/api/organization",
        "/api/assets",
        "/api/branch",
        "/api/checkpoint",
        "/api/checkpoint/preview",
        "/api/checkpoint/restore",
        "/api/tool-requests/cancel",
        "/api/profiles",
        "/api/rules",
        "/api/monitor/input",
        "/api/native-command",
        "/api/messages",
        "/api/rename",
        "/api/room/delete",
        "/api/complaints",
        "/api/conversation/delete",
        "/api/chats",
        "/api/connections",
        "/api/leads",
        "/api/conversation",
        "/api/agents",
        "/api/configure",
        "/api/connection-recovery",
        "/api/context-repair",
        "/api/capacity-retry",
        "/api/usage-resume",
        "/api/action",
        "/api/import",
        "/api/stop",
        "/api/monitor/cancel",
        "/api/questions/delete",
        "/api/questions/defer",
        "/api/answer",
    }
)

STREAM_OR_FILE_ROUTES = frozenset(
    {
        ("GET", "/api/sync/stream"),
        ("GET", "/api/monitor/log"),
    }
)
HIDDEN_COMPATIBILITY_ROUTES = frozenset({("POST", "/api/voice/{action:path}")})


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
        expected = {("GET", path) for path in GET_PATHS}
        expected.update(("POST", path) for path in POST_PATHS)
        self.assertEqual(expected - declared, set())

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
                original_keys = {(method, route.path) for method in methods}
                effective_keys = {(method, path) for method in methods}
                if original_keys & HIDDEN_COMPATIBILITY_ROUTES:
                    self.assertEqual(original_keys, {("POST", "/api/voice/{action:path}")})
                    self.assertFalse(route.include_in_schema)
                elif not any(key in STREAM_OR_FILE_ROUTES for key in effective_keys):
                    self.assertIsNotNone(route.response_model)

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
