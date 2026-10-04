"""FastAPI routes for history and checkpoint operations."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Annotated, Callable, Protocol, TypeVar, cast

from fastapi import APIRouter, Depends, Request
from fastapi.exceptions import RequestValidationError
from pydantic import ValidationError
from starlette.responses import Response

from studio_api.models import ContractModel, ErrorResponse, JsonValue
from codex_sync_entities import project
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

ERROR_RESPONSES: dict[int | str, dict[str, Any]] = {
    400: {"model": ErrorResponse},
    404: {"model": ErrorResponse},
}


class HistoryRuntime(Protocol):
    def transcript(
        self,
        agent: str | None,
        *,
        before: str | None,
        around: str | None,
        after: str | None,
        limit: str,
    ) -> JsonValue: ...

    def search_item(self, identity: str | None) -> JsonValue: ...

    def search_work(self, query: str | None, *, limit: str) -> JsonValue: ...

    def workspace_snapshot(self, agent: str | None) -> dict[str, JsonValue]: ...

    def branch_conversation(self, agent: str, data: dict[str, JsonValue]) -> JsonValue: ...

    def checkpoint_capture(self, agent: str, label: str) -> JsonValue: ...

    def checkpoint_summary(self, checkpoint: JsonValue) -> JsonValue: ...

    def checkpoint_preview(self, agent: str, checkpoint: str) -> JsonValue: ...

    def restore_checkpoint(self, agent: str, data: dict[str, JsonValue]) -> JsonValue: ...


class TranscriptCanvas(Protocol):
    def transcript(self, agent: str | None) -> JsonValue: ...


def _first_query(request: Request, model: type[QueryContract]) -> QueryContract:
    """Revalidate the first nonblank value, matching the legacy parser."""
    allowed = model.model_fields
    values: dict[str, str] = {}
    for name, value in request.query_params.multi_items():
        if name in allowed and value != "" and name not in values:
            values[name] = value
    try:
        return model.model_validate(values)
    except ValidationError as error:
        # The shared app handler maps request validation failures to the legacy
        # 400 contract. A bare Pydantic ValidationError would look like a 500.
        raise RequestValidationError(error.errors()) from error


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


def _history_runtime(context: ApiContext) -> HistoryRuntime | None:
    runtime = context.runtime
    return cast(HistoryRuntime, runtime) if runtime is not None else None


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/transcript/page", response_model=TranscriptPageResponse,
                responses=ERROR_RESPONSES)
    def transcript_page(
        request: Request,
        query: Annotated[TranscriptQuery, Depends()],
    ) -> Response:
        query = _first_query(request, TranscriptQuery)
        runtime = _history_runtime(context)
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
                responses=ERROR_RESPONSES)
    def transcript_item(
        request: Request,
        query: Annotated[TranscriptItemQuery, Depends()],
    ) -> Response:
        query = _first_query(request, TranscriptItemQuery)
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(request, _history_item(runtime, query.id, query.message_id))

    @router.get("/api/transcript/search", response_model=TranscriptSearchResponse,
                responses=ERROR_RESPONSES)
    def transcript_search(
        request: Request,
        query: Annotated[TranscriptSearchQuery, Depends()],
    ) -> Response:
        query = _first_query(request, TranscriptSearchQuery)
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(
            request,
            _search_history(runtime, query.id, query.q, query.limit or "100")
        )

    @router.get("/api/search/item", response_model=SearchItemResponse,
                responses=ERROR_RESPONSES)
    def search_item(
        request: Request,
        query: Annotated[TranscriptItemQuery, Depends()],
    ) -> Response:
        query = _first_query(request, TranscriptItemQuery)
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(request, runtime.search_item(query.id))

    @router.get("/api/search", response_model=SearchResponse,
                responses=ERROR_RESPONSES)
    def search(request: Request, query: Annotated[SearchQuery, Depends()]) -> Response:
        query = _first_query(request, SearchQuery)
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(
            request,
            runtime.search_work(query.q, limit=query.limit or "50")
        )

    @router.get("/api/checkpoints", response_model=CheckpointsResponse,
                responses=ERROR_RESPONSES)
    def checkpoints(
        request: Request,
        query: Annotated[CheckpointsQuery, Depends()],
    ) -> Response:
        query = _first_query(request, CheckpointsQuery)
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        value = runtime.workspace_snapshot(query.agent)
        return context.send(request, {"checkpoints": value["checkpoints"]}, etag=True)

    @router.get("/api/transcript", response_model=TranscriptPageResponse,
                responses=ERROR_RESPONSES)
    def transcript(
        request: Request,
        query: Annotated[TranscriptQuery, Depends()],
    ) -> Response:
        query = _first_query(request, TranscriptQuery)
        agent = query.id if query.id is not None else ""
        return context.send(request, _canvas_transcript(context.canvas, agent))

    @router.post("/api/branch", response_model=BranchResponse, responses=ERROR_RESPONSES)
    def branch(request: Request, body: BranchRequest) -> Response:
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        raw_value = runtime.branch_conversation(
            body.agent,
            cast(dict[str, JsonValue], body.model_dump(mode="json", exclude_unset=True)),
        )
        if not isinstance(raw_value, dict):
            raise TypeError("Branch producer did not return an agent record")
        value = raw_value
        public_agent: object = project("agent", value)
        if not isinstance(public_agent, dict):
            raise TypeError("Branch producer did not return an agent record")
        result = {**public_agent}
        if "draft" in value:
            result["draft"] = value["draft"]
        return context.send(request, cast(dict[str, JsonValue], result))

    @router.post("/api/checkpoint", response_model=CheckpointCaptureResponse,
                 responses=ERROR_RESPONSES)
    def checkpoint(request: Request, body: CheckpointCaptureRequest) -> Response:
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        value = runtime.checkpoint_capture(body.agent, body.label)
        return context.send(request, runtime.checkpoint_summary(value))

    @router.post("/api/checkpoint/preview", response_model=CheckpointPreviewResponse,
                 responses=ERROR_RESPONSES)
    def checkpoint_preview(request: Request, body: CheckpointPreviewRequest) -> Response:
        runtime = _history_runtime(context)
        if runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(request, runtime.checkpoint_preview(body.agent, body.checkpoint))

    @router.post("/api/checkpoint/restore", response_model=CheckpointRestoreResponse,
                 responses=ERROR_RESPONSES)
    def checkpoint_restore(request: Request, body: CheckpointRestoreRequest) -> Response:
        runtime = _history_runtime(context)
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
