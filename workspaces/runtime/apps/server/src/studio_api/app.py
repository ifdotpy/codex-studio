"""FastAPI application assembly and shared API error mapping."""
from __future__ import annotations

import json
import logging
import mimetypes
import re
import sqlite3
import types
from collections.abc import Sequence
from enum import Enum
from typing import TYPE_CHECKING, Annotated, Literal, Union, get_args, get_origin

from fastapi import Depends, FastAPI, Request
from fastapi.exceptions import RequestValidationError, ResponseValidationError
from fastapi.routing import APIRoute
from pydantic import BaseModel
from starlette.responses import Response
from starlette.routing import BaseRoute
from starlette.exceptions import HTTPException as StarletteHTTPException

from studio_api.core_models import SessionResponse
from studio_api.middleware import HttpTraceMiddleware, RequestBoundary
from studio_api.multi_server.boundary import MultiServerBoundary
from studio_api.models import ErrorResponse, JsonValue
from studio_api.responses import api_read_responses, error_response, install_error_response_docs

STATIC_ALLOWLIST = frozenset({
    "index.html", "studio-sw.js", "studio-renderer.json", "manifest.webmanifest", "apple-touch-icon.png",
    "icon.svg", "icon-192.png", "icon-512.png",
})


def route_models(routes: Sequence[BaseRoute]) -> set[type[BaseModel]]:
    """Return Pydantic models reachable from this application's API routes."""
    models: set[type[BaseModel]] = set()
    pending: list[type[BaseModel]] = []

    def collect_type(annotation: object) -> None:
        if isinstance(annotation, type) and issubclass(annotation, BaseModel):
            if annotation not in models:
                models.add(annotation)
                pending.append(annotation)
            return
        for argument in get_args(annotation):
            collect_type(argument)

    def collect_field(field: object) -> None:
        collect_type(getattr(field, "type_", None))
        field_info = getattr(field, "field_info", None)
        collect_type(getattr(field_info, "annotation", None))

    pending_routes = list(routes)
    seen_routes: set[int] = set()
    while pending_routes:
        route = pending_routes.pop()
        if id(route) in seen_routes:
            continue
        seen_routes.add(id(route))
        if not isinstance(route, APIRoute):
            original_router = getattr(route, "original_router", None)
            pending_routes.extend(getattr(original_router, "routes", ()))
            continue

        collect_type(route.response_model)
        for model in getattr(route, "studio_contract_models", ()):
            collect_type(model)
        collect_field(route.body_field)
        for response in route.responses.values():
            if isinstance(response, dict):
                collect_type(response.get("model"))

        dependants = [route.dependant]
        seen_dependants: set[int] = set()
        while dependants:
            dependant = dependants.pop()
            if id(dependant) in seen_dependants:
                continue
            seen_dependants.add(id(dependant))
            dependants.extend(dependant.dependencies)
            for name in (
                "body_params",
                "cookie_params",
                "header_params",
                "path_params",
                "query_params",
            ):
                for field in getattr(dependant, name):
                    collect_field(field)

    while pending:
        model = pending.pop()
        for field in model.model_fields.values():
            collect_type(field.annotation)
    return models

if TYPE_CHECKING:
    from studio_api.context import ApiContext


class QueryContractError(Exception):
    """Raised when a hand-encoded query value fails its route contract."""

    def __init__(self, parameter: str) -> None:
        self.parameter = parameter


