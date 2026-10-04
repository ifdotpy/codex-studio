"""FastAPI-level tests for agent route dispatch and generated query contracts."""

from __future__ import annotations

from contextlib import contextmanager
import sqlite3
import threading
from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from starlette.middleware.base import RequestResponseEndpoint
from starlette.responses import Response
from types import SimpleNamespace
from typing import Iterator
from unittest.mock import patch
import unittest

from studio_api.context import ApiContext
from studio_api.models import ErrorResponse
from .router import create_router


class _RuntimeFixture:
    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple[object, ...]]] = []
        self.invalid_stop_response = False
        self.lock = threading.RLock()

    def stop(self, agent_id: str, descendants: bool) -> dict[str, list[str]]:
        self.calls.append(("stop", (agent_id, descendants)))
        if self.invalid_stop_response:
            return {"stopped": [agent_id], "private": ["must not leak"]}
        return {"stopped": [agent_id]}

    def native_action(self, *args: object) -> dict[str, object]:
        self.calls.append(("native_action", args))
        return {}

    def create(self, data: dict[str, object], parent: str | None = None) -> dict[str, object]:
        self.calls.append(("create", (data, parent)))
        return _agent_record()

    def set_account(
        self,
        agent_id: str,
        account_key: str,
        cwd: str | None = None,
    ) -> dict[str, object]:
        self.calls.append(("set_account", (agent_id, account_key, cwd)))
        return _agent_record()

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(":memory:")
        try:
            yield connection
        finally:
            connection.close()

    def agent(self, _agent_id: str, _db: sqlite3.Connection) -> dict[str, object]:
        return _agent_record()

    def empty_lead(self, _db: sqlite3.Connection, _agent: dict[str, object]) -> bool:
        return False

    def capabilities(self, agent_id: str | None) -> dict[str, object]:
        self.calls.append(("capabilities", (agent_id,)))
        return {
            "agent": agent_id or "",
            "at": 1.0,
            "managed": [],
            "observed": [],
            "skills": [],
            "servers": [],
            "errors": [],
            "model": "open-catalog-model",
            "effort": None,
            "role": "orchestrator",
            "nativeInventory": "fixture",
            "observedNative": [],
            "mcp": [],
        }

    def skill_catalog(self, agent_id: str | None) -> dict[str, object]:
        self.calls.append(("skills", (agent_id,)))
        return {"skills": [], "errors": []}

    def usage_resume_action(
        self,
        agent_id: str,
        resume_id: str,
        enabled: bool,
    ) -> dict[str, object]:
        self.calls.append(("usage_resume", (agent_id, resume_id, enabled)))
        return {
            "id": resume_id,
            "status": "scheduled",
            "accountKey": "default",
            "threadId": "thread",
            "epoch": 2,
            "turnId": "turn",
            "cause": "usage_limit",
            "failedAt": 1.0,
            "dueAt": 2.0,
        }


def _agent_record() -> dict[str, object]:
    return {
        "id": "agent-created",
        "status": "idle",
        "source": "managed",
        "kind": "agent",
        "name": "Worker",
        "isLead": False,
    }


class _SyncDbFixture:
    def execute(self, _query: str, _parameters: tuple[int]) -> _SyncCursorFixture:
        return _SyncCursorFixture()


class _SyncCursorFixture:
    def fetchall(self) -> list[tuple[str, str, int, str, int]]:
        return [("agent", "agent-created", 42, '{"id":"agent-created"}', 0)]


class _SyncFixture:
    @contextmanager
    def connect(self) -> Iterator[_SyncDbFixture]:
        yield _SyncDbFixture()


