"""FastAPI application assembly and shared API error mapping."""
from __future__ import annotations

import json
import logging
import mimetypes
import sqlite3
from typing import TYPE_CHECKING

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError, ResponseValidationError
from starlette.responses import Response
from starlette.exceptions import HTTPException as StarletteHTTPException

from studio_api.core_models import SessionResponse
from studio_api.middleware import HttpTraceMiddleware, RequestBoundary
from studio_api.multi_server.boundary import MultiServerBoundary
from studio_api.models import ErrorResponse, JsonValue
from studio_api.responses import error_response, install_error_response_docs

STATIC_ALLOWLIST = frozenset({
    "index.html", "studio-sw.js", "studio-renderer.json", "manifest.webmanifest", "apple-touch-icon.png",
    "icon.svg", "icon-192.png", "icon-512.png",
})

if TYPE_CHECKING:
    from studio_api.context import ApiContext


def create_app(context: ApiContext) -> FastAPI:
    """Create the API without opening state or starting background services."""
    app = FastAPI(title="Codex Studio API", version="1")
    app.state.api_context = context
    app.add_middleware(RequestBoundary, context=context)
    app.add_middleware(MultiServerBoundary, context=context)
    # Trace wraps authorization and routing, matching the old handler's
    # begin-before-dispatch and finish-after-response coverage.
    app.add_middleware(HttpTraceMiddleware)

    @app.get("/api/session", response_model=SessionResponse, tags=["core"])
    async def session(request: Request) -> object:
        return context.send(request, {"token": context.token})

    from studio_api.agents.router import create_router as create_agents_router
    from studio_api.accounts.router import create_router as create_accounts_router
    from studio_api.federation.router import create_router as create_federation_router
    from studio_api.multi_server.router import create_router as create_multi_server_router
    from studio_api.history.router import create_router as create_history_router
    from studio_api.insights.router import create_router as create_insights_router
    from studio_api.io.router import create_router as create_io_router
    from studio_api.sync.router import create_router as create_sync_router
    from studio_api.system.router import create_router as create_system_router
    from studio_api.voice.router import create_router as create_voice_router
    from studio_api.work.router import create_router as create_work_router

    for create_router in (
        create_agents_router,
        create_work_router,
        create_accounts_router,
        create_sync_router,
        create_io_router,
        create_insights_router,
        create_history_router,
        create_voice_router,
        create_federation_router,
        create_multi_server_router,
        create_system_router,
    ):
        app.include_router(create_router(context))

    @app.get("/", include_in_schema=False)
    def root(request: Request) -> object:
        return static_file(request, "index.html")

    @app.get("/studio-renderer.json", response_model=JsonValue, include_in_schema=False)
    def renderer_manifest(request: Request) -> object:
        return static_file(request, "studio-renderer.json")

    @app.get("/{relative:path}", include_in_schema=False)
    def static_path(request: Request, relative: str) -> object:
        if relative == "" or relative.startswith("api/") or relative == "api":
            raise StarletteHTTPException(status_code=404, detail="Not found")
        if relative not in STATIC_ALLOWLIST and not relative.startswith("assets/"):
            raise StarletteHTTPException(status_code=404, detail="Not found")
        return static_file(request, relative)

    def static_file(request: Request, relative: str) -> object:
        from codex_canvas import HASHED_ASSET as LEGACY_HASHED_ASSET, WEB, static_content

        if not WEB.is_dir():
            if relative == "index.html":
                raise StarletteHTTPException(
                    status_code=503,
                    detail=(
                        "Build the interface from the repository root with "
                        "`pnpm --filter codex-agents-web run build`."
                    ),
                )
            raise StarletteHTTPException(status_code=404, detail="Not found")
        asset = (WEB / relative).resolve()
        if not asset.is_relative_to(WEB.resolve()) or not asset.is_file():
            raise StarletteHTTPException(status_code=404, detail="Not found")
        if relative not in STATIC_ALLOWLIST and not relative.startswith("assets/"):
            raise StarletteHTTPException(status_code=404, detail="Not found")
        mime = mimetypes.guess_type(asset.name)[0] or "application/octet-stream"
        if asset.suffix == ".webmanifest":
            mime = "application/manifest+json"
        if asset.suffix in {".html", ".js", ".css"}:
            mime += "; charset=utf-8"
        if LEGACY_HASHED_ASSET.fullmatch(relative):
            stat = asset.stat()
            data, compressed = static_content(str(asset), stat.st_mtime_ns, stat.st_size)
            return context.send(request, data, content_type=mime, compressed=compressed,
                                cache_control="private, max-age=31536000, immutable")
        data = asset.read_bytes()
        if relative == "index.html" and context.runtime is not None:
            from studio_api.sync.preferences import bootstrap_preferences
            with context.runtime.db() as db:
                seed = bootstrap_preferences(db, context.workspace_id())
            data = data.replace(b"</head>", seed + b"</head>", 1)
        if relative == "studio-renderer.json":
            return context.send(request, json.loads(data), cache_control="no-store")
        return context.send(request, data, content_type=mime,
                            cache_control="no-cache" if relative == "studio-sw.js" else "no-store")

    @app.exception_handler(RequestValidationError)
    async def validation_error(request: Request, error: RequestValidationError) -> Response:
        request.scope["studio_prehandler_failure"] = True
        problems: list[JsonValue] = [
            {"location": [str(part) for part in row.get("loc", ())], "message": str(row.get("msg", "Invalid request"))}
            for row in error.errors()
        ]
        if request.method == "POST" and request.url.path == "/api/action":
            return context.send(
                request,
                ErrorResponse(error="Invalid request", outcome="not_applied", details=problems),
                status=400,
            )
        return error_response(context, request, "Invalid request", 400, details=problems)

    @app.exception_handler(StarletteHTTPException)
    async def http_error(request: Request, error: StarletteHTTPException) -> Response:
        message = error.detail if isinstance(error.detail, str) else "Request failed"
        return error_response(context, request, message, error.status_code, headers=error.headers)

    @app.exception_handler(ResponseValidationError)
    async def response_error(request: Request, error: ResponseValidationError) -> Response:
        request.scope["studio_outcome_unknown"] = True
        logging.getLogger(__name__).error(
            "Response contract failure for %s %s: %s",
            request.method, request.url.path, str(error.errors())[:4000],
        )
        return error_response(context, request, "The server could not validate its response", 500)

    @app.exception_handler(ValueError)
    @app.exception_handler(RuntimeError)
    @app.exception_handler(OSError)
    @app.exception_handler(sqlite3.Error)
    async def service_error(request: Request, error: Exception) -> Response:
        if isinstance(error, (RuntimeError, OSError, sqlite3.Error)):
            request.scope["studio_outcome_unknown"] = True
        return error_response(context, request, str(error), 400)

    install_error_response_docs(app)
    return app
