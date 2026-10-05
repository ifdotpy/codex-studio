"""Workspace sync and transcript SSE routes."""

from __future__ import annotations

import asyncio
import json
import sqlite3
import time
from collections.abc import AsyncIterator, Callable, Sequence
from typing import TYPE_CHECKING, Any, ContextManager, NotRequired, Protocol, TypedDict, cast

from fastapi import APIRouter, Depends, Request
from fastapi.sse import format_sse_event
from fastapi.responses import StreamingResponse
from starlette.concurrency import run_in_threadpool
from pydantic import TypeAdapter, ValidationError

from studio_api.models import ErrorResponse
from studio_api.responses import register_route_components
from studio_api.sync.models import (
    DraftPushRequest,
    SyncGenerationState,
    SyncIdentityResponse,
    SyncProtocolResponse,
    SyncDocument,
    SyncPullResponse,
    SyncPullResetResponse,
    SyncStreamQuery,
    TranscriptStreamQuery,
)
from studio_api.sync.resources.models import (
    DraftsResource,
    ResourceChangeEvent,
    ResourceHeartbeatEvent,
    PanelResource,
    ResourceRef,
    ResourceTokenRatesEvent,
    TranscriptResource,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext

MAX_SAFE_CURSOR = 9_007_199_254_740_991
SYNC_BATCH_LIMIT = 100
SYNC_ENTITY_PAGE_LIMIT = 500
SYNC_STREAM_BYTE_LIMIT = 1_048_576
SYNC_HEARTBEAT_SECONDS = 15.0
RESOURCE_HEARTBEAT_SECONDS = 15.0
MAX_RESOURCE_QUERY_BYTES = 65_536
MAX_RESOURCE_COUNT = 256
RESOURCE_REFS_ADAPTER: TypeAdapter[list[ResourceRef]] = TypeAdapter(list[ResourceRef])
SYNC_POLL_SECONDS = 0.25
TRANSCRIPT_COALESCE_SECONDS = 0.08
SHARED_STREAM_POLL_SECONDS = 1.0
ERROR_RESPONSES: dict[int | str, dict[str, Any]] = {
    status: {"model": ErrorResponse} for status in (400, 403, 404, 409, 413, 415, 426, 500)
}


class SyncStreamBatch(TypedDict):
    kind: str
    documents: NotRequired[list[dict[str, object]]]
    cursor: NotRequired[int]
    floor: NotRequired[int]
    maxSeq: NotRequired[int]
    reason: NotRequired[str]


class SyncPullProjection(TypedDict):
    workspaceId: str
    documents: NotRequired[list[dict[str, object]]]
    checkpoint: NotRequired[dict[str, object]]
    reset: NotRequired[bool]
    floor: NotRequired[int]
    maxSeq: NotRequired[int]
    initialHigh: NotRequired[int]


class SyncStoreContract(Protocol):
    def identity(self) -> dict[str, object]: ...
    def pull(
        self, scope: str, after: int = 0, limit: int = 100, fresh: bool = False,
        initial_high: int = 0, reset_support: bool = False, priority_id: str | None = None,
    ) -> SyncPullProjection: ...
    def generation(self) -> int: ...
    def generation_state(self) -> dict[str, object]: ...
    def stream_batch(self, scope: str, after: int, limit: int = 100) -> SyncStreamBatch: ...
    def entity_sequence(self) -> int: ...
    def draft_sequence(self) -> int: ...
    def transcript_revision(self, agent: str) -> int | None: ...
    def push_drafts(self, rows: list[dict[str, object]]) -> list[dict[str, object]]: ...


class TokenRateReader(Protocol):
    def workspace_snapshot(self) -> dict[str, object]: ...


class PanelAgentValidator(Protocol):
    def db(self) -> ContextManager[sqlite3.Connection]: ...
    def checked_actor(self, db: sqlite3.Connection, agent_id: str) -> object: ...


def _active_panel_agents(runtime: PanelAgentValidator, resources: Sequence[ResourceRef]) -> frozenset[str]:
    """Validate panel identities before a native watcher can create their directory."""
    agents = {
        resource.root.agentId
        for resource in resources
        if isinstance(resource.root, PanelResource)
    }
    active: set[str] = set()
    if not agents:
        return frozenset()
    with runtime.db() as db:
        for agent_id in agents:
            try:
                runtime.checked_actor(db, agent_id)
            except ValueError:
                # Keep stale refs in the baseline so the client's targeted read
                # can observe the normal not-found result; simply don't watch them.
                continue
            active.add(agent_id)
    return frozenset(active)


def _sync_store(context: ApiContext) -> SyncStoreContract:
    # SyncStore remains a legacy untyped service; keep that boundary explicit.
    return cast(SyncStoreContract, context.sync())


def _first(request: Request, key: str, default: str | None = None) -> str | None:
    values = [value for value in request.query_params.getlist(key) if value != ""]
    return values[0] if values else default


def _query_int(request: Request, key: str, default: int) -> int:
    value = _first(request, key)
    return default if value is None else int(value)


def _event(event: str, payload: object) -> bytes:
    return format_sse_event(event=event, data_str=json.dumps(payload, ensure_ascii=False))


def _legacy_message(payload: object) -> bytes:
    """Keep transcript updates on EventSource's default `message` channel."""
    return format_sse_event(data_str=json.dumps(payload, ensure_ascii=False))


def _resource_event(
    event: str,
    payload: ResourceChangeEvent | ResourceHeartbeatEvent | ResourceTokenRatesEvent,
) -> bytes:
    return format_sse_event(
        event=event,
        id=str(payload.revision),
        data_str=payload.model_dump_json(by_alias=True),
    )


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
        return context.send(request, _sync_store(context).identity())

    @router.get("/api/sync/protocol", response_model=SyncProtocolResponse, responses=ERROR_RESPONSES)
    def protocol(request: Request) -> object:
        from pathlib import Path

        capabilities = ["pull", "stream", "streamChanges", "entityReset", "typedResources", "tokenRates"]
        if (Path(context.canvas.root) / "canvas.sock").exists():
            capabilities.append("unixSocket")
        value = {
            "protocolVersion": 1,
            "supportedVersions": [1, 2, 3],
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

    @router.get(
        "/api/sync/pull",
        response_model=SyncPullResponse | SyncPullResetResponse,
        responses=ERROR_RESPONSES,
    )
    def pull(
        request: Request,
        scope: str | None = None,
        after: int | None = None,
        limit: int | None = None,
        fresh: str | None = None,
        initialHigh: int | None = None,
        reset: str | None = None,
        priorityId: str | None = None,
    ) -> object:
        scope = _first(request, "scope", "state") or "state"
        after = _query_int(request, "after", 0)
        limit = _query_int(request, "limit", SYNC_BATCH_LIMIT)
        initial_high = _query_int(request, "initialHigh", 0)
        fresh_flag = _first(request, "fresh") == "1"
        reset_flag = _first(request, "reset") == "1"
        priority_id = _first(request, "priorityId")
        store = _sync_store(context)
        projection = store.pull(scope, after, limit, fresh_flag, initial_high, reset_flag, priority_id)
        for document in projection.get("documents", []):
            if scope == "state:entities:v1" and not document.get("_deleted"):
                from codex_sync_entities import validate_entity_payload

                payload = document.get("payload")
                if not isinstance(payload, str):
                    return context.send(request, {"error": "Invalid sync entity payload"}, status=500)
                try:
                    validate_entity_payload(payload)
                except ValidationError:
                    return context.send(request, {"error": "Invalid sync entity payload"}, status=500)
        return context.send(request, {**projection, "generation": store.generation()})

    @router.get("/api/sync/generations", response_model=SyncGenerationState, responses=ERROR_RESPONSES)
    def generations(request: Request) -> object:
        return context.send(request, _sync_store(context).generation_state())

    @router.get(
        "/api/sync/stream",
        response_class=StreamingResponse,
        response_model=None,
        responses={
            200: {
                "description": "Server-sent sync updates",
                "content": {
                    "text/event-stream": {
                        "schema": {
                            "oneOf": [
                                {"$ref": "#/components/schemas/ResourceChangeEvent"},
                                {"$ref": "#/components/schemas/ResourceHeartbeatEvent"},
                                {"$ref": "#/components/schemas/ResourceTokenRatesEvent"},
                            ],
                            "x-sse-events": {
                                "resources": "ResourceChangeEvent",
                                "heartbeat": "ResourceHeartbeatEvent",
                                "token-rates": "ResourceTokenRatesEvent",
                            },
                        }
                    }
                },
            },
            **ERROR_RESPONSES,
        },
    )
    async def sync_stream(request: Request, _query: SyncStreamQuery = Depends()) -> StreamingResponse:
        store = _sync_store(context)
        protocol_value = _first(request, "protocol")
        header_version = request.headers.get("X-Codex-Sync-Protocol")
        if protocol_value not in (None, "1", "2", "3") or header_version not in (None, "1", "2", "3"):
            response = context.send(
                request,
                {"error": "Unsupported sync protocol version", "supportedVersions": [1, 2, 3]},
                status=426,
            )
            return cast(StreamingResponse, response)

        if protocol_value and header_version and protocol_value != header_version:
            return cast(StreamingResponse, context.send(
                request,
                {"error": "Conflicting sync protocol versions", "supportedVersions": [1, 2, 3]},
                status=426,
            ))

        if protocol_value == "3" or header_version == "3":
            resources_json = _first(request, "resources", "") or ""
            if not resources_json or len(resources_json.encode("utf-8")) > MAX_RESOURCE_QUERY_BYTES:
                return cast(StreamingResponse, context.send(
                    request, {"error": "Invalid resource subscription"}, status=400
                ))
            try:
                raw_resources = json.loads(resources_json)
                resources = RESOURCE_REFS_ADAPTER.validate_python(raw_resources)
                if not resources or len(resources) > MAX_RESOURCE_COUNT:
                    raise ValueError("Invalid resource count")
                last_event_id = request.headers.get("Last-Event-ID")
                if last_event_id is not None:
                    if int(last_event_id) < 0 or int(last_event_id) > MAX_SAFE_CURSOR:
                        raise ValueError("Invalid last event ID")
            except (ValueError, TypeError, ValidationError):
                return cast(StreamingResponse, context.send(
                    request, {"error": "Invalid resource subscription"}, status=400
                ))

            try:
                runtime = context.runtime
                panel_agent_ids = frozenset(
                    resource.root.agentId
                    for resource in resources
                    if isinstance(resource.root, PanelResource)
                )
                if panel_agent_ids and runtime is None:
                    return cast(StreamingResponse, context.send(
                        request, {"error": "Resource stream is unavailable; reconnect to retry."}, status=503
                    ))
                active_panel_agents = (
                    await run_in_threadpool(_active_panel_agents, runtime, resources)
                    if runtime is not None and panel_agent_ids else None
                )
                subscription = context.resource_hub().subscribe(
                    resources,
                    loop=asyncio.get_running_loop(),
                    reconnect=last_event_id is not None,
                    progress_agent_ids=active_panel_agents,
                )
            except (OSError, RuntimeError, ValueError):
                return cast(StreamingResponse, context.send(
                    request, {"error": "Resource stream is unavailable; reconnect to retry."}, status=503
                ))

            async def resource_events() -> AsyncIterator[bytes]:
                runtime = context.runtime
                try:
                    yield _event("api-schema", {"hash": context.api_schema_hash})
                    yield _resource_event("resources", subscription.initial)
                    yield _resource_event("token-rates", subscription.initial_token_rates)
                    while not await request.is_disconnected() and not (runtime and runtime.closed):
                        event = await subscription.next_event(RESOURCE_HEARTBEAT_SECONDS)
                        if isinstance(event, ResourceTokenRatesEvent):
                            yield _resource_event("token-rates", event)
                        elif isinstance(event, ResourceChangeEvent):
                            yield _resource_event("resources", event)
                        elif event is None:
                            yield _resource_event("heartbeat", subscription.heartbeat())
                finally:
                    subscription.close()

            return _stream_response(resource_events())

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
                        identity = await run_in_threadpool(store.identity)
                        payload = {
                            "protocolVersion": 1,
                            "workspaceId": identity["workspaceId"],
                            "scope": query_scope,
                            **batch,
                        }
                        yield _event("reset" if kind == "reset" else "cursor-ahead", payload)
                        return
                    if kind == "changes":
                        identity = await run_in_threadpool(store.identity)
                        workspace_id = identity["workspaceId"]
                        body = {
                            "protocolVersion": 1,
                            "workspaceId": workspace_id,
                            "scope": query_scope,
                            "documents": batch["documents"],
                            "cursor": batch["cursor"],
                        }
                        encoded = json.dumps(body, ensure_ascii=False, separators=(",", ":"))
                        if len(encoded.encode("utf-8")) > SYNC_STREAM_BYTE_LIMIT:
                            reset_payload = {
                                "protocolVersion": 1,
                                "workspaceId": workspace_id,
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
                    rates = await run_in_threadpool(rate_reader_factory(runtime).workspace_snapshot)
                    if rates != rate_previous:
                        generation = cast(dict[str, object], current)
                        payload = {**rates, "protocol": 2, "workspaceId": generation["workspaceId"]}
                        yield f"event: token-rates\ndata: {json.dumps(payload)}\n\n".encode()
                        rate_previous = rates
                previous = current
                await asyncio.sleep(SHARED_STREAM_POLL_SECONDS)

        return _stream_response(events())

    resource_event_route = router.routes[-1]
    register_route_components(
        resource_event_route,
        {
            "ResourceRef": ResourceRef.model_json_schema(ref_template="#/components/schemas/{model}"),
            "ResourceChangeEvent": ResourceChangeEvent.model_json_schema(
                ref_template="#/components/schemas/{model}"
            ),
            "ResourceHeartbeatEvent": ResourceHeartbeatEvent.model_json_schema(
                ref_template="#/components/schemas/{model}"
            ),
            "ResourceTokenRatesEvent": ResourceTokenRatesEvent.model_json_schema(
                ref_template="#/components/schemas/{model}"
            ),
        },
    )

    @router.get(
        "/api/transcript/stream",
        response_class=StreamingResponse,
        response_model=None,
        responses={
            200: {
                "description": "Server-sent transcript updates",
                "content": {"text/event-stream": {"schema": {"type": "string"}}},
            },
            **ERROR_RESPONSES,
        },
    )
    async def transcript_stream(request: Request, _query: TranscriptStreamQuery = Depends()) -> StreamingResponse:
        runtime = context.runtime
        if runtime is None:
            return cast(StreamingResponse, context.send(request, {"error": "Not found"}, status=404))
        agent_id = _first(request, "id", "") or ""
        subscription = None
        try:
            subscription = context.resource_hub().subscribe(
                [ResourceRef(TranscriptResource(kind="transcript", agentId=agent_id))],
                loop=asyncio.get_running_loop(),
            )
            initial_data = await run_in_threadpool(runtime.transcript, agent_id)
        except BaseException as error:
            if subscription is not None:
                subscription.close()
            if not isinstance(error, (OSError, ValueError, RuntimeError, sqlite3.Error)):
                raise
            return cast(StreamingResponse, context.send(
                request, {"error": str(error) or "Transcript stream is unavailable"}, status=503
            ))

        async def transcript_events() -> AsyncIterator[bytes]:
            previous: dict[str, dict[str, object]] = {}
            version = 0
            previous_order: list[str] | None = None
            try:
                data = dict(initial_data)
                records = {item["id"]: item for item in data.pop("items")}
                previous_metadata = data
                previous = records
                previous_order = list(records)
                version += 1
                yield _legacy_message({
                    **data, "version": version, "replace": True,
                    "items": [{"id": item_id, "replace": item} for item_id, item in records.items()],
                    "order": previous_order,
                })
                while not await request.is_disconnected() and not runtime.closed:
                    event = await subscription.next_event(RESOURCE_HEARTBEAT_SECONDS)
                    if event is None:
                        yield b": heartbeat\n\n"
                        continue
                    if isinstance(event, ResourceTokenRatesEvent):
                        continue
                    data = await run_in_threadpool(runtime.transcript, agent_id)
                    records = {item["id"]: item for item in data.pop("items")}
                    metadata_changed = data != previous_metadata
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
                    removed = [item_id for item_id in previous if item_id not in records]
                    if not changed and not removed and order == previous_order and not metadata_changed:
                        previous = records
                        continue
                    version += 1
                    payload = {
                        **data,
                        "version": version,
                        "replace": False,
                        "items": changed,
                        **({"removed": removed} if removed else {}),
                    }
                    if order != previous_order:
                        payload["order"] = order
                    yield _legacy_message(payload)
                    previous, previous_order, previous_metadata = records, order, data
            except (OSError, ValueError, RuntimeError, sqlite3.Error) as error:
                yield _event("unavailable", {"error": str(error)})
            finally:
                subscription.close()

        return _stream_response(transcript_events())

    @router.post("/api/sync/drafts", response_model=list[SyncDocument], responses=ERROR_RESPONSES)
    def push_drafts(request: Request, body: DraftPushRequest) -> object:
        # The complete nested request is validated by DraftPushRequest before service access.
        workspace = request.headers.get("X-Canvas-Workspace")
        store = _sync_store(context)
        identity = store.identity()
        if workspace != identity["workspaceId"]:
            return context.send(request, {"error": "The server workspace changed. Reload before sending."}, status=409)
        body_rows = body.model_dump(mode="json", by_alias=True, exclude_unset=True)["rows"]
        previous_sequence = store.draft_sequence()
        result = store.push_drafts(body_rows)
        if store.draft_sequence() != previous_sequence:
            from studio_api.sync.resources.hub import publish_resources

            publish_resources(context.canvas.root, ResourceRef(DraftsResource(kind="drafts")))
        return context.send(request, result)

    from studio_api.sync.resources.relay.router import create_router as create_resource_notify_router

    router.include_router(create_resource_notify_router(context))
    return router
