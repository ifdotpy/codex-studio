"""Workspace sync and transcript SSE routes."""

from __future__ import annotations

import asyncio
import anyio
import json
import logging
import sqlite3
from collections.abc import AsyncIterator, Sequence
from typing import TYPE_CHECKING, Any, ContextManager, Literal, NotRequired, Protocol, TypedDict, cast

from fastapi import APIRouter, Depends, Header, Query, Request
from fastapi.sse import format_sse_event
from fastapi.responses import Response, StreamingResponse
from starlette.concurrency import run_in_threadpool
from starlette.types import Receive, Scope, Send
from pydantic import TypeAdapter, ValidationError

from studio_api.models import SafeIntHeader, SafeQueryInt
from studio_api.sync.preferences import (
    AppearancePreferences,
    PreferencePushRequest,
    PreferencePushResponse,
    merge_preferences,
)
from studio_api.responses import (
    API_BOUNDARY_RESPONSE_HEADERS,
    API_CONTEXT_RESPONSE_HEADERS,
    error_responses,
    register_route_components,
    response_headers,
)
from studio_api.schema import (
    API_SCHEMA_HASH_HEADER,
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
    ApiSchemaEvent,
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

SYNC_BATCH_LIMIT = 100
SYNC_ENTITY_PAGE_LIMIT = 500
SYNC_STREAM_BYTE_LIMIT = 1_048_576
RESOURCE_HEARTBEAT_SECONDS = 15.0
MAX_RESOURCE_QUERY_BYTES = 65_536
MAX_RESOURCE_COUNT = 256
RESOURCE_REFS_ADAPTER: TypeAdapter[list[ResourceRef]] = TypeAdapter(list[ResourceRef])
SYNC_READ_RESPONSES = {
    200: {"headers": {**API_CONTEXT_RESPONSE_HEADERS, **API_BOUNDARY_RESPONSE_HEADERS}},
    **error_responses((400, 403, 500, 503)),
}
SYNC_WRITE_RESPONSES = {
    200: {"headers": {**API_CONTEXT_RESPONSE_HEADERS, **API_BOUNDARY_RESPONSE_HEADERS}},
    **error_responses(
        (400, 403, 408, 409, 413, 415, 426, 500, 503),
        schema_mismatch=True,
    ),
}
SYNC_STREAM_ERRORS = error_responses((400, 403, 426, 500, 503))
_LOGGER = logging.getLogger(__name__)


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
    """Return one scalar query value; clients do not send repeated keys."""
    return request.query_params.get(key, default)


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


def _schema_event(payload: ApiSchemaEvent) -> bytes:
    return format_sse_event(
        event="api-schema",
        data_str=json.dumps(
            payload.model_dump(mode="json", by_alias=True, exclude_unset=True),
            ensure_ascii=False,
            separators=(",", ":"),
        ),
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


    @router.post("/api/sync/preferences", response_model=PreferencePushResponse, responses=SYNC_WRITE_RESPONSES)
    def push_preferences(request: Request, body: PreferencePushRequest) -> object:
        runtime = context.runtime
        if runtime is None:
            raise ValueError("Studio is not ready")
        with runtime.db() as db:
            result = merge_preferences(db, body)
        return context.send(request, result.model_dump(mode="json"))

    preferences_route = router.routes[-1]
    register_route_components(
        preferences_route,
        {
            "AppearancePreferences": AppearancePreferences.model_json_schema(
                ref_template="#/components/schemas/{model}"
            ),
        },
    )


    @router.get("/api/sync/identity", response_model=SyncIdentityResponse, responses=SYNC_READ_RESPONSES)
    def identity(request: Request) -> object:
        return context.send(request, _sync_store(context).identity())

    @router.get("/api/sync/protocol", response_model=SyncProtocolResponse, responses=SYNC_READ_RESPONSES)
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
        responses=SYNC_READ_RESPONSES,
    )
    def pull(
        request: Request,
        scope: str = Query(default="state:entities:v1", min_length=1),
        after: SafeQueryInt = Query(default=0),
        limit: SafeQueryInt = Query(
            default=SYNC_BATCH_LIMIT,
            ge=1,
            le=SYNC_ENTITY_PAGE_LIMIT,
            description="Upper bound; the server may lower it by scope.",
        ),
        fresh: Literal["1"] | None = None,
        initialHigh: SafeQueryInt = Query(default=0),
        reset: Literal["1"] | None = None,
        priorityId: str | None = None,
    ) -> object:
        initial_high = initialHigh
        fresh_flag = fresh == "1"
        reset_flag = reset == "1"
        priority_id = priorityId
        store = _sync_store(context)
        projection = store.pull(scope, after, limit, fresh_flag, initial_high, reset_flag, priority_id)
        if scope == "state:entities:v1":
            from studio_api.sync.entity_response import response_entity_document

            valid_documents = [
                sanitized
                for document in projection.get("documents", [])
                if (sanitized := response_entity_document(document)) is not None
            ]
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
                                {"$ref": "#/components/schemas/ApiSchemaEvent"},
                                {"$ref": "#/components/schemas/ResourceChangeEvent"},
                                {"$ref": "#/components/schemas/ResourceHeartbeatEvent"},
                                {"$ref": "#/components/schemas/ResourceTokenRatesEvent"},
                            ],
                            "x-sse-events": {
                                "api-schema": "ApiSchemaEvent",
                                "resources": "ResourceChangeEvent",
                                "heartbeat": "ResourceHeartbeatEvent",
                                "token-rates": "ResourceTokenRatesEvent",
                            },
                        }
                    }
                },
                "headers": {
                    **API_BOUNDARY_RESPONSE_HEADERS,
                    **response_headers("Cache-Control", "Content-Type", "X-Accel-Buffering"),
                },
            },
            **SYNC_STREAM_ERRORS,
        },
    )
    async def sync_stream(
        request: Request,
        _query: SyncStreamQuery = Depends(),
        protocol: str | None = Query(default=None),
        sync_protocol_header: str | None = Header(
            default=None,
            alias="X-Codex-Sync-Protocol",
        ),
        last_event_id_header: SafeIntHeader | None = Header(
            default=None,
            alias="Last-Event-ID",
        ),
    ) -> Response:
        protocol_value = protocol
        header_version = sync_protocol_header
        if protocol_value not in (None, "3") or header_version not in (None, "3"):
            response = context.send(
                request,
                {"error": "Unsupported sync protocol version", "supportedVersions": [3]},
                status=426,
            )
            return response

        if protocol_value == "3" or header_version == "3":
            renderer_hash = request.headers.get(API_SCHEMA_HASH_HEADER)
            if renderer_hash is None:
                renderer_hash = _query.apiSchema
            server_hash = await context.get_api_schema_hash() if renderer_hash is not None else None
            if renderer_hash is not None and renderer_hash != server_hash:
                async def schema_mismatch_event() -> AsyncIterator[bytes]:
                    payload = ApiSchemaEvent(
                        hash=cast(str, server_hash),
                        mismatch=True,
                    )
                    yield _schema_event(payload)

                return _stream_response(schema_mismatch_event())
            resources_json = _first(request, "resources", "") or ""
            if not resources_json or len(resources_json.encode("utf-8")) > MAX_RESOURCE_QUERY_BYTES:
                return context.send(
                    request, {"error": "Invalid resource subscription"}, status=400
                )
            try:
                raw_resources = json.loads(resources_json)
                resources = RESOURCE_REFS_ADAPTER.validate_python(raw_resources)
                if not resources or len(resources) > MAX_RESOURCE_COUNT:
                    raise ValueError("Invalid resource count")
                last_event_id = last_event_id_header
            except (ValueError, TypeError, ValidationError):
                return context.send(
                    request, {"error": "Invalid resource subscription"}, status=400
                )

            try:
                runtime = context.runtime
                panel_agent_ids = frozenset(
                    resource.root.agentId
                    for resource in resources
                    if isinstance(resource.root, PanelResource)
                )
                if panel_agent_ids and runtime is None:
                    _LOGGER.error(
                        "Sync resource stream unavailable: panel resources requested without a runtime"
                    )
                    return context.send(
                        request, {"error": "Resource stream is unavailable; reconnect to retry."}, status=503
                    )
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
                _LOGGER.exception(
                    "Sync resource stream unavailable while resolving panel activity "
                    "or subscribing to the resource hub"
                )
                return context.send(
                    request, {"error": "Resource stream is unavailable; reconnect to retry."}, status=503
                )

            async def resource_events() -> AsyncIterator[bytes]:
                runtime = context.runtime
                shutdown_notifier = request.scope.get("state", {}).get("studio_shutdown_event")
                shutdown_wait = (
                    asyncio.create_task(shutdown_notifier.async_event().wait())
                    if shutdown_notifier is not None else None
                )
                try:
                    if renderer_hash is not None:
                        yield _schema_event(ApiSchemaEvent(hash=cast(str, server_hash)))
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

        return context.send(
            request,
            {"error": "Unsupported sync protocol version", "supportedVersions": [3]},
            status=426,
        )

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
            "ApiSchemaEvent": ApiSchemaEvent.model_json_schema(
                ref_template="#/components/schemas/{model}"
            ),
        },
    )

    @router.post("/api/sync/drafts", response_model=list[SyncDocument], responses=SYNC_WRITE_RESPONSES)
    def push_drafts(request: Request, body: DraftPushRequest) -> object:
        # The complete nested request is validated by DraftPushRequest before service access.
        workspace = request.headers.get("X-Canvas-Workspace")
        store = _sync_store(context)
        identity = store.identity()
        if workspace != identity["workspaceId"]:
            return context.send(request, {"error": "The server workspace changed. Reload before sending."}, status=409)
        body_rows = body.model_dump(mode="json", exclude_unset=True)["rows"]
        for row in body_rows:
            for state_name in ("newDocumentState", "assumedMasterState"):
                state = row.get(state_name)
                if isinstance(state, dict):
                    state["_deleted"] = state.pop("deleted", False)
        previous_sequence = store.draft_sequence()
        result = store.push_drafts(body_rows)
        if store.draft_sequence() != previous_sequence:
            from studio_api.sync.resources.hub import publish_resources

            publish_resources(context.canvas.root, ResourceRef(DraftsResource(kind="drafts")))
        return context.send(request, result)

    from studio_api.sync.resources.relay.router import create_router as create_resource_notify_router

    router.include_router(create_resource_notify_router(context))
    return router
