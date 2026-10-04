"""FastAPI routes for account, project, provider catalog, and limit operations."""

from __future__ import annotations

from collections.abc import Callable
from typing import Annotated, Any, TYPE_CHECKING, Protocol, cast

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
    ProjectMutationResponse,
    RegisterAccountRequest,
    ResetRequest,
    UsageLimitsResponse,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext


class AccountService(Protocol):
    def snapshot(self) -> JsonValue: ...

    def discover(self) -> JsonValue: ...

    def register(self, home: str) -> str: ...

    def default(self, key: str | None = None) -> JsonValue: ...

    def cancel_login(self, runtime: RuntimePort, request_id: str) -> JsonValue: ...

    def delete(self, key: str, request_id: str) -> JsonValue: ...

    def disconnect(self, key: str) -> JsonValue: ...

    def reconnect(self, key: str) -> JsonValue: ...

    def start_login(self, runtime: RuntimePort, request_id: str, account_key: str | None = None) -> JsonValue: ...


class RuntimePort(Protocol):
    accounts: AccountService

    def projects(self, data: dict[str, JsonValue] | None = None) -> JsonValue: ...

    def rate_limits_for(self, account_key: str) -> dict[str, JsonValue]: ...

    def limits(self, account_key: str) -> dict[str, JsonValue]: ...

    def catalog(self, account_key: str) -> JsonValue: ...


class ClaudeLoginService(Protocol):
    def status(self, request_id: str | None) -> JsonValue: ...

    def start(self, account_key: str, request_id: str) -> JsonValue: ...

    def code(self, request_id: str, code: str) -> JsonValue: ...

    def cancel(self, request_id: str) -> JsonValue: ...


_ERROR_RESPONSES: dict[int | str, dict[str, Any]] = {
    400: {"model": ErrorResponse, "description": "Invalid request or operation failed"},
    403: {"model": ErrorResponse, "description": "Local origin and session token required"},
    422: {"description": "Input validation errors are returned as HTTP 400"},
}


def _runtime(context: ApiContext) -> RuntimePort:
    runtime = context.runtime
    if runtime is None:
        raise HTTPException(status_code=404, detail="Not found")
    return cast(RuntimePort, runtime)


def _claude_login(runtime: RuntimePort) -> ClaudeLoginService:
    from codex_claude_login import manager

    manager_factory = cast(Callable[[RuntimePort], ClaudeLoginService], manager)
    return manager_factory(runtime)


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
        return context.send(request, _runtime(context).accounts.snapshot())

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

                worker_catalog = cast(Callable[[RuntimePort, str], JsonValue], catalog)
                result = worker_catalog(runtime, account_key)
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
        selected = _first_query(request, "request_id", query.request_id)
        return context.send(request, _claude_login(_runtime(context)).status(selected))

    @router.post("/api/accounts/claude/login", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_start(request: Request, body: Annotated[ClaudeStartRequest, Body()]) -> Response:
        return context.send(
            request,
            _claude_login(_runtime(context)).start(body.account_key, body.request_id),
        )

    @router.post("/api/accounts/claude/login/code", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_code(request: Request, body: Annotated[ClaudeCodeRequest, Body()]) -> Response:
        return context.send(
            request, _claude_login(_runtime(context)).code(body.request_id, body.code)
        )

    @router.post("/api/accounts/claude/login/cancel", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_cancel(request: Request, body: Annotated[ClaudeCancelRequest, Body()]) -> Response:
        return context.send(request, _claude_login(_runtime(context)).cancel(body.request_id))

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
        runtime.accounts.default(body.account_key)
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
        runtime.accounts.delete(body.account_key, body.request_id)
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
        return context.send(
            request,
            runtime.accounts.start_login(runtime, body.request_id, body.account_key),
        )

    @router.post("/api/claude/profiles", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def claude_profile(request: Request, body: Annotated[ClaudeProfileRequest, Body()]) -> Response:
        from codex_claude_controls import profile

        profile_service = cast(Callable[[RuntimePort, dict[str, JsonValue]], JsonValue], profile)
        return context.send(request, profile_service(_runtime(context), _dump(body)))

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

        session_action = cast(Callable[[RuntimePort, dict[str, JsonValue]], JsonValue], action)
        return context.send(request, session_action(_runtime(context), _dump(body)))

    @router.post("/api/peer-teams", response_model=PeerTeamsResponse, responses=_ERROR_RESPONSES)
    def peer_teams(request: Request, body: Annotated[PeerTeamRequest, Body()]) -> Response:
        from codex_peer_teams import manage

        peer_team_service = cast(Callable[[RuntimePort, dict[str, JsonValue]], JsonValue], manage)
        return context.send(request, peer_team_service(_runtime(context), _dump(body)))

    @router.post("/api/limits/reset", response_model=LimitResetResponse, responses=_ERROR_RESPONSES)
    def reset_limits(request: Request, body: Annotated[ResetRequest, Body()]) -> Response:
        from codex_limit_resets import consume_reset

        reset_service = cast(Callable[[RuntimePort, dict[str, JsonValue]], JsonValue], consume_reset)
        return context.send(request, reset_service(_runtime(context), _dump(body)))

    @router.post("/api/projects", response_model=ProjectMutationResponse, responses=_ERROR_RESPONSES)
    def project_write(request: Request, body: Annotated[ProjectWriteRequest, Body()]) -> Response:
        return context.send(request, _runtime(context).projects(_dump(body)))

    return router
