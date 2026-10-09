"""Local owner management and signed remote pairing routes."""
from __future__ import annotations

import sqlite3
from collections.abc import Callable
from typing import TYPE_CHECKING, Any, Protocol, cast

from fastapi import APIRouter, Request
from starlette.concurrency import run_in_threadpool
from starlette.responses import Response

from codex_multi_server import AccessError, MultiServerService
from studio_api.multi_server.models import (
    AccessAuditResponse, AccessSnapshot, AcceptInvite, CreateInvite, DevicePairRequest,
    DevicePairResponse, InvitationResponse, ManagementRequest, RevokeClient,
    ServerOperationRequest, ServerOperationResponse, DiscoveryIdentity, AutoPairRequest,
    DiscoverServers, UiInvite, SetAccessSettings, UnrevokeServer, SetServerAlias,
)
from studio_api.models import ErrorResponse

if TYPE_CHECKING:
    from studio_api.context import ApiContext

ERRORS: dict[int | str, dict[str, Any]] = {
    status: {"model": ErrorResponse} for status in (400, 401, 403, 404, 409, 410, 413, 429, 502, 503, 504)
}


class OrchestrationService(Protocol):
    def receive(self, server_id: str, envelope: dict[str, Any]) -> object: ...


class OrchestrationRuntime(Protocol):
    def multi_server(self) -> OrchestrationService: ...


def service_for(context: ApiContext) -> MultiServerService:
    runtime = context.runtime
    if runtime is None:
        raise AccessError(503, "runtime_unavailable", "The server runtime is unavailable")
    service = cast(MultiServerService, runtime.paired_access())
    service.public_origin = context.remote.origin()
    return service


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter(tags=["server access"])

    async def dispatch(request: Request, callback: Callable[[], object]) -> Response:
        def run() -> Response:
            try:
                return context.send(request, callback())
            except AccessError as error:
                return context.send(request, ErrorResponse(error=str(error), code=error.code), status=error.status)
            except (ValueError, OSError, RuntimeError, sqlite3.Error):
                return context.send(request, ErrorResponse(error="The server access operation failed", code="access_failed"), status=503)
        return await run_in_threadpool(run)

    @router.get("/api/multi-server", response_model=AccessSnapshot, responses=ERRORS)
    async def snapshot(request: Request) -> Response:
        return await dispatch(request, lambda: service_for(context).snapshot())

    @router.get("/api/multi-server/audit", response_model=AccessAuditResponse, responses=ERRORS)
    async def audit(request: Request) -> Response:
        return await dispatch(request, lambda: service_for(context).audit())

    @router.post("/api/multi-server", response_model=AccessSnapshot | InvitationResponse, responses=ERRORS)
    async def manage(request: Request, body: ManagementRequest) -> Response:
        def action() -> object:
            service = service_for(context)
            principal = request.scope.get("studio_principal") or {}
            actor = principal.get("clientId", "local")
            if isinstance(body, (DiscoverServers, UiInvite, SetServerAlias)):
                if principal or any(request.headers.get(name) is not None for name in ("X-Forwarded-For", "X-Forwarded-Host", "X-Forwarded-Proto")):
                    raise AccessError(403, "local_session_required", "This action requires the local server session")
                if isinstance(body, DiscoverServers):
                    return service.discovery().discover()
                if isinstance(body, SetServerAlias):
                    return service.set_server_alias(body.serverId, body.alias, body.requestId, actor)
                return service.ui_invite(body.serverId, body.requestId)
            if isinstance(body, SetAccessSettings):
                return service.discovery().settings(body.autoPair, actor)
            if isinstance(body, UnrevokeServer):
                return service.discovery().unrevoke(body.clientId, body.requestId, actor)
            if isinstance(body, CreateInvite):
                return service.create_invite(body.model_dump(exclude_unset=True), actor)
            if isinstance(body, RevokeClient):
                return service.revoke(body.clientId, actor)
            if isinstance(body, AcceptInvite):
                return service.accept_invite(body.invitation.model_dump(), body.requestId, actor)
            raise AccessError(400, "invalid_action", "The server access action is invalid")
        return await dispatch(request, action)

    @router.get("/api/multi-server/v1/identity", response_model=DiscoveryIdentity, responses=ERRORS)
    async def discovery_identity(request: Request) -> Response:
        if not request.scope.get("studio_discovery_owner"):
            return context.send(request, ErrorResponse(error="The identity endpoint requires owner proof through Serve", code="serve_required"), status=403)
        return await dispatch(request, lambda: service_for(context).discovery().identity())

    @router.post("/api/multi-server/v1/auto-pair", response_model=DevicePairResponse, responses=ERRORS)
    async def auto_pair(request: Request, body: AutoPairRequest) -> Response:
        return await dispatch(request, lambda: service_for(context).discovery().accept(request.scope.get("studio_principal") or {}, body.model_dump()))

    @router.post("/api/multi-server/v1/pair", response_model=DevicePairResponse, responses=ERRORS)
    async def pair(request: Request, body: DevicePairRequest) -> Response:
        def action() -> object:
            principal = request.scope.get("studio_principal")
            if not isinstance(principal, dict) or not principal.get("pairing"):
                raise AccessError(403, "serve_required", "A signed Tailscale Serve request is required")
            return service_for(context).pair(principal, body.model_dump(exclude_unset=True))
        return await dispatch(request, action)

    @router.post("/api/servers/orchestration", response_model=ServerOperationResponse, responses=ERRORS)
    async def orchestration(request: Request, body: ServerOperationRequest) -> Response:
        def action() -> object:
            principal = request.scope.get("studio_principal")
            if not isinstance(principal, dict) or principal.get("kind") != "server" or principal.get("pairing"):
                raise AccessError(403, "server_required", "A paired server credential is required")
            runtime = context.runtime
            if runtime is None:
                raise AccessError(503, "runtime_unavailable", "The server runtime is unavailable")
            if body.requestId != principal.get("requestId"):
                raise AccessError(409, "request_conflict", "The operation ID does not match its signature")
            orchestration_runtime = cast(OrchestrationRuntime, runtime)
            if not hasattr(runtime, "multi_server"):
                raise AccessError(503, "orchestration_unavailable", "The cross-server service is unavailable")
            return orchestration_runtime.multi_server().receive(principal["clientId"], body.model_dump())
        return await dispatch(request, action)

    @router.get("/api/multi-server/v1/pair", response_model=ErrorResponse, include_in_schema=False)
    async def no_pair_get(request: Request) -> Response:
        return context.send(request, {"error": "Not found"}, status=404)

    return router