def _app(*, include_sync_envelope: bool = False) -> tuple[FastAPI, _RuntimeFixture, ApiContext]:
    runtime = _RuntimeFixture()
    context = ApiContext.for_schema()
    setattr(context.canvas, "runtime", runtime)
    if include_sync_envelope:
        setattr(context, "sync", lambda: _SyncFixture())
    app = FastAPI()
    app.include_router(create_router(context))
    if include_sync_envelope:
        @app.middleware("http")
        async def add_sync_cursor(request: Request, call_next: RequestResponseEndpoint) -> Response:
            request.scope["studio_sync_entities_after"] = 0
            return await call_next(request)

    @app.exception_handler(RequestValidationError)
    async def legacy_validation_error(request: Request, _error: RequestValidationError) -> Response:
        return context.send(
            request,
            ErrorResponse(
                error="Invalid request",
                outcome=(
                    "not_applied"
                    if request.method == "POST" and request.url.path == "/api/action"
                    else None
                ),
            ),
            status=400,
        )

    return app, runtime, context


def test_durable_action_rejection_happens_before_runtime_call() -> None:
    app, runtime, _context = _app()
    response = TestClient(app).post("/api/action", json={"id": "agent", "action": "review"})

    assert response.status_code == 400
    assert response.json()["outcome"] == "not_applied"
    assert runtime.calls == []


def test_stop_preserves_explicit_false_descendant_flag() -> None:
    app, runtime, _context = _app()
    response = TestClient(app).post("/api/stop", json={"id": "agent", "descendants": False})

    assert response.status_code == 200
    assert response.json() == {"stopped": ["agent"]}
    assert runtime.calls == [("stop", ("agent", False))]


def test_capabilities_query_is_in_openapi_and_uses_legacy_first_value() -> None:
    app, runtime, _context = _app()
    parameters = app.openapi()["paths"]["/api/capabilities"]["get"]["parameters"]
    response = TestClient(app).get("/api/capabilities?agent=first&agent=second")

    assert any(parameter["name"] == "agent" and parameter["in"] == "query" for parameter in parameters)
    assert response.status_code == 200
    assert runtime.calls == [("capabilities", ("first",))]


def test_invalid_native_command_enum_returns_400_before_side_effect() -> None:
    app, runtime, _context = _app()
    response = TestClient(app).post("/api/native-command", json={"id": "agent", "action": "restart"})

    assert response.status_code == 400
    assert runtime.calls == []


def test_usage_resume_returns_the_receipt_shape_not_an_agent_projection() -> None:
    app, runtime, _context = _app()
    response = TestClient(app).post(
        "/api/usage-resume",
        json={"id": "agent", "resume_id": "resume", "enabled": True},
    )

    assert response.status_code == 200
    assert response.json()["id"] == "resume"
    assert response.json()["status"] == "scheduled"
    assert runtime.calls == [("usage_resume", ("agent", "resume", True))]


def test_context_sender_rejects_invalid_service_response_without_retry_hint() -> None:
    app, runtime, _context = _app()
    runtime.invalid_stop_response = True
    response = TestClient(app, raise_server_exceptions=False).post(
        "/api/stop", json={"id": "agent"}
    )

    assert response.status_code == 500
    assert "outcome" not in response.json()


def test_transfer_response_preserves_receipt_identity_and_filters_internal_state() -> None:
    operation = {
        "id": "durable-operation-id",
        "leadId": "lead",
        "targetAccountKey": "destination",
        "status": "pending",
        "created": 10.0,
        "updated": 11.0,
        "scope": "team",
        "members": {
            "worker": {
                "phase": "blocked",
                "name": "worker name",
                "provider": "codex",
                "error": "Native history needs attention",
                "lazy": True,
                "archiveInvalidated": False,
                "nativeParams": {"privateNativeMarker": "secret"},
                "targetConnection": "private-connection",
                "archiveSourceThread": {"privateThreadMarker": "secret"},
                "portableHistory": {"archivePath": "/private/archive.json"},
                "sourceHistoryMissing": {"privateMissingMarker": "secret"},
                "emptyThreadRecovery": {"privateRecoveryMarker": "secret"},
                "sourceClaudeOptions": {"privateOptionMarker": "secret"},
                "result": {"thread": {"id": "private-thread"}},
                "source": {"cwd": "/private/workspace"},
            }
        },
        "requests": {
            "durable-operation-id": {
                "leadId": "lead",
                "targetAccountKey": "destination",
                "scope": "team",
            }
        },
        "portableHistory": {"privateOperationMarker": "secret"},
    }

    class FakeTransferStore:
        def request(
            self,
            _key: str | None,
            _account_key: str | None,
            _request_id: str,
            _scope: str,
        ) -> dict[str, object]:
            return operation

        def action(self, _request_id: str, _action: str) -> dict[str, object]:
            return operation

    api_module = SimpleNamespace(
        transfer_store=lambda _runtime: FakeTransferStore(),
    )
    app, _runtime, _context = _app()
    with patch.dict("sys.modules", {"codex_account_transfer": api_module}):
        response = TestClient(app).post(
            "/api/agents/account-transfer",
            json={"id": "lead", "account_key": "destination", "request_id": "durable-operation-id"},
        )

    assert response.status_code == 200
    payload = response.json()
    assert payload["id"] == "durable-operation-id"
    assert payload["canRetry"] is True
    assert payload["blocked"] == [
        {"id": "worker", "name": "worker name", "provider": "codex", "reason": "Native history needs attention"}
    ]
    serialized = response.text
    for private_marker in (
        "privateNativeMarker",
        "private-connection",
        "privateThreadMarker",
        "/private/archive.json",
        "privateMissingMarker",
        "privateRecoveryMarker",
        "privateOptionMarker",
        "private-thread",
        "/private/workspace",
        "privateOperationMarker",
    ):
        assert private_marker not in serialized


