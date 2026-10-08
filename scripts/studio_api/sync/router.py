"""Workspace sync and transcript SSE routes."""

from __future__ import annotations

import asyncio
import anyio
import json
import logging
import sqlite3
from collections.abc import AsyncIterator, Sequence
from typing import TYPE_CHECKING, Any, ContextManager, NotRequired, Protocol, TypedDict, cast

from fastapi import APIRouter, Depends, Request
from fastapi.sse import format_sse_event
from fastapi.responses import StreamingResponse
from starlette.concurrency import run_in_threadpool
from starlette.types import Receive, Scope, Send
from pydantic import TypeAdapter, ValidationError

from studio_api.models import ErrorResponse
from studio_api.responses import register_route_components
from studio_api.schema import (
    API_SCHEMA_HASH_HEADER,
    API_SCHEMA_HASH_PARAM,
    API_SCHEMA_MISMATCH_FIELD,
)
from studio_api.sync.models import (
    DraftPushRequest,
    SyncIdentityResponse,
    SyncProtocolResponse,
    SyncDocument,
    SyncEntityPayload,
    SyncPullResponse,
    SyncPullResetResponse,
    SyncStreamQuery,
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
RESOURCE_HEARTBEAT_SECONDS = 15.0
MAX_RESOURCE_QUERY_BYTES = 65_536
MAX_RESOURCE_COUNT = 256
RESOURCE_REFS_ADAPTER: TypeAdapter[list[ResourceRef]] = TypeAdapter(list[ResourceRef])
ERROR_RESPONSES: dict[int | str, dict[str, Any]] = {
    status: {"model": ErrorResponse} for status in (400, 403, 404, 409, 413, 415, 426, 500)
}


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
    def push_drafts(self, rows: list[dict[str, object]]) -> list[dict[str, object]]: ...
    def draft_sequence(self) -> int: ...


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


def _resource_event(
    event: str,
    payload: ResourceChangeEvent | ResourceHeartbeatEvent | ResourceTokenRatesEvent,
) -> bytes:
    return format_sse_event(
        event=event,
        id=str(payload.revision),
        data_str=payload.model_dump_json(by_alias=True, exclude_none=True),
    )


def _schema_event(payload: dict[str, object]) -> bytes:
    return format_sse_event(
        event="api-schema",
        data_str=json.dumps(payload, ensure_ascii=False, separators=(",", ":")),
    )


class ClosingEventStreamResponse(StreamingResponse):
    async def __call__(self, scope: Scope, receive: Receive, send: Send) -> None:
        try:
            await super().__call__(scope, receive, send)
        finally:
            close = getattr(self.body_iterator, "aclose", None)
            if close is not None:
                await close()


def _stream_response(content: AsyncIterator[bytes]) -> StreamingResponse:
    headers = {
        "Cache-Control": "no-cache, no-transform",
        "X-Accel-Buffering": "no",
    }
    return ClosingEventStreamResponse(
        content, media_type="text/event-stream; charset=utf-8", headers=headers
    )


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
            "protocolVersion": 3,
            "supportedVersions": [3],
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
        scope = _first(request, "scope", "state:entities:v1") or "state:entities:v1"
        after = _query_int(request, "after", 0)
        limit = _query_int(request, "limit", SYNC_BATCH_LIMIT)
        initial_high = _query_int(request, "initialHigh", 0)
        fresh_flag = _first(request, "fresh") == "1"
        reset_flag = _first(request, "reset") == "1"
        priority_id = _first(request, "priorityId")
        store = _sync_store(context)
        projection = store.pull(scope, after, limit, fresh_flag, initial_high, reset_flag, priority_id)
        if scope == "state:entities:v1":
            from codex_sync_entities import (
                _report_bad_entity,
                response_entity_payload_fail_open,
                validate_stored_entity_payload,
            )

            valid_documents: list[dict[str, object]] = []
            for document in projection.get("documents", []):
                payload = document.get("payload")
                row_id = str(document.get("id", ""))
                if not isinstance(payload, str):
                    _report_bad_entity("response", row_id, TypeError("payload is not a string"))
                    continue
                was_deleted = bool(document.get("_deleted"))
                if was_deleted:
                    try:
                        envelope = json.loads(payload)
                        if not isinstance(envelope, dict):
                            raise ValueError("invalid entity envelope")
                        collection = envelope.get("collection")
                        entity_id = envelope.get("id")
                        if not isinstance(collection, str) or not isinstance(entity_id, str):
                            raise ValueError("invalid entity envelope")
                        validate_stored_entity_payload(payload, collection, entity_id, True)
                    except (ValueError, TypeError) as error:
                        _report_bad_entity("response", row_id, error)
                        continue
                    # Tombstones carry no DTO value. Never validate or expose
                    # any stale fields that may remain in a deleted row.
                    document["payload"] = json.dumps(
                        {"collection": collection, "id": entity_id, "value": {}},
                        sort_keys=True,
                        separators=(",", ":"),
                        ensure_ascii=False,
                    )
                    document["_deleted"] = True
                    valid_documents.append(document)
                    continue
                try:
                    # SyncStore rejects unreadable envelopes. For readable rows,
                    # strip DTO extras but deliver other mismatches so one stale
                    # row cannot block the checkpoint or later entity updates.
                    (document["payload"], alias_deleted, dropped_paths,
                     remaining_mismatch) = response_entity_payload_fail_open(payload)
                except (ValueError, TypeError) as error:
                    # SyncStore skips unreadable envelopes before they reach this
                    # route; retain the guard for alternate stores and test doubles.
                    _report_bad_entity("response", row_id, error)
                    continue
                document["_deleted"] = was_deleted or alias_deleted
                if dropped_paths:
                    logging.getLogger(__name__).warning(
                        "Sync entity contract extras removed for %s: %s",
                        row_id, ", ".join(dropped_paths),
                    )
                if remaining_mismatch:
                    logging.getLogger(__name__).error(
                        "Sync entity contract mismatch for %s: %s",
                        row_id, remaining_mismatch,
                    )
                valid_documents.append(document)
            if "documents" in projection:
                projection["documents"] = valid_documents
        return context.send(request, {**projection, "generation": store.generation()})

    pull_route = router.routes[-1]
    register_route_components(
        pull_route,
        {
            "SyncEntityPayload": TypeAdapter(SyncEntityPayload).json_schema(
                ref_template="#/components/schemas/{model}"
            ),
        },
    )

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
                                "api-schema": {
                                    "type": "object",
                                    "required": ["hash"],
                                    "properties": {
                                        "hash": {"type": "string"},
                                        API_SCHEMA_MISMATCH_FIELD: {"type": "boolean"},
                                    },
                                },
                            },
                        }
                    }
                },
            },
            **ERROR_RESPONSES,
        },
    )
    async def sync_stream(request: Request, _query: SyncStreamQuery = Depends()) -> StreamingResponse:  # type: ignore[return]
        protocol_value = _first(request, "protocol")
        header_version = request.headers.get("X-Codex-Sync-Protocol")
        if protocol_value not in (None, "3") or header_version not in (None, "3"):
            response = context.send(
                request,
                {"error": "Unsupported sync protocol version", "supportedVersions": [3]},
                status=426,
            )
            return cast(StreamingResponse, response)

        if protocol_value == "3" or header_version == "3":
            renderer_hash = request.headers.get(API_SCHEMA_HASH_HEADER)
            if renderer_hash is None:
                renderer_hash = request.query_params.get(API_SCHEMA_HASH_PARAM)
            server_hash = await context.get_api_schema_hash() if renderer_hash is not None else None
            if renderer_hash is not None and renderer_hash != server_hash:
                async def schema_mismatch_event() -> AsyncIterator[bytes]:
                    payload: dict[str, object] = {
                        "hash": server_hash,
                        API_SCHEMA_MISMATCH_FIELD: True,
                    }
                    yield _schema_event(payload)

                return _stream_response(schema_mismatch_event())
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
                with anyio.CancelScope(shield=True):
                    subscription = await run_in_threadpool(
                        context.resource_hub().subscribe,
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
                shutdown_notifier = request.scope.get("state", {}).get("studio_shutdown_event")
                shutdown_wait = (
                    asyncio.create_task(shutdown_notifier.async_event().wait())
                    if shutdown_notifier is not None else None
                )
                try:
                    if renderer_hash is not None:
                        yield _schema_event({"hash": server_hash})
                    yield _resource_event("resources", subscription.initial)
                    yield _resource_event("token-rates", subscription.initial_token_rates)
                    while not await request.is_disconnected() and not (runtime and runtime.closed):
                        if shutdown_wait is not None:
                            next_event = asyncio.create_task(
                                subscription.next_event(RESOURCE_HEARTBEAT_SECONDS)
                            )
                            done, _pending = await asyncio.wait(
                                (next_event, shutdown_wait),
                                return_when=asyncio.FIRST_COMPLETED,
                            )
                            if shutdown_wait in done:
                                next_event.cancel()
                                await asyncio.gather(next_event, return_exceptions=True)
                                break
                            event = next_event.result()
                        else:
                            event = await subscription.next_event(RESOURCE_HEARTBEAT_SECONDS)
                        if isinstance(event, ResourceTokenRatesEvent):
                            yield _resource_event("token-rates", event)
                        elif isinstance(event, ResourceChangeEvent):
                            yield _resource_event("resources", event)
                        elif event is None:
                            yield _resource_event("heartbeat", subscription.heartbeat())
                finally:
                    # Native file watches can wait for their OS worker to stop.
                    # Keep that wait outside the HTTP event loop, including when
                    # the stream is already cancelled by a disconnected client.
                    with anyio.CancelScope(shield=True):
                        await run_in_threadpool(subscription.close)
                    if shutdown_wait is not None:
                        shutdown_wait.cancel()
                        await asyncio.gather(shutdown_wait, return_exceptions=True)

            return _stream_response(resource_events())

        if protocol_value != "3" and header_version != "3":
            return cast(StreamingResponse, context.send(
                request,
                {"error": "Unsupported sync protocol version", "supportedVersions": [3]},
                status=426,
            ))

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
