"""FastAPI routes for account, project, provider catalog, and limit operations."""

from __future__ import annotations

from typing import Annotated, TYPE_CHECKING, cast

from fastapi import APIRouter, Body, HTTPException, Query, Request
from starlette.responses import Response

from studio_api.models import ContractModel, ErrorResponse, JsonValue
from .models import (
    AccountsResponse,
    AccountLoginResponse,
    AccountKeyRequest,
    RequiredAccountKeyRequest,
    ClaudeCancelRequest,
    ClaudeCodeRequest,
    ClaudeLoginResponse,
    ClaudeLoginQuery,
    ClaudeProfileRequest,
    ClaudeSessionCommandRequest,
    ClaudeSessionResponse,
    ClaudeSessionRollbackRequest,
    ClaudeSessionSettingsRequest,
    ClaudeSessionStateRequest,
    ClaudeSessionStopTaskRequest,
    ClaudeStartRequest,
    DeleteAccountRequest,
    LoginRequest,
    LimitResetResponse,
    ModelCatalogResponse,
    ModelsQuery,
    LimitsQuery,
    PeerTeamRequest,
    PeerTeamsResponse,
    ProjectReadResponse,
    ProjectWriteRequest,
    ProjectsResponse,
    RegisterAccountRequest,
    ResetRequest,
    UsageLimitsResponse,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext
    from codex_runtime import Runtime

_ERROR_RESPONSES = {
    400: {"model": ErrorResponse, "description": "Invalid request or operation failed"},
    403: {"model": ErrorResponse, "description": "Local origin and session token required"},
    422: {"description": "Input validation errors are returned as HTTP 400"},
}


def _runtime(context: ApiContext) -> Runtime:
    runtime = context.runtime
    if runtime is None:
        raise HTTPException(status_code=404, detail="Not found")
    return runtime


def _dump(value: ContractModel) -> dict[str, JsonValue]:
    # Request models expose only declared wire keys. exclude_unset retains an
    # explicitly supplied null, which several project update operations use.
    return cast(dict[str, JsonValue], value.model_dump(mode="json", exclude_unset=True))


def _first_query(request: Request, name: str, default: str | None = None) -> str | None:
    values = [value for value in request.query_params.getlist(name) if value]
    return values[0] if values else default


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/accounts", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def accounts(request: Request) -> Response:
        return context.send(request, _runtime(context).accounts_snapshot())

    @router.get("/api/projects", response_model=ProjectReadResponse, responses=_ERROR_RESPONSES)
    def projects(request: Request) -> Response:
        return context.send(request, _runtime(context).projects())

    @router.get("/api/models", response_model=ModelCatalogResponse,
                responses={**_ERROR_RESPONSES, 400: {"model": ErrorResponse, "description": "Catalog pending or unavailable"}})
    def models(
        request: Request,
        query: Annotated[ModelsQuery, Query()],
    ) -> Response:
        runtime = _runtime(context)
        from codex_catalog import CatalogPending, DISPLAY_READ

        display = DISPLAY_READ.set(True)
        try:
            account_key = _first_query(request, "account_key", query.account_key) or "default"
            workers = [value for value in request.query_params.getlist("workers") if value]
            if workers == ["1"]:
                from codex_worker_accounts import catalog

                result = catalog(runtime, account_key)
            else:
                result = runtime.catalog(account_key)
            return context.send(request, result)
        except CatalogPending as error:
            return context.send(
                request,
                {"error": str(error), "catalogPending": True},
                status=400,
            )
        finally:
            DISPLAY_READ.reset(display)

    @router.get("/api/limits", response_model=UsageLimitsResponse, responses=_ERROR_RESPONSES)
    def limits(
        request: Request,
        query: Annotated[LimitsQuery, Query()],
    ) -> Response:
        runtime = _runtime(context)
        account_key = _first_query(request, "account_key", query.account_key) or "default"
        cached = [value for value in request.query_params.getlist("cached") if value]
        current = runtime.rate_limits_for(account_key)
        if cached == ["1"] and (current.get("data") is not None or current.get("error")):
            return context.send(request, current)
        return context.send(request, runtime.limits(account_key))

    @router.get("/api/accounts/claude/login", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_status(
        request: Request,
        query: Annotated[ClaudeLoginQuery, Query()],
    ) -> Response:
        from codex_claude_login import manager

        selected = _first_query(request, "request_id", query.request_id)
        return context.send(request, manager(_runtime(context)).status(selected))

    @router.post("/api/accounts/claude/login", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_start(request: Request, body: Annotated[ClaudeStartRequest, Body()]) -> Response:
        from codex_claude_login import manager

        data = _dump(body)
        return context.send(
            request,
            manager(_runtime(context)).start(data["account_key"], data["request_id"]),
        )

    @router.post("/api/accounts/claude/login/code", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_code(request: Request, body: Annotated[ClaudeCodeRequest, Body()]) -> Response:
        from codex_claude_login import manager

        data = _dump(body)
        return context.send(
            request, manager(_runtime(context)).code(data["request_id"], data["code"])
        )

    @router.post("/api/accounts/claude/login/cancel", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_cancel(request: Request, body: Annotated[ClaudeCancelRequest, Body()]) -> Response:
        from codex_claude_login import manager

        data = _dump(body)
        return context.send(request, manager(_runtime(context)).cancel(data["request_id"]))

    @router.post("/api/accounts/discover", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_discover(request: Request) -> Response:
        return context.send(request, _runtime(context).accounts.discover())

    @router.post("/api/accounts/register", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_register(request: Request, body: Annotated[RegisterAccountRequest, Body()]) -> Response:
        runtime = _runtime(context)
        runtime.accounts.register(body.home)
        return context.send(request, runtime.accounts.snapshot())

    @router.post("/api/accounts/default", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_default(request: Request, body: Annotated[AccountKeyRequest, Body()]) -> Response:
        runtime = _runtime(context)
        runtime.accounts.default(_dump(body).get("account_key"))
        return context.send(request, runtime.accounts.snapshot())

    @router.post("/api/accounts/login/cancel", response_model=AccountLoginResponse, responses=_ERROR_RESPONSES)
    def account_login_cancel(request: Request, body: Annotated[ClaudeCancelRequest, Body()]) -> Response:
        runtime = _runtime(context)
        return context.send(
            request,
            runtime.accounts.cancel_login(runtime, body.request_id),
        )

    @router.post("/api/accounts/delete", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_delete(request: Request, body: Annotated[DeleteAccountRequest, Body()]) -> Response:
        runtime = _runtime(context)
        data = _dump(body)
        runtime.accounts.delete(data["account_key"], data["request_id"])
        return context.send(request, runtime.accounts.snapshot())

    @router.post("/api/accounts/disconnect", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_disconnect(request: Request, body: Annotated[RequiredAccountKeyRequest, Body()]) -> Response:
        runtime = _runtime(context)
        return context.send(request, runtime.accounts.disconnect(body.account_key))

    @router.post("/api/accounts/reconnect", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_reconnect(request: Request, body: Annotated[RequiredAccountKeyRequest, Body()]) -> Response:
        runtime = _runtime(context)
        return context.send(request, runtime.accounts.reconnect(body.account_key))

    @router.post("/api/accounts/login", response_model=AccountLoginResponse, responses=_ERROR_RESPONSES)
    def account_login(request: Request, body: Annotated[LoginRequest, Body()]) -> Response:
        runtime = _runtime(context)
        data = _dump(body)
        return context.send(
            request,
            runtime.accounts.start_login(runtime, data["request_id"], data.get("account_key")),
        )

    @router.post("/api/claude/profiles", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def claude_profile(request: Request, body: Annotated[ClaudeProfileRequest, Body()]) -> Response:
        from codex_claude_controls import profile

        return context.send(request, profile(_runtime(context), _dump(body)))

    @router.post("/api/claude/session", response_model=ClaudeSessionResponse, responses=_ERROR_RESPONSES)
    def claude_session(
        request: Request,
        body: Annotated[
            ClaudeSessionStateRequest
            | ClaudeSessionCommandRequest
            | ClaudeSessionSettingsRequest
            | ClaudeSessionRollbackRequest
            | ClaudeSessionStopTaskRequest,
            Body(),
        ],
    ) -> Response:
        from codex_claude_controls import action

        return context.send(request, action(_runtime(context), _dump(body)))

    @router.post("/api/peer-teams", response_model=PeerTeamsResponse, responses=_ERROR_RESPONSES)
    def peer_teams(request: Request, body: Annotated[PeerTeamRequest, Body()]) -> Response:
        from codex_peer_teams import manage

        return context.send(request, manage(_runtime(context), _dump(body)))

    @router.post("/api/limits/reset", response_model=LimitResetResponse, responses=_ERROR_RESPONSES)
    def reset_limits(request: Request, body: Annotated[ResetRequest, Body()]) -> Response:
        from codex_limit_resets import consume_reset

        return context.send(request, consume_reset(_runtime(context), _dump(body)))

    @router.post("/api/projects", response_model=ProjectsResponse, responses=_ERROR_RESPONSES)
    def project_write(request: Request, body: Annotated[ProjectWriteRequest, Body()]) -> Response:
        return context.send(request, _runtime(context).projects(_dump(body)))

    return router
