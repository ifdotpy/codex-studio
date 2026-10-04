"""Workspace sync and transcript SSE routes."""

from __future__ import annotations

import asyncio
import json
import sqlite3
import time
from collections.abc import AsyncIterator, Callable
from typing import TYPE_CHECKING, NotRequired, TypedDict, cast

from fastapi import APIRouter, Depends, Request
from fastapi.responses import StreamingResponse
from starlette.concurrency import run_in_threadpool
from pydantic import ValidationError

from studio_api.models import ErrorResponse
from studio_api.sync.models import (
    DraftPushRequest,
    SyncGenerationState,
    SyncIdentityResponse,
    SyncProtocolResponse,
    SyncDocument,
    SyncPullQuery,
    SyncPullResponse,
    SyncStreamQuery,
    TranscriptStreamQuery,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext

MAX_SAFE_CURSOR = 9_007_199_254_740_991
SYNC_BATCH_LIMIT = 100
SYNC_ENTITY_PAGE_LIMIT = 500
SYNC_STREAM_BYTE_LIMIT = 1_048_576
SYNC_HEARTBEAT_SECONDS = 15.0
SYNC_POLL_SECONDS = 0.25
TRANSCRIPT_COALESCE_SECONDS = 0.08
SHARED_STREAM_POLL_SECONDS = 1.0
ERROR_RESPONSES = {status: {"model": ErrorResponse} for status in (400, 403, 404, 409, 413, 415, 426, 500)}


class SyncStreamBatch(TypedDict):
    kind: str
    documents: NotRequired[list[dict[str, object]]]
    cursor: NotRequired[int]
    floor: NotRequired[int]
    maxSeq: NotRequired[int]
    reason: NotRequired[str]


class TokenRateReader(TypedDict):
    workspace_snapshot: Callable[[], dict[str, object]]


def _first(request: Request, key: str, default: str | None = None) -> str | None:
    values = [value for value in request.query_params.getlist(key) if value != ""]
    return values[0] if values else default


def _query_int(request: Request, key: str, default: int) -> int:
    value = _first(request, key)
    return default if value is None else int(value)


def _event(event: str, payload: object) -> bytes:
    return f"event: {event}\ndata: {json.dumps(payload, ensure_ascii=False)}\n\n".encode()


def _stream_response(content: AsyncIterator[bytes], protocol_v1: bool = False) -> StreamingResponse:
    headers = {
        "Cache-Control": "no-cache, no-transform",
        "X-Accel-Buffering": "no",
    }
    if protocol_v1:
        headers["X-Codex-Sync-Protocol"] = "1"
    return StreamingResponse(content, media_type="text/event-stream; charset=utf-8", headers=headers)


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/sync/identity", response_model=SyncIdentityResponse, responses=ERROR_RESPONSES)
    def identity(request: Request) -> object:
        return context.send(request, context.sync().identity())

    @router.get("/api/sync/protocol", response_model=SyncProtocolResponse, responses=ERROR_RESPONSES)
    def protocol(request: Request) -> object:
        from pathlib import Path

        capabilities = ["pull", "stream", "streamChanges", "entityReset"]
        if (Path(context.canvas.root) / "canvas.sock").exists():
            capabilities.append("unixSocket")
        value = {
            "protocolVersion": 1,
            "supportedVersions": [1, 2],
            "capabilities": capabilities,
            "scopes": ["state:entities:v1", "transcript:<agent-id>", "drafts"],
            "pullEndpoint": "/api/sync/pull",
            "streamEndpoint": "/api/sync/stream",
            "maxEntityPage": SYNC_ENTITY_PAGE_LIMIT,
            "maxOtherPage": SYNC_BATCH_LIMIT,
            "maxStreamDocuments": SYNC_BATCH_LIMIT,
            "maxStreamBytes": SYNC_STREAM_BYTE_LIMIT,
        }
        return context.send(request, value)

    @router.get("/api/sync/pull", response_model=SyncPullResponse, responses=ERROR_RESPONSES)
    def pull(request: Request, _query: SyncPullQuery = Depends()) -> object:
        scope = _first(request, "scope", "state") or "state"
        after = _query_int(request, "after", 0)
        limit = _query_int(request, "limit", SYNC_BATCH_LIMIT)
        initial_high = _query_int(request, "initialHigh", 0)
        fresh = _first(request, "fresh") == "1"
        reset = _first(request, "reset") == "1"
        priority_id = _first(request, "priorityId")
        store = context.sync()
        projection = store.pull(scope, after, limit, fresh, initial_high, reset, priority_id)
        for document in projection.get("documents", []):
            if scope == "state:entities:v1" and not document.get("_deleted"):
                from codex_sync_entities import validate_entity_payload

                try:
                    validate_entity_payload(document["payload"])
                except ValidationError:
                    return context.send(request, {"error": "Invalid sync entity payload"}, status=500)
        return context.send(request, {**projection, "generation": store.generation()})

    @router.get("/api/sync/generations", response_model=SyncGenerationState, responses=ERROR_RESPONSES)
    def generations(request: Request) -> object:
        return context.send(request, context.sync().generation_state())

    @router.get("/api/sync/stream", responses=ERROR_RESPONSES)
    async def sync_stream(request: Request, _query: SyncStreamQuery = Depends()) -> StreamingResponse:
        store = context.sync()
        protocol_value = _first(request, "protocol")
        header_version = request.headers.get("X-Codex-Sync-Protocol")
        if protocol_value not in (None, "1", "2") or header_version not in (None, "1", "2"):
            response = context.send(
                request,
                {"error": "Unsupported sync protocol version", "supportedVersions": [1, 2]},
                status=426,
            )
            return cast(StreamingResponse, response)

        is_v1 = protocol_value == "1" or header_version == "1"
        query_scope = _first(request, "scope", "") or ""
        if is_v1:
            raw_cursor = request.headers.get("Last-Event-ID") or _first(request, "after", "0") or "0"
            try:
                cursor = int(raw_cursor)
                if cursor < 0 or cursor > MAX_SAFE_CURSOR:
                    raise ValueError
                first_batch = cast(
                    SyncStreamBatch,
                    await run_in_threadpool(store.stream_batch, query_scope, cursor),
                )
            except ValueError:
                return cast(StreamingResponse, context.send(request, {"error": "Invalid sync scope or cursor"}, status=400))

            async def legacy_events() -> AsyncIterator[bytes]:
                batch: SyncStreamBatch | None = first_batch
                previous_heartbeat = time.monotonic()
                observed_generation: int | None = None
                current_cursor = cursor
                runtime = context.runtime
                while not await request.is_disconnected() and not (runtime and runtime.closed):
                    if batch is None:
                        if query_scope == "state:entities:v1" or query_scope.startswith("transcript:"):
                            current_generation = await run_in_threadpool(store.generation)
                            if current_generation != observed_generation:
                                await run_in_threadpool(store.pull, query_scope, current_cursor, SYNC_BATCH_LIMIT, False, 0, True)
                                observed_generation = await run_in_threadpool(store.generation)
                        batch = cast(
                            SyncStreamBatch,
                            await run_in_threadpool(store.stream_batch, query_scope, current_cursor),
                        )
                    kind = batch["kind"]
                    if kind in ("reset", "cursor-ahead"):
                        payload = {
                            "protocolVersion": 1,
                            "workspaceId": store.identity()["workspaceId"],
                            "scope": query_scope,
                            **batch,
                        }
                        yield _event("reset" if kind == "reset" else "cursor-ahead", payload)
                        return
                    if kind == "changes":
                        body = {
                            "protocolVersion": 1,
                            "workspaceId": store.identity()["workspaceId"],
                            "scope": query_scope,
                            "documents": batch["documents"],
                            "cursor": batch["cursor"],
                        }
                        encoded = json.dumps(body, ensure_ascii=False, separators=(",", ":"))
                        if len(encoded.encode("utf-8")) > SYNC_STREAM_BYTE_LIMIT:
                            reset_payload = {
                                "protocolVersion": 1,
                                "workspaceId": store.identity()["workspaceId"],
                                "scope": query_scope,
                                "kind": "reset",
                                "reason": "event-too-large",
                                "floor": batch.get("floor", 0),
                                "maxSeq": batch["maxSeq"],
                            }
                            yield _event("reset", reset_payload)
                            return
                        current_cursor = batch["cursor"]
                        yield f"id: {current_cursor}\nevent: changes\ndata: {encoded}\n\n".encode()
                    elif time.monotonic() - previous_heartbeat >= SYNC_HEARTBEAT_SECONDS:
                        yield b": heartbeat\n\n"
                        previous_heartbeat = time.monotonic()
                    batch = None
                    await asyncio.sleep(SYNC_POLL_SECONDS)

            return _stream_response(legacy_events(), protocol_v1=True)

        shared_stream = protocol_value == "2"
        entity_stream = query_scope == "state:entities:v1"
        draft_stream = query_scope == "drafts"
        transcript_id = (
            query_scope[len("transcript:") :]
            if query_scope.startswith("transcript:") and len(query_scope) < 300
            else None
        )

        async def events() -> AsyncIterator[bytes]:
            previous: object = None
            observed_generation: int | None = None
            rate_previous: object = object()
            runtime = context.runtime
            while not await request.is_disconnected() and not (runtime and runtime.closed):
                if entity_stream or transcript_id is not None:
                    current_generation = await run_in_threadpool(store.generation)
                    if current_generation != observed_generation:
                        stream_scope = "state:entities:v1" if entity_stream else f"transcript:{transcript_id}"
                        await run_in_threadpool(store.pull, stream_scope, 0, SYNC_BATCH_LIMIT, False, 0, True)
                        observed_generation = await run_in_threadpool(store.generation)
                if shared_stream:
                    current: object = await run_in_threadpool(store.generation_state)
                elif entity_stream:
                    current = await run_in_threadpool(store.entity_sequence)
                elif draft_stream:
                    current = await run_in_threadpool(store.draft_sequence)
                elif transcript_id is not None:
                    current = await run_in_threadpool(store.transcript_revision, transcript_id)
                else:
                    current = await run_in_threadpool(store.generation)
                if current is None:
                    current = await run_in_threadpool(store.generation)
                if current != previous:
                    data = json.dumps(current) if shared_stream or entity_stream or draft_stream or transcript_id is not None else '"RESYNC"'
                    yield f"data: {data}\n\n".encode()
                else:
                    yield b": heartbeat\n\n"
                if shared_stream and runtime:
                    from codex_token_rate import token_rates

                    rate_reader_factory = cast(Callable[[object], TokenRateReader], token_rates)
                    rates = await run_in_threadpool(rate_reader_factory(runtime)["workspace_snapshot"])
                    if rates != rate_previous:
                        generation = cast(dict[str, object], current)
                        payload = {**rates, "protocol": 2, "workspaceId": generation["workspaceId"]}
                        yield f"event: token-rates\ndata: {json.dumps(payload)}\n\n".encode()
                        rate_previous = rates
                previous = current
                await asyncio.sleep(SHARED_STREAM_POLL_SECONDS)

        return _stream_response(events())

    @router.get("/api/transcript/stream", responses=ERROR_RESPONSES)
    async def transcript_stream(request: Request, _query: TranscriptStreamQuery = Depends()) -> StreamingResponse:
        runtime = context.runtime
        if runtime is None:
            return cast(StreamingResponse, context.send(request, {"error": "Not found"}, status=404))
        agent_id = _first(request, "id", "") or ""
        await run_in_threadpool(runtime.transcript, agent_id)

        async def transcript_events() -> AsyncIterator[bytes]:
            revision: int | None = -1
            previous: dict[str, dict[str, object]] = {}
            version = 0
            previous_order: list[str] | None = None
            try:
                while not await request.is_disconnected() and not runtime.closed:
                    current, data = await run_in_threadpool(runtime.wait_transcript, agent_id, revision)
                    if current is None:
                        return
                    if data is None:
                        yield b": heartbeat\n\n"
                    else:
                        records = {item["id"]: item for item in data.pop("items")}
                        order = list(records)
                        changed: list[dict[str, object]] = []
                        for item_id, item in records.items():
                            old = previous.get(item_id)
                            if old == item:
                                continue
                            old_text = old.get("text") if old else None
                            new_text = item.get("text")
                            old_rest = {key: value for key, value in old.items() if key != "text"} if old else {}
                            new_rest = {key: value for key, value in item.items() if key != "text"}
                            if (isinstance(old_text, str) and isinstance(new_text, str)
                                    and new_text.startswith(old_text) and old_rest == new_rest):
                                changed.append({"id": item_id, "append": new_text[len(old_text):]})
                            else:
                                changed.append({"id": item_id, "replace": item})
                        version += 1
                        payload = {**data, "version": version, "replace": revision == -1, "items": changed}
                        if revision == -1 or order != previous_order:
                            payload["order"] = order
                        yield f"data: {json.dumps(payload, ensure_ascii=False)}\n\n".encode()
                        revision, previous, previous_order = current, records, order
                    await asyncio.sleep(TRANSCRIPT_COALESCE_SECONDS)
            except (OSError, ValueError, RuntimeError, sqlite3.Error) as error:
                yield _event("unavailable", {"error": str(error)})

        return _stream_response(transcript_events())

    @router.post("/api/sync/drafts", response_model=list[SyncDocument], responses=ERROR_RESPONSES)
    def push_drafts(request: Request, body: DraftPushRequest) -> object:
        # The complete nested request is validated by DraftPushRequest before service access.
        workspace = request.headers.get("X-Canvas-Workspace")
        identity = context.sync().identity()
        if workspace != identity["workspaceId"]:
            return context.send(request, {"error": "The server workspace changed. Reload before sending."}, status=409)
        body_rows = body.model_dump(mode="json", by_alias=True, exclude_unset=True)["rows"]
        return context.send(request, context.sync().push_drafts(body_rows))

    return router
