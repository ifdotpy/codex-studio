"""FastAPI routes for account, project, provider catalog, and limit operations."""

from __future__ import annotations

from collections.abc import Callable
from pathlib import Path
from typing import Annotated, Any, TYPE_CHECKING, Protocol, cast

from fastapi import APIRouter, Body, Depends, HTTPException, Query, Request
from starlette.responses import Response

from studio_api.models import ErrorResponse, JsonValue
from studio_api.request_helpers import body_data, first_nonempty_query
from .events import publish_account_change
from .models import (
    AccountsResponse,
    AccountDiscoverRequest,
    AccountLoginResponse,
    AccountKeyRequest,
    AccountNameRequest,
    RequiredAccountKeyRequest,
    ClaudeCancelRequest,
    ClaudeAddStartRequest,
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
    ProjectLocationRequest,
    ProjectLocationQuery,
    ProjectLocationQueryResponse,
    SidebarReorderRequest,
    ProjectMutationResponse,
    RegisterAccountRequest,
    ResetRequest,
    UsageLimitsResponse,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext


class AccountService(Protocol):
    root: Path
    discovered: bool

    def snapshot(self) -> JsonValue: ...

    def discover(self) -> JsonValue: ...

    def register(self, home: str) -> str: ...

    def default(self, key: str | None = None) -> JsonValue: ...

    def cancel_login(self, runtime: RuntimePort, request_id: str) -> JsonValue: ...

    def delete(self, key: str, request_id: str) -> JsonValue: ...

    def set_name(self, key: str, label: str, request_id: str) -> JsonValue: ...

    def disconnect(self, key: str) -> JsonValue: ...

    def reconnect(self, key: str) -> JsonValue: ...

    def start_login(
        self, runtime: RuntimePort, request_id: str, account_key: str | None = None,
        email: str | None = None, label: str | None = None,
    ) -> JsonValue: ...


class RuntimePort(Protocol):
    root: Path
    accounts: AccountService

    def accounts_snapshot(self) -> JsonValue: ...

    def projects(self, data: dict[str, JsonValue] | None = None) -> JsonValue: ...

    def rate_limits_for(self, account_key: str) -> dict[str, JsonValue]: ...

    def limits(self, account_key: str) -> dict[str, JsonValue]: ...

    def refresh_limits_background(self, account_key: str) -> None: ...

    def catalog(self, account_key: str) -> JsonValue: ...


class ClaudeLoginService(Protocol):
    def status(self, request_id: str | None) -> JsonValue: ...

    def start(self, account_key: str, request_id: str) -> JsonValue: ...

    def start_add(self, email: str | None, label: str | None, request_id: str) -> JsonValue: ...

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


def _accounts_before(runtime: RuntimePort) -> JsonValue | None:
    return runtime.accounts.snapshot() if runtime.accounts.discovered else None


def _publish_accounts_if_changed(runtime: RuntimePort, before: JsonValue | None) -> None:
    if before is None or runtime.accounts.snapshot() != before:
        publish_account_change(runtime.accounts.root.parent)


def _publish_login_if_changed(
    runtime: RuntimePort,
    before: JsonValue | None,
    after: JsonValue,
) -> None:
    if before != after:
        publish_account_change(runtime.accounts.root.parent)


def _codex_login_response(value: JsonValue) -> JsonValue:
    """Expose the account login receipt fields, not internal identity guards."""
    if not isinstance(value, dict):
        return value
    allowed = {
        "requestId", "accountKey", "status", "loginId", "verificationUrl", "userCode",
        "error", "resolvedAccountKey", "email", "createdAt", "expiresAt",
    }
    return {key: item for key, item in value.items() if key in allowed}


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
        from codex_catalog import CatalogPending, CatalogUnavailable, DISPLAY_READ, DISPLAY_RETRY

        display = DISPLAY_READ.set(True)
        retry = DISPLAY_RETRY.set(
            query.retry == "1" and request.query_params.getlist("retry") == ["1"]
        )
        try:
            account_key = first_nonempty_query(request, "account_key", query.account_key) or "default"
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
        except CatalogUnavailable as error:
            return context.send(
                request,
                {"error": str(error)},
                status=400,
            )
        finally:
            DISPLAY_RETRY.reset(retry)
            DISPLAY_READ.reset(display)

    @router.get("/api/limits", response_model=UsageLimitsResponse, responses=_ERROR_RESPONSES)
    def limits(
        request: Request,
        query: Annotated[LimitsQuery, Query()],
    ) -> Response:
        runtime = _runtime(context)
        account_key = first_nonempty_query(request, "account_key", query.account_key) or "default"
        cached = [value for value in request.query_params.getlist("cached") if value]
        current = runtime.rate_limits_for(account_key)
        if cached == ["1"] and (current.get("data") is not None or current.get("error")):
            return context.send(request, current)
        if current.get("data") is not None:
            runtime.refresh_limits_background(account_key)
            return context.send(request, current)
        return context.send(request, runtime.limits(account_key))

    @router.get("/api/accounts/claude/login", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_status(
        request: Request,
        query: Annotated[ClaudeLoginQuery, Query()],
    ) -> Response:
        selected = first_nonempty_query(request, "request_id", query.request_id)
        return context.send(request, _claude_login(_runtime(context)).status(selected))

    @router.post("/api/accounts/claude/login", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_start(request: Request, body: Annotated[ClaudeStartRequest, Body()]) -> Response:
        runtime = _runtime(context)
        service = _claude_login(runtime)
        try:
            before = service.status(body.login_id)
        except ValueError:
            before = None
        result = service.start(body.account_key, body.login_id)
        _publish_login_if_changed(runtime, before, result)
        return context.send(request, result)

    @router.post("/api/accounts/claude/add", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_add_start(request: Request, body: Annotated[ClaudeAddStartRequest, Body()]) -> Response:
        runtime = _runtime(context)
        service = _claude_login(runtime)
        try:
            before = service.status(body.login_id)
        except ValueError:
            before = None
        result = service.start_add(body.email, body.label, body.login_id)
        _publish_login_if_changed(runtime, before, result)
        return context.send(request, result)

    @router.post("/api/accounts/claude/login/code", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_code(request: Request, body: Annotated[ClaudeCodeRequest, Body()]) -> Response:
        runtime = _runtime(context)
        service = _claude_login(runtime)
        before = service.status(body.login_id)
        result = service.code(body.login_id, body.code)
        _publish_login_if_changed(runtime, before, result)
        return context.send(request, result)

    @router.post("/api/accounts/claude/login/cancel", response_model=ClaudeLoginResponse, responses=_ERROR_RESPONSES)
    def claude_login_cancel(request: Request, body: Annotated[ClaudeCancelRequest, Body()]) -> Response:
        runtime = _runtime(context)
        service = _claude_login(runtime)
        before = service.status(body.login_id)
        result = service.cancel(body.login_id)
        _publish_login_if_changed(runtime, before, result)
        return context.send(request, result)

    @router.post("/api/accounts/discover", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_discover(
        request: Request,
        body: Annotated[AccountDiscoverRequest, Body()] = AccountDiscoverRequest(),
    ) -> Response:
        del body
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        result = runtime.accounts.discover()
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/register", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_register(request: Request, body: Annotated[RegisterAccountRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        runtime.accounts.register(body.home)
        result = runtime.accounts.snapshot()
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/default", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_default(request: Request, body: Annotated[AccountKeyRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        runtime.accounts.default(body.account_key)
        result = runtime.accounts.snapshot()
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/login/cancel", response_model=AccountLoginResponse, responses=_ERROR_RESPONSES)
    def account_login_cancel(request: Request, body: Annotated[ClaudeCancelRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        result = _codex_login_response(runtime.accounts.cancel_login(runtime, body.login_id))
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/delete", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_delete(request: Request, body: Annotated[DeleteAccountRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        runtime.accounts.delete(body.account_key, body.request_id)
        result = runtime.accounts.snapshot()
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/name", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_name(request: Request, body: Annotated[AccountNameRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        result = runtime.accounts.set_name(body.account_key, body.label, body.request_id)
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/disconnect", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_disconnect(request: Request, body: Annotated[RequiredAccountKeyRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        result = runtime.accounts.disconnect(body.account_key)
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/reconnect", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def account_reconnect(request: Request, body: Annotated[RequiredAccountKeyRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        result = runtime.accounts.reconnect(body.account_key)
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/accounts/login", response_model=AccountLoginResponse, responses=_ERROR_RESPONSES)
    def account_login(request: Request, body: Annotated[LoginRequest, Body()]) -> Response:
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        result = _codex_login_response(runtime.accounts.start_login(
            runtime, body.login_id, body.account_key, body.email, body.label,
        ))
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

    @router.post("/api/claude/profiles", response_model=AccountsResponse, responses=_ERROR_RESPONSES)
    def claude_profile(request: Request, body: Annotated[ClaudeProfileRequest, Body()]) -> Response:
        from codex_claude_controls import profile

        profile_service = cast(Callable[[RuntimePort, dict[str, JsonValue]], JsonValue], profile)
        runtime = _runtime(context)
        before = _accounts_before(runtime)
        result = profile_service(runtime, body_data(body))
        _publish_accounts_if_changed(runtime, before)
        return context.send(request, result)

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
        return context.send(request, session_action(_runtime(context), body_data(body)))

    @router.post("/api/peer-teams", response_model=PeerTeamsResponse, responses=_ERROR_RESPONSES)
    def peer_teams(request: Request, body: Annotated[PeerTeamRequest, Body()]) -> Response:
        from codex_peer_teams import manage

        peer_team_service = cast(Callable[[RuntimePort, dict[str, JsonValue]], JsonValue], manage)
        return context.send(request, peer_team_service(_runtime(context), body_data(body)))

    @router.post("/api/limits/reset", response_model=LimitResetResponse, responses=_ERROR_RESPONSES)
    def reset_limits(request: Request, body: Annotated[ResetRequest, Body()]) -> Response:
        from codex_limit_resets import consume_reset

        reset_service = cast(Callable[[RuntimePort, dict[str, JsonValue]], JsonValue], consume_reset)
        return context.send(request, reset_service(_runtime(context), body_data(body)))

    @router.get("/api/project-locations", response_model=ProjectLocationQueryResponse, responses=_ERROR_RESPONSES)
    def project_location_query(request: Request, query: ProjectLocationQuery = Depends()) -> Response:
        from codex_project_locations import query as read_location
        try:
            result = read_location(_runtime(context), query.model_dump(exclude_none=True))
        except PermissionError as error:
            return context.send(request, {"error": str(error)}, status=403)
        except (ValueError, RuntimeError) as error:
            return context.send(request, {"error": str(error)}, status=400)
        return context.send(request, result)

    @router.post("/api/projects", response_model=ProjectMutationResponse, responses=_ERROR_RESPONSES)
    def project_write(request: Request, body: Annotated[ProjectWriteRequest | SidebarReorderRequest | ProjectLocationRequest, Body()]) -> Response:
        from codex_project_folders import SidebarOrderConflict

        try:
            result = _runtime(context).projects(body_data(body))
        except PermissionError as error:
            return context.send(request, {"error": str(error)}, status=403)
        except SidebarOrderConflict as error:
            return context.send(request, {"error": str(error)}, status=409)
        return context.send(request, result)

    return router
