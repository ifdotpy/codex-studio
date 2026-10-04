"""Federation management and signed peer HTTP routes."""

from __future__ import annotations

import sqlite3
from email.message import Message
from typing import TYPE_CHECKING
from pydantic import TypeAdapter, ValidationError

from fastapi import APIRouter, Request
from starlette.concurrency import run_in_threadpool
from starlette.responses import Response

from studio_api.models import ErrorResponse
from studio_api.federation.models import (
    ApprovePeerRequest,
    CreateInviteResponse,
    FederationSnapshot,
    ManagementRequest,
    MessageRequest,
    PairRequest,
    PullRequest,
    SignedMessageResponse,
    SignedPairResponse,
    SignedPullResponse,
    SignedStatusResponse,
    StatusRequest,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext


class RequestHeaders:
    """Case-insensitive request headers retaining duplicate values."""

    def __init__(self, raw: list[tuple[bytes, bytes]]) -> None:
        self._values: dict[str, list[str]] = {}
        for name, value in raw:
            self._values.setdefault(name.decode("latin-1").lower(), []).append(
                value.decode("latin-1")
            )

    def get(self, name: str, default: str | None = None) -> str | None:
        values = self._values.get(name.lower(), [])
        return values[0] if values else default

    def get_all(self, name: str, default: list[str] | None = None) -> list[str]:
        return list(self._values.get(name.lower(), default or []))

MAX_BODY_BYTES = 256 * 1024
FEDERATION_PATHS = (
    "/api/federation/v1/pair",
    "/api/federation/v1/status",
    "/api/federation/v1/message",
    "/api/federation/v1/pull",
)
NO_GET_PATHS = ("/api/federation", *FEDERATION_PATHS)


def _raw_headers(request: Request) -> RequestHeaders:
    return RequestHeaders(request.headers.raw)


def _openapi_body(schema: dict[str, object]) -> dict[str, object]:
    return {
        "requestBody": {
            "required": True,
            "content": {"application/json": {"schema": schema}},
        }
    }


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.post(
        "/api/federation",
        response_model=FederationSnapshot | CreateInviteResponse,
        responses={400: {"model": ErrorResponse}, 403: {"model": ErrorResponse},
                   413: {"model": ErrorResponse}, 415: {"model": ErrorResponse},
                   503: {"model": ErrorResponse}},
        openapi_extra=_openapi_body(TypeAdapter(ManagementRequest).json_schema()),
    )
    async def manage_federation(request: Request) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Runtime unavailable"}, status=503)

        content_length = request.headers.get("content-length", "0")
        try:
            length = int(content_length)
        except ValueError:
            return context.send(request, {"error": "Invalid request size"}, status=400)
        if not 0 < length <= 262_144:
            return context.send(request, {"error": "Invalid request size"}, status=413)
        content_type = Message()
        content_type["Content-Type"] = request.headers.get("content-type", "")
        if content_type.get_content_type() != "application/json":
            return context.send(request, {"error": "JSON required"}, status=415)
        raw = await request.body()
        if len(raw) != length:
            return context.send(request, {"error": "Incomplete request"}, status=400)
        try:
            body: ManagementRequest = TypeAdapter(ManagementRequest).validate_json(raw)
        except ValidationError:
            return context.send(request, {"error": "Invalid federation request"}, status=400)

        def dispatch() -> Response:
            try:
                value = body.model_dump(mode="json", exclude_unset=True)
                service = runtime.federation()  # type: ignore[no-untyped-call]
                if isinstance(body, ApprovePeerRequest):
                    result = service.approve_peer(body.state_id, body.accept_missing_whois)
                else:
                    result = service.action(value)
                return context.send(request, result)
            except PermissionError as error:
                return context.send(request, {"error": str(error)}, status=403)
            except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
                return context.send(request, {"error": str(error)}, status=400)

        return await run_in_threadpool(dispatch)

    async def signed_post(request: Request, action: str) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Runtime unavailable"}, status=503)

        listener = request.scope.get("server")
        port = context.server_port
        if not context.unix_socket and isinstance(listener, tuple) and len(listener) >= 2:
            port = listener[1] or context.server_port
        peer = request.client.host if request.client is not None else ""
        headers = _raw_headers(request)
        try:
            remote = context.remote
            origin = remote.request_origin(headers, peer, port)
        except ValueError as error:
            return context.send(request, {"error": str(error)}, status=400)
        if origin is None:
            return context.send(request, {"error": "Tailscale Serve origin required"}, status=403)

        content_length = request.headers.get("content-length", "0")
        try:
            length = int(content_length)
        except ValueError:
            return context.send(request, {"error": "Invalid Content-Length"}, status=400)
        if not 0 < length <= MAX_BODY_BYTES:
            return context.send(request, {"error": "Invalid federation request size"}, status=413)

        content_type = Message()
        content_type["Content-Type"] = request.headers.get("content-type", "")
        if content_type.get_content_type() != "application/json":
            return context.send(request, {"error": "JSON required"}, status=415)

        raw = await request.body()
        if len(raw) != length:
            return context.send(request, {"error": "Incomplete federation request"}, status=400)

        def dispatch() -> Response:
            try:
                # Keep the signed byte sequence untouched; the service verifies
                # its signature against these exact bytes.
                result = runtime.federation().route(action, headers, peer, raw)  # type: ignore[no-untyped-call]
                return context.send(request, result)
            except PermissionError as error:
                return context.send(request, {"error": str(error)}, status=403)
            except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
                return context.send(request, {"error": str(error)}, status=400)
        return await run_in_threadpool(dispatch)

    @router.post(
        FEDERATION_PATHS[0],
        response_model=SignedPairResponse,
        responses={400: {"model": ErrorResponse}, 403: {"model": ErrorResponse},
                   413: {"model": ErrorResponse}, 415: {"model": ErrorResponse}},
        openapi_extra=_openapi_body(PairRequest.model_json_schema()),
    )
    async def pair(request: Request) -> Response:
        return await signed_post(request, "pair")

    @router.post(
        FEDERATION_PATHS[1],
        response_model=SignedStatusResponse,
        responses={400: {"model": ErrorResponse}, 403: {"model": ErrorResponse},
                   413: {"model": ErrorResponse}, 415: {"model": ErrorResponse}},
        openapi_extra=_openapi_body(StatusRequest.model_json_schema()),
    )
    async def status(request: Request) -> Response:
        return await signed_post(request, "status")

    @router.post(
        FEDERATION_PATHS[2],
        response_model=SignedMessageResponse,
        responses={400: {"model": ErrorResponse}, 403: {"model": ErrorResponse},
                   413: {"model": ErrorResponse}, 415: {"model": ErrorResponse}},
        openapi_extra=_openapi_body(MessageRequest.model_json_schema()),
    )
    async def message(request: Request) -> Response:
        return await signed_post(request, "message")

    @router.post(
        FEDERATION_PATHS[3],
        response_model=SignedPullResponse,
        responses={400: {"model": ErrorResponse}, 403: {"model": ErrorResponse},
                   413: {"model": ErrorResponse}, 415: {"model": ErrorResponse}},
        openapi_extra=_openapi_body(PullRequest.model_json_schema()),
    )
    async def pull(request: Request) -> Response:
        return await signed_post(request, "pull")

    async def no_get_counterpart(request: Request) -> Response:
        return context.send(request, {"error": "Not found"}, status=404)

    for path in NO_GET_PATHS:
        router.add_api_route(path, no_get_counterpart, methods=["GET"],
                             response_model=ErrorResponse, include_in_schema=False)

    return router
