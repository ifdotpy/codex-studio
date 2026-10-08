"""Authenticate Serve API requests before the existing local API boundary."""
from __future__ import annotations

import asyncio
import base64
import json
import sqlite3
import time
from typing import TYPE_CHECKING, Any

from starlette.types import ASGIApp, Message, Receive, Scope, Send

from codex_multi_server import AccessError, MAX_RESPONSE_BYTES, SIGNATURE_HEADERS
from studio_api.middleware import (
    FEDERATION_PATHS, HeaderView, REQUEST_READ_TIMEOUT_SECONDS, _request_limit,
)
from studio_api.multi_server.router import service_for
from studio_api.schema import API_SCHEMA_HASH_HEADER, API_SCHEMA_MISMATCH_HEADER

if TYPE_CHECKING:
    from studio_api.context import ApiContext

CORS_HEADERS = [
    (b"access-control-allow-origin", b"*"),
    (b"access-control-expose-headers", f"X-Studio-Server, {API_SCHEMA_HASH_HEADER}, {API_SCHEMA_MISMATCH_HEADER}, ETag".encode()),
]
CORS_REQUEST_HEADERS = (
    "content-type", "accept", "x-canvas-workspace", API_SCHEMA_HASH_HEADER.lower(),
    "x-codex-sync-protocol", "last-event-id", "if-none-match", "cache-control", "range",
    *(header.lower() for header in SIGNATURE_HEADERS),
)
STREAM_RECHECK_SECONDS = 5.0


async def _failure(send: Send, error: AccessError) -> None:
    body = json.dumps({"error": str(error), "code": error.code}, separators=(",", ":")).encode()
    await send({"type": "http.response.start", "status": error.status,
                "headers": [(b"content-type", b"application/json"),
                            (b"content-length", str(len(body)).encode()), (b"cache-control", b"no-store")]})
    await send({"type": "http.response.body", "body": body})


async def _body(scope: Scope, headers: HeaderView, receive: Receive) -> bytes:
    lengths = headers.get_all("content-length", [])
    if len(lengths) > 1:
        raise AccessError(400, "request_size", "A single Content-Length header is required")
    method = scope.get("method", "GET").upper()
    if method in {"GET", "HEAD", "OPTIONS"}:
        if lengths and lengths[0] != "0":
            raise AccessError(400, "request_size", "A read request must have an empty body")
        if headers.get("transfer-encoding") is not None:
            raise AccessError(400, "request_size", "A read request must have an empty body")
        return b""
    try:
        declared = int(lengths[0]) if lengths else -1
    except ValueError:
        raise AccessError(400, "request_size", "The request size is invalid") from None
    limit = _request_limit(scope.get("path", ""))
    if not 0 <= declared <= limit:
        raise AccessError(413, "request_size", "The request exceeds the allowed size")
    if headers.get("transfer-encoding") is not None:
        raise AccessError(400, "request_size", "Use Content-Length for a signed request")
    chunks = bytearray()
    deadline = time.monotonic() + REQUEST_READ_TIMEOUT_SECONDS
    while True:
        try:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError
            message = await asyncio.wait_for(receive(), remaining)
        except TimeoutError:
            raise AccessError(408, "request_timeout", "The request body timed out") from None
        if message["type"] == "http.disconnect":
            raise AccessError(400, "request_incomplete", "The request body is incomplete")
        if message["type"] != "http.request":
            continue
        chunks.extend(message.get("body", b""))
        if len(chunks) > declared or len(chunks) > limit:
            raise AccessError(413, "request_size", "The request exceeds the allowed size")
        if not message.get("more_body", False):
            break
    if len(chunks) != declared:
        raise AccessError(400, "request_incomplete", "The request body is incomplete")
    return bytes(chunks)


