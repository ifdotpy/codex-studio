"""Pure ASGI origin, session, and bounded JSON upload boundary."""
from __future__ import annotations

import asyncio
import json
import secrets
import sqlite3
import time
from collections.abc import Awaitable, Callable
from typing import TypeAlias

from starlette.datastructures import QueryParams
from starlette.types import ASGIApp, Message, Receive, Scope, Send

from studio_api.context import ApiContext
from studio_api.schema import (
    API_SCHEMA_HASH_HEADER,
    API_SCHEMA_HASH_PARAM,
    API_SCHEMA_MISMATCH_HEADER,
)

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


class HttpTraceMiddleware:
    """Keep the legacy slow-read journal around the actual ASGI request."""

    def __init__(self, app: ASGIApp) -> None:
        self.app = app

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return

        trace_id: object = None
        status: int | None = None
        outcome = "complete"
        try:
            import codex_http_traces

            target = str(scope.get("path", ""))
            query = scope.get("query_string", b"")
            if query:
                target += "?" + query.decode("latin-1")
            trace_id = codex_http_traces.begin(str(scope.get("method", "GET")), target)  # type: ignore[no-untyped-call]
        except Exception as error:
            try:
                import codex_http_traces

                codex_http_traces.report_failure(error)  # type: ignore[no-untyped-call]
            except Exception:
                pass

        async def traced_send(message: Message) -> None:
            nonlocal status
            if message["type"] == "http.response.start":
                status = int(message["status"])
            await send(message)

        try:
            await self.app(scope, receive, traced_send)
        except BaseException:
            outcome = "error"
            raise
        finally:
            try:
                import codex_http_traces

                codex_http_traces.finish(trace_id, status, outcome)  # type: ignore[no-untyped-call]
            except Exception as error:
                try:
                    import codex_http_traces

                    codex_http_traces.report_failure(error)  # type: ignore[no-untyped-call]
                    codex_http_traces.discard(trace_id)  # type: ignore[no-untyped-call]
                except Exception:
                    pass


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


async def _reject(
    send: Send,
    status: int,
    error: str,
    headers: list[tuple[bytes, bytes]] | None = None,
) -> None:
    start, body = _error_response(status, error)
    if headers:
        start["headers"].extend(headers)
    await send(start)
    await send(body)


def _request_limit(path: str) -> int:
    if path in FEDERATION_PATHS:
        return FEDERATION_BODY_LIMIT
    if path == "/api/assets":
        return ASSET_BODY_LIMIT
    if path in {"/api/voice/audio", "/api/projects"}:
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
        renderer_hash = headers.get(API_SCHEMA_HASH_HEADER)
        if not self._trusted(scope, headers, write=write, federation=federation):
            error = "Local origin and session token required" if write else "Local origin required"
            await _reject(send, 403, error)
            return

        schema_hash: str | None = None
        if path.startswith("/api/"):
            stream_hash = None
            if path == "/api/sync/stream":
                query = scope.get("query_string", b"").decode("latin-1")
                stream_hash = QueryParams(query).get(API_SCHEMA_HASH_PARAM)
                if renderer_hash is not None:
                    stream_hash = renderer_hash
            needs_hash = (write and renderer_hash is not None) or stream_hash is not None
            try:
                if needs_hash:
                    schema_hash = await self.context.get_api_schema_hash()
                else:
                    schema_hash = self.context.peek_api_schema_hash()
            except Exception:
                import logging

                logging.getLogger(__name__).exception("Studio API schema identity is unavailable")
                await _reject(send, 503, "Studio API schema identity is unavailable; restart Studio.")
                return

            if schema_hash is not None:
                raw_send = send

                async def send_with_schema_hash(message: Message) -> None:
                    if message["type"] == "http.response.start":
                        response_headers = [
                            (name, value)
                            for name, value in message.get("headers", [])
                            if name.lower() != API_SCHEMA_HASH_HEADER.lower().encode()
                        ]
                        response_headers.append(
                            (API_SCHEMA_HASH_HEADER.lower().encode(), schema_hash.encode())
                        )
                        message = {**message, "headers": response_headers}
                    await raw_send(message)

                send = send_with_schema_hash

        if write and renderer_hash is not None and renderer_hash != schema_hash:
            await _reject(
                send,
                426,
                "Studio was updated. Reload this tab to continue.",
                [(API_SCHEMA_MISMATCH_HEADER.lower().encode(), b"1")],
            )
            return

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
        if content_length is None:
            await _reject(send, 413, "Invalid request size")
            return
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
        more = True
        deadline = time.monotonic() + REQUEST_READ_TIMEOUT_SECONDS
        while more:
            try:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError
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
        if len(body) != declared:
            await _reject(send, 400, "Incomplete request")
            return
        if not federation:
            try:
                decoded = json.loads(body)
            except (ValueError, UnicodeDecodeError):
                await _reject(send, 400, "Invalid JSON")
                return
            if not isinstance(decoded, dict):
                await _reject(send, 400, "JSON object required")
                return
            workspace = headers.get("x-canvas-workspace")
            if workspace is not None or path == "/api/sync/drafts":
                try:
                    workspace_id = await asyncio.to_thread(self.context.workspace_id)
                except (OSError, RuntimeError, sqlite3.Error):
                    await _reject(send, 400, "The server workspace identity is unavailable")
                    return
                if workspace != workspace_id:
                    await _reject(send, 409, "The server workspace changed. Reload before sending.")
                    return
            # A readonly cursor sample occurs after complete body validation,
            # but before router dispatch and any service write.
            if not self.context.schema_only:
                try:
                    scope["studio_sync_entities_after"] = await asyncio.to_thread(self.context.entity_sequence)
                except (OSError, sqlite3.Error):
                    await _reject(send, 400, "The server sync state is unavailable")
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
        if federation and unix_transport:
            return False
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
        if federation:
            # Signed federation authenticates its own exact raw request; retain
            # its established Serve origin gate without local Origin heuristics.
            return True
        request_origin = headers.get("origin")
        if not unix_transport and request_origin not in (None, origin):
            return False
        if not unix_transport and headers.get("sec-fetch-site") == "cross-site":
            return False
        if write and not federation:
            return secrets.compare_digest(headers.get("x-canvas-token", "") or "", self.context.token)
        return True
