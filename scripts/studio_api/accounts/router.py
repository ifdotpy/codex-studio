"""FastAPI routes for account, project, provider catalog, and limit operations."""

from __future__ import annotations

from typing import Annotated, TYPE_CHECKING, cast

from fastapi import APIRouter, Body, Depends, HTTPException, Query, Request
from starlette.responses import Response

from studio_api.context import ApiContext
from studio_api.models import ContractModel, JsonValue
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
    from codex_runtime import Runtime


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
    values = request.query_params.getlist(name)
    return values[0] if values else default


def _limits_query(
    request: Request,
    account_key: str = Query(default="default"),
    cached: str | None = Query(default=None),
) -> LimitsQuery:
    return LimitsQuery(
        account_key=_first_query(request, "account_key", account_key) or "default",
        cached=_first_query(request, "cached", cached),
    )


def _models_query(
    request: Request,
    account_key: str = Query(default="default"),
    workers: str | None = Query(default=None),
) -> ModelsQuery:
    return ModelsQuery(
        account_key=_first_query(request, "account_key", account_key) or "default",
        workers=_first_query(request, "workers", workers),
    )


def _claude_login_query(
    request: Request,
    request_id: str | None = Query(default=None),
) -> ClaudeLoginQuery:
    return ClaudeLoginQuery(request_id=_first_query(request, "request_id", request_id))


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/accounts", response_model=AccountsResponse)
    def accounts(request: Request):
        return context.send(request, _runtime(context).accounts_snapshot())

    @router.get("/api/projects", response_model=ProjectReadResponse)
    def projects(request: Request):
        return context.send(request, _runtime(context).projects())

    @router.get("/api/models", response_model=ModelCatalogResponse)
    def models(
        request: Request,
        query: Annotated[ModelsQuery, Depends(_models_query)],
    ):
        runtime = _runtime(context)
        from codex_catalog import CatalogPending, DISPLAY_READ

        display = DISPLAY_READ.set(True)
        try:
            if query.workers == "1":
                from codex_worker_accounts import catalog

                result = catalog(runtime, query.account_key)
            else:
                result = runtime.catalog(query.account_key)
            return context.send(request, result)
        except CatalogPending as error:
            return context.send(
                request,
                {"error": str(error), "catalogPending": True},
                status=400,
            )
        finally:
            DISPLAY_READ.reset(display)

    @router.get("/api/limits", response_model=UsageLimitsResponse)
    def limits(
        request: Request,
        query: Annotated[LimitsQuery, Depends(_limits_query)],
    ):
        runtime = _runtime(context)
        current = runtime.rate_limits_for(query.account_key)
        if query.cached == "1" and (current.get("data") is not None or current.get("error")):
            return context.send(request, current)
        return context.send(request, runtime.limits(query.account_key))

    @router.get("/api/accounts/claude/login", response_model=ClaudeLoginResponse)
    def claude_login_status(
        request: Request,
        query: Annotated[ClaudeLoginQuery, Depends(_claude_login_query)],
    ):
        from codex_claude_login import manager

        return context.send(request, manager(_runtime(context)).status(query.request_id))

    @router.post("/api/accounts/claude/login", response_model=ClaudeLoginResponse)
    def claude_login_start(request: Request, body: Annotated[ClaudeStartRequest, Body()]):
        from codex_claude_login import manager

        data = _dump(body)
        return context.send(
            request,
            manager(_runtime(context)).start(data["account_key"], data["request_id"]),
        )

    @router.post("/api/accounts/claude/login/code", response_model=ClaudeLoginResponse)
    def claude_login_code(request: Request, body: Annotated[ClaudeCodeRequest, Body()]):
        from codex_claude_login import manager

        data = _dump(body)
        return context.send(
            request, manager(_runtime(context)).code(data["request_id"], data["code"])
        )

    @router.post("/api/accounts/claude/login/cancel", response_model=ClaudeLoginResponse)
    def claude_login_cancel(request: Request, body: Annotated[ClaudeCancelRequest, Body()]):
        from codex_claude_login import manager

        data = _dump(body)
        return context.send(request, manager(_runtime(context)).cancel(data["request_id"]))

    @router.post("/api/accounts/discover", response_model=AccountsResponse)
    def account_discover(request: Request):
        return context.send(request, _runtime(context).accounts.discover())

    @router.post("/api/accounts/register", response_model=AccountsResponse)
    def account_register(request: Request, body: Annotated[RegisterAccountRequest, Body()]):
        runtime = _runtime(context)
        runtime.accounts.register(body.home)
        return context.send(request, runtime.accounts.snapshot())

    @router.post("/api/accounts/default", response_model=AccountsResponse)
    def account_default(request: Request, body: Annotated[AccountKeyRequest, Body()]):
        runtime = _runtime(context)
        runtime.accounts.default(_dump(body).get("account_key"))
        return context.send(request, runtime.accounts.snapshot())

    @router.post("/api/accounts/login/cancel", response_model=AccountLoginResponse)
    def account_login_cancel(request: Request, body: Annotated[ClaudeCancelRequest, Body()]):
        runtime = _runtime(context)
        return context.send(
            request,
            runtime.accounts.cancel_login(runtime, body.request_id),
        )

    @router.post("/api/accounts/delete", response_model=AccountsResponse)
    def account_delete(request: Request, body: Annotated[DeleteAccountRequest, Body()]):
        runtime = _runtime(context)
        data = _dump(body)
        runtime.accounts.delete(data["account_key"], data["request_id"])
        return context.send(request, runtime.accounts.snapshot())

    @router.post("/api/accounts/disconnect", response_model=AccountsResponse)
    def account_disconnect(request: Request, body: Annotated[RequiredAccountKeyRequest, Body()]):
        runtime = _runtime(context)
        return context.send(request, runtime.accounts.disconnect(body.account_key))

    @router.post("/api/accounts/reconnect", response_model=AccountsResponse)
    def account_reconnect(request: Request, body: Annotated[RequiredAccountKeyRequest, Body()]):
        runtime = _runtime(context)
        return context.send(request, runtime.accounts.reconnect(body.account_key))

    @router.post("/api/accounts/login", response_model=AccountLoginResponse)
    def account_login(request: Request, body: Annotated[LoginRequest, Body()]):
        runtime = _runtime(context)
        data = _dump(body)
        return context.send(
            request,
            runtime.accounts.start_login(runtime, data["request_id"], data.get("account_key")),
        )

    @router.post("/api/claude/profiles", response_model=AccountsResponse)
    def claude_profile(request: Request, body: Annotated[ClaudeProfileRequest, Body()]):
        from codex_claude_controls import profile

        return context.send(request, profile(_runtime(context), _dump(body)))

    @router.post("/api/claude/session", response_model=ClaudeSessionResponse)
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
    ):
        from codex_claude_controls import action

        return context.send(request, action(_runtime(context), _dump(body)))

    @router.post("/api/peer-teams", response_model=PeerTeamsResponse)
    def peer_teams(request: Request, body: Annotated[PeerTeamRequest, Body()]):
        from codex_peer_teams import manage

        return context.send(request, manage(_runtime(context), _dump(body)))

    @router.post("/api/limits/reset", response_model=LimitResetResponse)
    def reset_limits(request: Request, body: Annotated[ResetRequest, Body()]):
        from codex_limit_resets import consume_reset

        return context.send(request, consume_reset(_runtime(context), _dump(body)))

    @router.post("/api/projects", response_model=ProjectsResponse)
    def project_write(request: Request, body: Annotated[ProjectWriteRequest, Body()]):
        return context.send(request, _runtime(context).projects(_dump(body)))

    return router