def test_agent_create_and_select_accept_the_shared_sync_envelope() -> None:
    app, runtime, _context = _app(include_sync_envelope=True)
    client = TestClient(app)

    created = client.post(
        "/api/agents",
        json={"id": "9750de4d-a148-49a7-a15c-14887ab55bdc", "prompt": "Do work", "parent": "lead"},
    )
    selected = client.post(
        "/api/agents/account",
        json={"id": "agent-created", "account_key": "account-2"},
    )

    for response in (created, selected):
        assert response.status_code == 200, response.text
        assert response.json()["id"] == "agent-created"
        assert response.json()["_syncEntities"] == [
            {"id": "entity:agent:agent-created", "seq": 42, "payload": '{"id":"agent-created"}', "_deleted": False}
        ]
    assert runtime.calls[0][0] == "create"
    assert runtime.calls[1] == ("set_account", ("agent-created", "account-2", None))


def test_agent_routes_register_response_dto_with_sync_envelope() -> None:
    app, _runtime, _context = _app()
    schema = app.openapi()
    response_name = schema["paths"]["/api/agents"]["post"]["responses"]["200"]["content"]["application/json"]["schema"]["$ref"].rsplit("/", 1)[-1]

    assert response_name == "AgentResponse"
    assert "_syncEntities" in schema["components"]["schemas"][response_name]["properties"]


class AgentRouterTests(unittest.TestCase):
    def test_durable_action_rejection_happens_before_runtime_call(self) -> None:
        test_durable_action_rejection_happens_before_runtime_call()

    def test_stop_preserves_explicit_false_descendant_flag(self) -> None:
        test_stop_preserves_explicit_false_descendant_flag()

    def test_capabilities_query_is_in_openapi_and_uses_legacy_first_value(self) -> None:
        test_capabilities_query_is_in_openapi_and_uses_legacy_first_value()

    def test_invalid_native_command_enum_returns_400_before_side_effect(self) -> None:
        test_invalid_native_command_enum_returns_400_before_side_effect()

    def test_usage_resume_returns_receipt_shape(self) -> None:
        test_usage_resume_returns_the_receipt_shape_not_an_agent_projection()

    def test_context_sender_rejects_invalid_response_without_retry_hint(self) -> None:
        test_context_sender_rejects_invalid_service_response_without_retry_hint()

    def test_transfer_projection_preserves_identity_and_filters_internal_state(self) -> None:
        test_transfer_response_preserves_receipt_identity_and_filters_internal_state()

    def test_agent_create_and_select_accept_sync_envelope(self) -> None:
        test_agent_create_and_select_accept_the_shared_sync_envelope()

    def test_agent_route_schema_includes_sync_envelope(self) -> None:
        test_agent_routes_register_response_dto_with_sync_envelope()