class MultiServerBoundary:
    """Keep local trust unchanged and require device proof on the Serve API."""

    def __init__(self, app: ASGIApp, context: ApiContext) -> None:
        self.app, self.context = app, context

    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return
        path, method = scope.get("path", ""), scope.get("method", "GET").upper()
        headers = HeaderView(scope.get("headers", []))
        forwarded = any(headers.get(name) is not None for name in ("x-forwarded-for", "x-forwarded-host", "x-forwarded-proto"))
        if not forwarded:
            if headers.get("X-Studio-Signature") is not None:
                await _failure(send, AccessError(403, "serve_required", "Signed requests require Tailscale Serve HTTPS"))
                return
            await self.app(scope, receive, send)
            return
        if method == "POST" and path in FEDERATION_PATHS:
            await self.app(scope, receive, send)
            return

        response_started = False

        async def cors_send(message: Message) -> None:
            nonlocal response_started
            if message["type"] == "http.response.start":
                response_started = True
                existing = [(key, value) for key, value in message.get("headers", []) if not key.lower().startswith(b"access-control-")]
                message = {**message, "headers": [*existing, *CORS_HEADERS]}
            await send(message)

        client, listener = scope.get("client"), scope.get("server")
        peer = str(client[0]) if client else ""
        port = int(listener[1]) if listener and listener[1] else self.context.server_port
        origin = self.context.remote.request_origin(headers, peer, port)
        if not origin or not origin.startswith("https://") or scope.get("extensions", {}).get("studio.unix_socket"):
            await _failure(cors_send, AccessError(403, "serve_required", "The Tailscale Serve origin is invalid"))
            return
        if method == "OPTIONS":
            requested = (headers.get("access-control-request-headers") or "").lower().split(",")
            if any(value.strip() and value.strip() not in CORS_REQUEST_HEADERS for value in requested):
                await _failure(cors_send, AccessError(403, "cors_headers", "The preflight requests an unsupported header"))
                return
            await cors_send({"type": "http.response.start", "status": 204, "headers": [
                (b"access-control-allow-methods", b"GET, HEAD, POST, PUT, PATCH, DELETE, OPTIONS"),
                (b"access-control-allow-headers", ", ".join(CORS_REQUEST_HEADERS).encode()),
                (b"access-control-max-age", b"600"), (b"cache-control", b"no-store"),
            ]})
            await cors_send({"type": "http.response.body", "body": b""})
            return
        if not path.startswith("/api/"):
            await self.app(scope, receive, cors_send)
            return
        if method not in {"GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"}:
            await _failure(cors_send, AccessError(405, "method_refused", "The HTTP method is not supported"))
            return
        service = None
        reserved = False
        principal: dict[str, Any] = {}
        try:
            if any(len(headers.get_all(name, [])) != 1 for name in SIGNATURE_HEADERS):
                raise AccessError(401, "invalid_credentials", "A single signed credential header is required")
            raw = await _body(scope, headers, receive)
            target = scope.get("raw_path", path.encode()).decode("ascii")
            query = scope.get("query_string", b"")
            if query:
                target += "?" + query.decode("ascii")
            service = await asyncio.to_thread(service_for, self.context)
            principal = await asyncio.to_thread(service.authenticate, headers, method, target, raw)
            scope["studio_principal"] = principal
            write = method not in {"GET", "HEAD"}
            generic_receipt = write and path != "/api/servers/orchestration"
            decoded: dict[str, Any] = {}
            if write and raw:
                try:
                    decoded = json.loads(raw)
                except ValueError:
                    raise AccessError(400, "invalid_json", "The signed request requires valid JSON") from None
                if not isinstance(decoded, dict):
                    raise AccessError(400, "invalid_json", "The signed request requires a JSON object")
                for name in ("requestId", "request_id"):
                    if name in decoded and decoded[name] != principal["requestId"]:
                        raise AccessError(409, "request_conflict", "The operation ID does not match its signature")
            if generic_receipt:
                saved = await asyncio.to_thread(service.reserve, principal["clientId"], principal["requestId"], method, target, raw)
                if saved is not None:
                    saved_headers = [(name.encode("latin-1"), value.encode("latin-1")) for name, value in saved["headers"]]
                    saved_headers.append((b"x-studio-server", principal["targetServerId"].encode()))
                    await cors_send({"type": "http.response.start", "status": saved["status"], "headers": saved_headers})
                    await cors_send({"type": "http.response.body", "body": base64.b64decode(saved["body"])})
                    return
                reserved = True
            sent_body = False

            async def replay_receive() -> Message:
                nonlocal sent_body
                if sent_body:
                    return await receive()
                sent_body = True
                return {"type": "http.request", "body": raw, "more_body": False}

            async def identified_send(message: Message) -> None:
                if message["type"] == "http.response.start":
                    message = {**message, "headers": [*message.get("headers", []), (b"x-studio-server", principal["targetServerId"].encode())]}
                await cors_send(message)

            if path == "/api/sync/stream":
                assert service is not None
                started = False

                async def stream_send(message: Message) -> None:
                    nonlocal started
                    if message["type"] == "http.response.start":
                        started = True
                    await identified_send(message)

                async def revoked() -> None:
                    while True:
                        await asyncio.sleep(STREAM_RECHECK_SECONDS)
                        try:
                            allowed = await asyncio.to_thread(service.allowed, principal["clientId"])
                        except (OSError, RuntimeError, sqlite3.Error):
                            return
                        if not allowed:
                            return

                application = asyncio.ensure_future(self.app(scope, replay_receive, stream_send))
                watcher = asyncio.create_task(revoked())
                try:
                    finished, _ = await asyncio.wait((application, watcher), return_when=asyncio.FIRST_COMPLETED)
                    if application in finished:
                        await application
                    else:
                        await watcher
                        application.cancel()
                        await asyncio.gather(application, return_exceptions=True)
                        if started:
                            await identified_send({"type": "http.response.body", "body": b"", "more_body": False})
                        else:
                            await _failure(identified_send, AccessError(403, "client_revoked", "The client is revoked"))
                finally:
                    application.cancel()
                    watcher.cancel()
                    await asyncio.gather(application, watcher, return_exceptions=True)
            elif generic_receipt:
                start: Message | None = None
                response_body = bytearray()

                async def save_send(message: Message) -> None:
                    nonlocal start
                    if message["type"] == "http.response.start":
                        start = message
                    elif message["type"] == "http.response.body":
                        response_body.extend(message.get("body", b""))
                        if len(response_body) > MAX_RESPONSE_BYTES:
                            raise AccessError(502, "outcome_unknown", "The response exceeds 1 MiB. Recover the existing operation receipt")

                await self.app(scope, replay_receive, save_send)
                if start is None:
                    raise AccessError(502, "outcome_unknown", "The operation response is missing. Recover its existing receipt")
                response_headers = start.get("headers", [])
                await asyncio.to_thread(service.complete, principal["clientId"], principal["requestId"], start["status"], response_headers,
                                        bytes(response_body), redact_invite=path == "/api/multi-server" and decoded.get("action") == "create_invite")
                await identified_send(start)
                await identified_send({"type": "http.response.body", "body": bytes(response_body)})
            else:
                await self.app(scope, replay_receive, identified_send)
        except AccessError as error:
            if response_started:
                await cors_send({"type": "http.response.body", "body": b"", "more_body": False})
            else:
                await _failure(cors_send, error)
        except (ValueError, OSError, RuntimeError, sqlite3.Error):
            if response_started:
                await cors_send({"type": "http.response.body", "body": b"", "more_body": False})
            else:
                await _failure(cors_send, AccessError(503, "access_unavailable", "The server access boundary is unavailable. Keep the request ID"))
        finally:
            if service is not None and reserved:
                service.abandon(principal["clientId"], principal["requestId"])
