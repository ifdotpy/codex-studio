"""FastAPI-level tests for agent route dispatch and generated query contracts."""

from typing import cast

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from pydantic import TypeAdapter
from starlette.responses import JSONResponse, Response

from studio_api.context import ApiContext
from studio_api.models import ErrorResponse, ResponseModel
from .router import create_router


class _RuntimeFixture:
    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple[object, ...]]] = []

    def stop(self, agent_id: str, descendants: bool) -> dict[str, list[str]]:
        self.calls.append(("stop", (agent_id, descendants)))
        return {"stopped": [agent_id]}

    def native_action(self, *args: object) -> dict[str, object]:
        self.calls.append(("native_action", args))
        return {}

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


class _ContextFixture:
    def __init__(self, runtime: _RuntimeFixture) -> None:
        self.runtime = runtime

    def send(
        self,
        request: Request,
        value: object,
        status: int = 200,
        **_options: object,
    ) -> Response:
        route = request.scope.get("route")
        model = getattr(route, "response_model", None)
        if status < 400 and model is not None:
            validated = TypeAdapter(model).validate_python(value)
            if isinstance(validated, ResponseModel):
                value = validated.model_dump(mode="json", by_alias=True, exclude_unset=True)
            else:
                value = validated
        return JSONResponse(value, status_code=status)


def _app() -> tuple[FastAPI, _RuntimeFixture]:
    runtime = _RuntimeFixture()
    context = cast(ApiContext, _ContextFixture(runtime))
    app = FastAPI()
    app.include_router(create_router(context))

    @app.exception_handler(RequestValidationError)
    async def legacy_validation_error(_request: Request, _error: RequestValidationError) -> JSONResponse:
        return JSONResponse(
            ErrorResponse(error="Invalid request").model_dump(mode="json", exclude_unset=True),
            status_code=400,
        )

    return app, runtime


def test_durable_action_rejection_happens_before_runtime_call() -> None:
    app, runtime = _app()
    response = TestClient(app).post("/api/action", json={"id": "agent", "action": "review"})

    assert response.status_code == 400
    assert response.json()["outcome"] == "not_applied"
    assert runtime.calls == []


def test_stop_preserves_explicit_false_descendant_flag() -> None:
    app, runtime = _app()
    response = TestClient(app).post("/api/stop", json={"id": "agent", "descendants": False})

    assert response.status_code == 200
    assert response.json() == {"stopped": ["agent"]}
    assert runtime.calls == [("stop", ("agent", False))]


def test_capabilities_query_is_in_openapi_and_uses_legacy_first_value() -> None:
    app, runtime = _app()
    parameters = app.openapi()["paths"]["/api/capabilities"]["get"]["parameters"]
    response = TestClient(app).get("/api/capabilities?agent=first&agent=second")

    assert any(parameter["name"] == "agent" and parameter["in"] == "query" for parameter in parameters)
    assert response.status_code == 200
    assert runtime.calls == [("capabilities", ("first",))]


def test_invalid_native_command_enum_returns_400_before_side_effect() -> None:
    app, runtime = _app()
    response = TestClient(app).post("/api/native-command", json={"id": "agent", "action": "restart"})

    assert response.status_code == 400
    assert runtime.calls == []


def test_usage_resume_returns_the_receipt_shape_not_an_agent_projection() -> None:
    app, runtime = _app()
    response = TestClient(app).post(
        "/api/usage-resume",
        json={"id": "agent", "resume_id": "resume", "enabled": True},
    )

    assert response.status_code == 200
    assert response.json()["id"] == "resume"
    assert response.json()["status"] == "scheduled"
    assert runtime.calls == [("usage_resume", ("agent", "resume", True))]
