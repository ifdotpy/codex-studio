"""FastAPI routes for history and checkpoint operations."""

from __future__ import annotations

from typing import TYPE_CHECKING, Annotated, Callable, Protocol, TypeVar, cast

from fastapi import APIRouter, Depends, Request
from starlette.responses import Response

from studio_api.models import ContractModel, JsonValue
from studio_api.history.models import (
    BranchRequest,
    BranchResponse,
    CheckpointCaptureRequest,
    CheckpointCaptureResponse,
    CheckpointPreviewRequest,
    CheckpointPreviewResponse,
    CheckpointRestoreRequest,
    CheckpointRestoreResponse,
    CheckpointsQuery,
    CheckpointsResponse,
    SearchItemResponse,
    SearchQuery,
    SearchResponse,
    TranscriptItemQuery,
    TranscriptItemResponse,
    TranscriptPageResponse,
    TranscriptQuery,
    TranscriptSearchQuery,
    TranscriptSearchResponse,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext

QueryContract = TypeVar("QueryContract", bound=ContractModel)


class TranscriptCanvas(Protocol):
    def transcript(self, agent: str | None) -> JsonValue: ...


def _parse_query(request: Request, model: type[QueryContract]) -> QueryContract:
    """Validate first nonblank query values, matching urllib.parse_qs behavior."""
    allowed = model.model_fields
    values: dict[str, str] = {}
    for name, value in request.query_params.multi_items():
        if name in allowed and value != "" and name not in values:
            values[name] = value
    return model.model_validate(values)


def _query_openapi(model: type[ContractModel]) -> dict[str, list[dict[str, object]]]:
    """Expose dependency query models using their Pydantic field schemas."""
    schema = model.model_json_schema(mode="validation")
    properties = schema.get("properties", {})
    required = set(schema.get("required", []))
    return {
        "parameters": [
            {
                "name": name,
                "in": "query",
                "required": name in required,
                "schema": field_schema,
            }
            for name, field_schema in properties.items()
        ]
    }


def _transcript_query(request: Request) -> TranscriptQuery:
    return _parse_query(request, TranscriptQuery)


def _transcript_item_query(request: Request) -> TranscriptItemQuery:
    return _parse_query(request, TranscriptItemQuery)


def _transcript_search_query(request: Request) -> TranscriptSearchQuery:
    return _parse_query(request, TranscriptSearchQuery)


def _search_query(request: Request) -> SearchQuery:
    return _parse_query(request, SearchQuery)


def _checkpoints_query(request: Request) -> CheckpointsQuery:
    return _parse_query(request, CheckpointsQuery)


def _history_item(runtime: object, agent: str | None, message_id: str | None) -> JsonValue:
    from codex_transcript_history import history_item

    service = cast(Callable[[object, str | None, str | None], JsonValue], history_item)
    return service(runtime, agent, message_id)


def _search_history(runtime: object, agent: str | None, query: str | None, limit: str) -> JsonValue:
    from codex_transcript_history import search_history

    service = cast(Callable[[object, str | None, str | None, str], JsonValue], search_history)
    return service(runtime, agent, query, limit)


def _canvas_transcript(canvas: object, agent: str | None) -> JsonValue:
    return cast(TranscriptCanvas, canvas).transcript(agent)


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/transcript/page", response_model=TranscriptPageResponse,
                openapi_extra=_query_openapi(TranscriptQuery))
    def transcript_page(
        request: Request,
        query: Annotated[TranscriptQuery, Depends(_transcript_query)],
    ) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        limit = query.limit or "120"
        return context.send(
            request,
            runtime.transcript(
                query.id,
                before=query.before,
                around=query.around,
                after=query.after,
                limit=limit,
            )
        )

    @router.get("/api/transcript/item", response_model=TranscriptItemResponse,
                openapi_extra=_query_openapi(TranscriptItemQuery))
    def transcript_item(
        request: Request,
        query: Annotated[TranscriptItemQuery, Depends(_transcript_item_query)],
    ) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(request, _history_item(runtime, query.id, query.message_id))

    @router.get("/api/transcript/search", response_model=TranscriptSearchResponse,
                openapi_extra=_query_openapi(TranscriptSearchQuery))
    def transcript_search(
        request: Request,
        query: Annotated[TranscriptSearchQuery, Depends(_transcript_search_query)],
    ) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(
            request,
            _search_history(runtime, query.id, query.q, query.limit or "100")
        )

    @router.get("/api/search/item", response_model=SearchItemResponse,
                openapi_extra=_query_openapi(TranscriptItemQuery))
    def search_item(
        request: Request,
        query: Annotated[TranscriptItemQuery, Depends(_transcript_item_query)],
    ) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(request, runtime.search_item(query.id))

    @router.get("/api/search", response_model=SearchResponse,
                openapi_extra=_query_openapi(SearchQuery))
    def search(request: Request, query: Annotated[SearchQuery, Depends(_search_query)]) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(
            request,
            runtime.search_work(query.q, limit=query.limit or "50")
        )

    @router.get("/api/checkpoints", response_model=CheckpointsResponse,
                openapi_extra=_query_openapi(CheckpointsQuery))
    def checkpoints(
        request: Request,
        query: Annotated[CheckpointsQuery, Depends(_checkpoints_query)],
    ) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        value = runtime.workspace_snapshot(query.agent)
        return context.send(request, {"checkpoints": value["checkpoints"]}, etag=True)

    @router.get("/api/transcript", response_model=TranscriptPageResponse,
                openapi_extra=_query_openapi(TranscriptQuery))
    def transcript(
        request: Request,
        query: Annotated[TranscriptQuery, Depends(_transcript_query)],
    ) -> Response:
        agent = query.id if query.id is not None else ""
        return context.send(request, _canvas_transcript(context.canvas, agent))

    @router.post("/api/branch", response_model=BranchResponse)
    def branch(request: Request, body: BranchRequest) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(
            request,
            runtime.branch_conversation(
                body.agent,
                body.model_dump(mode="json", exclude_unset=True),
            )
        )

    @router.post("/api/checkpoint", response_model=CheckpointCaptureResponse)
    def checkpoint(request: Request, body: CheckpointCaptureRequest) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        value = runtime.checkpoint_capture(body.agent, body.label)
        return context.send(request, runtime.checkpoint_summary(value))

    @router.post("/api/checkpoint/preview", response_model=CheckpointPreviewResponse)
    def checkpoint_preview(request: Request, body: CheckpointPreviewRequest) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(request, runtime.checkpoint_preview(body.agent, body.checkpoint))

    @router.post("/api/checkpoint/restore", response_model=CheckpointRestoreResponse)
    def checkpoint_restore(request: Request, body: CheckpointRestoreRequest) -> Response:
        runtime = context.runtime
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(
            request,
            runtime.restore_checkpoint(
                body.agent,
                cast(dict[str, JsonValue], body.model_dump(mode="json", exclude_unset=True)),
            )
        )

    return router
