"""FastAPI routes for analytics, cost estimates, and worktree disk use."""

from __future__ import annotations

import logging
import math
import re
import sqlite3
from collections.abc import AsyncGenerator, Generator, Iterator
from pathlib import Path
from typing import TYPE_CHECKING, Annotated, Callable, Protocol, cast
from urllib.parse import parse_qs, urlsplit

import anyio
from fastapi import APIRouter, Query, Request
from fastapi.responses import Response, StreamingResponse
from pydantic import ValidationError

from studio_api.insights.models import (
    AccountCostQuery,
    AccountCostResponse,
    AnalyticsQuery,
    AnalyticsResponse,
    SessionCostResponse,
    SessionCostQuery,
    WorktreeDiskQuery,
    WorktreeDiskResponse,
)
from studio_api.models import JsonValue

if TYPE_CHECKING:
    from studio_api.context import ApiContext

ANALYTICS_EXPORT_FILENAME = "codex-studio-analytics.json"
ANALYTICS_CONTENT_TYPE = "application/json; charset=utf-8"
MAX_WORKTREE_QUERY_LENGTH = 24_000
MAX_WORKTREE_IDS = 500
MAX_WORKER_ID_LENGTH = 128
AGENT_ID_PATTERN = r"[A-Za-z0-9._:/-]{1,200}\Z"
_END = object()


class AnalyticsRuntime(Protocol):
    def analytics(self, **options: str) -> dict[str, JsonValue]: ...

    def analytics_export_chunks(
        self, **options: str
    ) -> Generator[bytes, None, None]: ...


class WorktreeScanner(Protocol):
    def snapshot(self, priority_ids: list[str]) -> dict[str, JsonValue]: ...


class CostSnapshotReader(Protocol):
    def snapshot(self, account_key: str) -> dict[str, JsonValue]: ...


class SessionCostSnapshotReader(Protocol):
    def snapshot(self, agent_id: str) -> dict[str, JsonValue]: ...


def _query_first_values(request: Request) -> dict[str, str]:
    """Match the legacy parse_qs contract: first occurrence, blank omitted."""
    return {
        key: values[0]
        for key, values in parse_qs(urlsplit(str(request.url)).query).items()
        if values
    }


def _error(
    context: ApiContext, request: Request, error: BaseException, status: int = 400
) -> Response:
    return context.send(request, {"error": str(error)}, status=status)


def _server_timing(value: JsonValue | None) -> dict[str, float] | None:
    if not isinstance(value, dict):
        return None
    result: dict[str, float] = {}
    for key, duration in value.items():
        if (
            isinstance(key, str)
            and isinstance(duration, (int, float))
            and not isinstance(duration, bool)
            and math.isfinite(duration)
        ):
            result[key] = float(duration)
        else:
            return None
    return result


def _next_chunk(chunks: Iterator[bytes]) -> bytes | object:
    try:
        chunk = next(chunks)
    except StopIteration:
        return _END
    if not isinstance(chunk, bytes):
        raise TypeError("Analytics export produced a non-byte chunk")
    return chunk


async def _stream_export(
    first: bytes, chunks: Generator[bytes, None, None]
) -> AsyncGenerator[bytes, None]:
    """Advance a blocking export off-loop and close it on completion/cancel."""
    try:
        yield first
        while True:
            chunk = await anyio.to_thread.run_sync(_next_chunk, chunks)
            if chunk is _END:
                break
            yield cast(bytes, chunk)
    except (BrokenPipeError, ConnectionResetError):
        return
    except (sqlite3.Error, ValueError, RuntimeError) as error:
        logging.getLogger(__name__).error("Analytics export interrupted: %s", error)
    finally:
        await anyio.to_thread.run_sync(chunks.close)


def _analytics_export(runtime: AnalyticsRuntime, options: dict[str, str]) -> Response:
    """Start the export before committing response headers, as the old server did."""
    chunks = runtime.analytics_export_chunks(**options)
    try:
        first = next(chunks)
        if not isinstance(first, bytes):
            raise TypeError("Analytics export produced a non-byte chunk")
    except BaseException:
        chunks.close()
        raise
    try:
        return StreamingResponse(
            _stream_export(first, chunks),
            status_code=200,
            media_type=ANALYTICS_CONTENT_TYPE,
            headers={
                "Content-Disposition": f'attachment; filename="{ANALYTICS_EXPORT_FILENAME}"',
                "Cache-Control": "no-store",
                "X-Content-Type-Options": "nosniff",
            },
        )
    except BaseException:
        chunks.close()
        raise


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/analytics", response_model=AnalyticsResponse)
    def analytics(
        request: Request, _query_schema: Annotated[AnalyticsQuery, Query()]
    ) -> Response:
        query_values = _query_first_values(request)
        try:
            query = AnalyticsQuery.model_validate(query_values)
        except ValidationError as error:
            return _error(context, request, ValueError(str(error)))

        runtime_value = context.runtime
        if runtime_value is None:
            return _error(context, request, ValueError("Not found"), 404)
        runtime = cast(AnalyticsRuntime, runtime_value)
        try:
            options = query.service_options()
            if query.export == "1":
                return _analytics_export(runtime, options)
            result = runtime.analytics(**options)
            timing = _server_timing(result.pop("__serverTiming", None))
        except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
            return _error(context, request, error)
        return context.send(request, result, server_timing=timing)

    @router.get("/api/costs", response_model=AccountCostResponse)
    def costs(request: Request, _query_schema: Annotated[AccountCostQuery, Query()]) -> Response:
        query = _query_first_values(request)
        account_key = query.get("account_key", "default")
        try:
            reader = cast(CostSnapshotReader, context.costs())
            result = reader.snapshot(account_key)
        except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
            return _error(context, request, error)
        return context.send(request, result)

    @router.get("/api/session-cost", response_model=SessionCostResponse)
    def session_cost(
        request: Request, _query_schema: Annotated[SessionCostQuery, Query()]
    ) -> Response:
        query = _query_first_values(request)
        agent_id = query.get("agent")
        if not agent_id or re.fullmatch(AGENT_ID_PATTERN, agent_id) is None:
            return _error(context, request, ValueError("Select a chat"))
        try:
            reader = cast(SessionCostSnapshotReader, context.session_costs())
            result = reader.snapshot(agent_id)
        except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
            return _error(context, request, error)
        return context.send(request, result)

    @router.get("/api/worktree-disk", response_model=WorktreeDiskResponse)
    def worktree_disk(
        request: Request, _query_schema: Annotated[WorktreeDiskQuery, Query()]
    ) -> Response:
        if context.runtime is None:
            return _error(context, request, ValueError("Not found"), 404)
        values = _query_first_values(request)
        values["workers"] = values.get("workers", "")[:MAX_WORKTREE_QUERY_LENGTH]
        try:
            query = WorktreeDiskQuery.model_validate(values)
        except ValidationError as error:
            return _error(context, request, ValueError(str(error)))
        worker_ids = [
            value
            for value in query.workers.split(",")
            if value and len(value) <= MAX_WORKER_ID_LENGTH
        ][:MAX_WORKTREE_IDS]
        try:
            from codex_worktree_disk import scanner

            get_scanner = cast(Callable[[str | Path], WorktreeScanner], scanner)
            result = get_scanner(context.canvas.root).snapshot(worker_ids)
        except (ValueError, RuntimeError, OSError, sqlite3.Error) as error:
            return _error(context, request, error)
        return context.send(request, result)

    return router
