"""Check the real FastAPI application's route and OpenAPI boundaries."""
from __future__ import annotations

import unittest
from typing import ClassVar, cast

from fastapi import FastAPI
from fastapi.routing import APIRoute
from fastapi.testclient import TestClient

from studio_api.app import create_app
from studio_api.context import ApiContext


GET_PATHS = frozenset(
    {
        "/api/session",
        "/api/sync/identity",
        "/api/sync/protocol",
        "/api/sync/pull",
        "/api/sync/generations",
        "/api/sync/stream",
        "/api/state",
        "/api/worktree-disk",
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
        "/api/transcript/stream",
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
        ("GET", "/api/transcript/stream"),
        ("GET", "/api/monitor/log"),
    }
)
HIDDEN_COMPATIBILITY_ROUTES = frozenset({("POST", "/api/voice/{action:path}")})


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
        for route in self.app.routes:
            if not isinstance(route, APIRoute) or not route.path.startswith("/api/"):
                continue
            declared.update((method, route.path) for method in route.methods or ())
        expected = {("GET", path) for path in GET_PATHS}
        expected.update(("POST", path) for path in POST_PATHS)
        self.assertEqual(expected - declared, set())

    def test_api_routes_are_explicit_component_routes_with_response_models(self) -> None:
        for route in self.app.routes:
            if not isinstance(route, APIRoute) or not route.path.startswith("/api/"):
                continue
            with self.subTest(path=route.path):
                self.assertTrue(
                    route.endpoint.__module__.startswith("studio_api."),
                    "API endpoint must be owned by a concrete studio_api module",
                )
                self.assertNotIn("{path:path}", route.path)
                methods = route.methods or set()
                self.assertTrue(methods.issubset({"GET", "POST", "HEAD", "OPTIONS"}))
                route_keys = {(method, route.path) for method in methods}
                if route_keys & HIDDEN_COMPATIBILITY_ROUTES:
                    self.assertEqual(route_keys, {("POST", "/api/voice/{action:path}")})
                    self.assertFalse(route.include_in_schema)
                elif not any(key in STREAM_OR_FILE_ROUTES for key in route_keys):
                    self.assertIsNotNone(route.response_model)

    def test_openapi_exposes_existing_query_parameters(self) -> None:
        document = cast(dict[str, object], self.app.openapi())
        query_names = {
            path: query_parameter_names(document, path)
            for path in ("/api/state", "/api/sync/pull", "/api/agent-chat")
        }
        self.assertIn("view", query_names["/api/state"])
        self.assertTrue({"scope", "after", "limit"}.issubset(query_names["/api/sync/pull"]))
        self.assertTrue({"room", "before", "after", "limit"}.issubset(query_names["/api/agent-chat"]))

    def test_application_has_no_generic_api_catchall(self) -> None:
        dynamic_routes: set[tuple[str, str]] = set()
        for route in self.app.routes:
            path = getattr(route, "path", "")
            if path.startswith("/api/"):
                methods = getattr(route, "methods", None) or set()
                if "{" in path:
                    dynamic_routes.update((method, path) for method in methods)
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
        with TestClient(self.app) as client:
            response = client.get("/api/migration-contract-unknown")
        self.assertEqual(response.status_code, 404)
        self.assertEqual(response.json(), {"error": "Not found"})


if __name__ == "__main__":
    unittest.main(verbosity=2)