def create_app(context: ApiContext) -> FastAPI:
    """Create the API without opening state or starting background services."""
    integer_query_value = re.compile(r"[0-9]+\Z")
    number_query_value = re.compile(
        r"-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?\Z"
    )

    query_parameters: dict[tuple[str, str], dict[str, tuple[bool, frozenset[str]]]] = {}

    def query_types(annotation: object) -> tuple[bool, frozenset[str]]:
        origin = get_origin(annotation)
        args = get_args(annotation)
        if origin is Annotated:
            return query_types(args[0])
        if origin in (Union, types.UnionType):
            variants = [query_types(arg) for arg in args if arg is not type(None)]
            return any(array for array, _ in variants), frozenset().union(*(types_ for _, types_ in variants))
        if origin in (list, tuple, set, frozenset):
            _, item_types = query_types(args[0] if args else str)
            return True, frozenset({"array", *item_types})
        if origin is Literal:
            values = args
            return False, frozenset(
                "boolean" if isinstance(value, bool) else
                "integer" if isinstance(value, int) else
                "number" if isinstance(value, float) else "string"
                for value in values
            )
        if isinstance(annotation, type):
            if issubclass(annotation, BaseModel):
                return False, frozenset()
            if issubclass(annotation, Enum):
                return query_types(type(next(iter(annotation)).value)) if annotation else (False, frozenset({"string"}))
            if issubclass(annotation, bool):
                return False, frozenset({"boolean"})
            if issubclass(annotation, int):
                return False, frozenset({"integer"})
            if issubclass(annotation, float):
                return False, frozenset({"number"})
        return False, frozenset({"string"})

    def add_query_model(target: dict[str, tuple[bool, frozenset[str]]], model: type[BaseModel]) -> None:
        for field_name, field in model.model_fields.items():
            name = field.alias or field_name
            target[name] = query_types(field.annotation)

    def cache_route_queries(routes: Sequence[BaseRoute]) -> None:
        for route in routes:
            if isinstance(route, APIRoute):
                declared = route_query_parameters(route)
                for method in route.methods or ():
                    query_parameters[(route.path, method)] = declared

    def route_query_parameters(route: APIRoute) -> dict[str, tuple[bool, frozenset[str]]]:
        declared: dict[str, tuple[bool, frozenset[str]]] = {}
        pending = [route.dependant]
        seen: set[int] = set()
        while pending:
            dependant = pending.pop()
            if id(dependant) in seen:
                continue
            seen.add(id(dependant))
            pending.extend(dependant.dependencies)
            for field in dependant.query_params:
                annotation = field.field_info.annotation
                if isinstance(annotation, type) and issubclass(annotation, BaseModel):
                    add_query_model(declared, annotation)
                else:
                    declared[field.alias] = query_types(annotation)
        return declared

    def query_contract_problem(request: Request) -> str | None:
        if not request.scope.get("query_string"):
            return None
        route = request.scope.get("route")
        if not isinstance(route, APIRoute) or not route.path.startswith("/api/"):
            return None
        path = route.path
        route_key = (path, request.method)
        declared = query_parameters.get(route_key)
        if declared is None:
            declared = route_query_parameters(route)
            query_parameters[route_key] = declared
        for name in request.query_params.keys():
            parameter = declared.get(name)
            if parameter is None:
                return name
            is_array, value_types = parameter
            values = request.query_params.getlist(name)
            if len(values) > 1 and not is_array:
                return name
            if "integer" in value_types and any(not integer_query_value.fullmatch(value) for value in values):
                return name
            if "number" in value_types and any(not number_query_value.fullmatch(value) for value in values):
                return name
            if "boolean" in value_types and any(value not in {"true", "false"} for value in values):
                return name
        return None

    def validate_query_contract(request: Request) -> None:
        problem = query_contract_problem(request)
        if problem is not None:
            raise QueryContractError(problem)

    app = FastAPI(
        title="Codex Studio API",
        version="1",
        dependencies=[Depends(validate_query_contract)],
    )
    app.state.api_context = context
    app.add_middleware(RequestBoundary, context=context)
    app.add_middleware(MultiServerBoundary, context=context)
    # Trace wraps authorization and routing, matching the old handler's
    # begin-before-dispatch and finish-after-response coverage.
    app.add_middleware(HttpTraceMiddleware)

    @app.get(
        "/api/session",
        response_model=SessionResponse,
        responses=api_read_responses(),
        tags=["core"],
    )
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

    cache_route_queries(app.routes)

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
        problems = error.errors()
        location = [str(part) for part in problems[0].get("loc", ())] if problems else []
        status = 400
        if request.method == "POST" and request.url.path == "/api/action":
            return context.send(
                request,
                ErrorResponse(error="Invalid request", outcome="not_applied", location=location),
                status=status,
            )
        return error_response(context, request, "Invalid request", status, location=location)

    @app.exception_handler(QueryContractError)
    async def query_contract_error(request: Request, error: QueryContractError) -> Response:
        request.scope["studio_prehandler_failure"] = True
        return error_response(
            context,
            request,
            "Invalid request",
            400,
            location=["query", error.parameter],
        )

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
