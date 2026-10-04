"""Pure ASGI origin, session, and bounded JSON upload boundary."""
from __future__ import annotations

import asyncio
import json
import secrets
from collections.abc import Awaitable, Callable
from typing import TypeAlias

from starlette.types import ASGIApp, Message, Receive, Scope, Send

from studio_api.context import ApiContext

ASGIHandler: TypeAlias = Callable[[Scope, Receive, Send], Awaitable[None]]
FEDERATION_PATHS = frozenset({
    "/api/federation/v1/pair", "/api/federation/v1/status",
    "/api/federation/v1/message", "/api/federation/v1/pull",
})
DEFAULT_BODY_LIMIT = 262_144
ASSET_BODY_LIMIT = 28 * 1024 * 1024
VOICE_AUDIO_BODY_LIMIT = 6 * 1024 * 1024
FEDERATION_BODY_LIMIT = 256 * 1024
REQUEST_READ_TIMEOUT_SECONDS = 10.0


class HeaderView:
    """Case-insensitive header view retaining duplicates for policy checks."""

    def __init__(self, raw: list[tuple[bytes, bytes]]) -> None:
        self._values: dict[str, list[str]] = {}
        for name, value in raw:
            self._values.setdefault(name.decode("latin-1").lower(), []).append(value.decode("latin-1"))

    def get(self, name: str, default: str | None = None) -> str | None:
        values = self._values.get(name.lower(), [])
        return values[0] if values else default

    def get_all(self, name: str, default: list[str] | None = None) -> list[str]:
        return list(self._values.get(name.lower(), default or []))


def _error_response(status: int, error: str) -> tuple[Message, Message]:
    body = json.dumps({"error": error}, ensure_ascii=False, separators=(",", ":")).encode()
    start: Message = {
        "type": "http.response.start", "status": status,
        "headers": [
            (b"content-type", b"application/json; charset=utf-8"),
            (b"content-length", str(len(body)).encode()),
            (b"cache-control", b"no-store"),
            (b"x-content-type-options", b"nosniff"),
        ],
    }
    return start, {"type": "http.response.body", "body": body}


async def _reject(send: Send, status: int, error: str) -> None:
    start, body = _error_response(status, error)
    await send(start)
    await send(body)


def _request_limit(path: str) -> int:
    if path in FEDERATION_PATHS:
        return FEDERATION_BODY_LIMIT
    if path == "/api/assets":
        return ASSET_BODY_LIMIT
    if path == "/api/voice/audio":
        return VOICE_AUDIO_BODY_LIMIT
    return DEFAULT_BODY_LIMIT


class RequestBoundary:
    """Authorize before FastAPI validation and prebuffer bounded JSON writes."""

    def __init__(self, app: ASGIApp, context: ApiContext) -> None:
        self.app = app
        self.context = context

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return

        path = scope.get("path", "")
        method = scope.get("method", "GET").upper()
        headers = HeaderView(scope.get("headers", []))
        federation = method == "POST" and path in FEDERATION_PATHS
        write = method not in {"GET", "HEAD", "OPTIONS"}
        if not self._trusted(scope, headers, write=write, federation=federation):
            error = "Local origin and session token required" if write else "Local origin required"
            await _reject(send, 403, error)
            return

        if write and not federation:
            # This is a readonly cursor sample. It does not construct a service
            # or touch durable state before input validation and route handling.
            scope["studio_sync_entities_after"] = self.context.entity_sequence()

        if method not in {"POST", "PUT", "PATCH"}:
            await self.app(scope, receive, send)
            return

        limit = _request_limit(path)
        content_type = (headers.get("content-type") or "").split(";", 1)[0].strip().lower()
        if content_type != "application/json":
            await _reject(send, 415, "JSON required")
            return
        content_length = headers.get("content-length")
        declared: int | None = None
        if content_length is not None:
            try:
                declared = int(content_length)
            except ValueError:
                await _reject(send, 400, "Invalid request size")
                return
            if declared <= 0:
                await _reject(send, 413, "Invalid request size")
                return
            if declared > limit:
                await _reject(send, 413, "Invalid request size")
                return

        body = bytearray()
        deadline = asyncio.get_running_loop().time() + REQUEST_READ_TIMEOUT_SECONDS
        more = True
        while more:
            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                await _reject(send, 408, "Request body timed out")
                return
            try:
                message = await asyncio.wait_for(receive(), remaining)
            except TimeoutError:
                await _reject(send, 408, "Request body timed out")
                return
            if message["type"] == "http.disconnect":
                return
            if message["type"] != "http.request":
                continue
            body.extend(message.get("body", b""))
            if len(body) > limit:
                await _reject(send, 413, "Invalid request size")
                return
            more = message.get("more_body", False)

        if not body:
            await _reject(send, 413, "Invalid request size")
            return
        if declared is not None and len(body) != declared:
            await _reject(send, 400, "Incomplete request")
            return
        delivered = False

        async def replay_receive() -> Message:
            nonlocal delivered
            if delivered:
                return {"type": "http.request", "body": b"", "more_body": False}
            delivered = True
            return {"type": "http.request", "body": bytes(body), "more_body": False}

        await self.app(scope, replay_receive, send)

    def _trusted(self, scope: Scope, headers: HeaderView, *, write: bool, federation: bool) -> bool:
        extensions = scope.get("extensions", {})
        unix_transport = bool(extensions.get("studio.unix_socket"))
        if unix_transport:
            origin: str | None = "http://unix"
        else:
            client = scope.get("client")
            peer = str(client[0]) if client else ""
            server = scope.get("server")
            port = int(server[1]) if server and server[1] else 0
            origin = self.context.remote.request_origin(headers, peer, port)
        if origin is None:
            return False
        request_origin = headers.get("origin")
        if not unix_transport and request_origin not in (None, origin):
            return False
        if not unix_transport and headers.get("sec-fetch-site") == "cross-site":
            return False
        if write and not federation:
            return secrets.compare_digest(headers.get("x-canvas-token", "") or "", self.context.token)
        return True
