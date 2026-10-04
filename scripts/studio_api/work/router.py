"""Workspace and messaging endpoints backed directly by the existing services."""

from __future__ import annotations

import json
import sqlite3
import time
from contextlib import AbstractContextManager
from typing import TYPE_CHECKING, Callable, Literal, Protocol, TypeVar, cast, get_args

from fastapi import APIRouter, Depends, Request
from pydantic import StrictStr, TypeAdapter, ValidationError
from starlette.responses import Response

from studio_api.models import ContractModel, ErrorResponse, JsonValue

if TYPE_CHECKING:
    from studio_api.context import ApiContext

from .models import (
    AgentChatQuery,
    AgentQuery,
    AgentRoomQuery,
    AnnotationBody,
    AnswerBody,
    AnswerResult,
    ChatRead,
    ChatCreateBody,
    ChatCreated,
    ChatReceipt,
    ChangesQuery,
    ChangesResponse,
    ComplaintBody,
    ConnectionBody,
    ManagedMessageBody,
    MessageHistory,
    MessageReceipt,
    MessageReceipts,
    MutationReceipt,
    OrganizationBody,
    PanelLayoutBody,
    PlanBody,
    PlanView,
    ProfileBody,
    ProfileList,
    ProfileMutation,
    QuestionBody,
    QuestionDeferBody,
    ProgressPanel,
    QueueBody,
    QueueView,
    ReceiptQuery,
    RequestIdQuery,
    RoomDeleteBody,
    RuleBody,
    RuleList,
    RuleMutation,
    TaskFeedQuery,
    TaskIdQuery,
    ToolRequestCancelBody,
    WorkBody,
    WorkItem,
    WorkList,
    WorkspaceTaskFeed,
    WorkspaceQuery,
    WorkspaceView,
    VersionedRuntimeRecord,
    PanelLayoutResult,
    QuestionHistory,
    ToolRequestList,
)


ModelT = TypeVar("ModelT", bound=ContractModel)
Body = dict[str, JsonValue]


class FederationPort(Protocol):
    def has_room(self, room: str) -> bool: ...
    def user_message(self, room: str, text: str, request_id: str) -> dict[str, JsonValue]: ...


class WorkRuntime(Protocol):
    def request_action(self, actor: str | None, body: Body) -> object: ...
    def question_history(self, agent: str | None) -> object: ...
    def workspace_part(self, agent: str | None, view: Literal["work", "inbox", "annotations"]) -> object: ...
    def workspace_snapshot(self, agent: str | None, *, view: Literal["full", "inbox"]) -> object: ...
    def workspace_task_feed(self, agent: str | None, *, cursor: JsonValue, before: JsonValue, limit: int) -> object: ...
    def work_action(self, agent: str | None, body: Body, request_id: str | None = None) -> object: ...
    def queue_action(self, agent: str | None, body: Body | None = None) -> object: ...
    def user_delivery_receipts(self, agent: str | None, ids: list[str]) -> object: ...
    def changes(self, agent: str | None, *, scope: Literal["chat"] | None = None) -> object: ...
    def plan_action(self, agent: str | None, body: Body | None = None) -> object: ...
    def get_panel(self, agent: str | None) -> object: ...
    def profiles(self, body: Body | None = None) -> object: ...
    def rules(self, body: Body | None = None) -> object: ...
    def task_detail(self, task_id: str) -> object: ...
    def complaint_detail(self, complaint_id: str) -> object: ...
    def chat_read(self, room: str, *, before: int | None, after: int | None, limit: int) -> object: ...
    def annotate(self, agent: str | None, body: Body) -> object: ...
    def chat_organization(self, agent: str, body: Body) -> object: ...
    def db(self) -> AbstractContextManager[sqlite3.Connection]: ...
    def federation(self) -> FederationPort: ...
    def send(self, agent: str, text: str, request_id: str, *, delivery: Literal["queue", "steer", "after_tool", "after_turn"], assets: list[JsonValue]) -> dict[str, JsonValue]: ...
    def hide_room(self, room_id: str) -> object: ...
    def complaint_response_from_user(self, body: Body, request_id: str) -> object: ...
    def complaint(self, lead: str | None, body: Body, request_id: str, *, user: bool) -> object: ...
    def delete_question(self, request_id: str) -> object: ...
    def defer_question(self, request_id: str, deferred: bool | None) -> object: ...
    def answer(self, request_id: str, body: Body) -> object: ...


class CanvasPort(Protocol):
    lock: AbstractContextManager[object]

    def connect(self) -> AbstractContextManager[sqlite3.Connection]: ...
    def messages(self, room: str) -> object: ...
    def post(self, room: str, text: str, request_id: str) -> object: ...
    def create_chat(self, name: str, members: list[str], request_id: str) -> object: ...
    def connect_chat(self, source: str, target: str, connected: bool) -> object: ...


def _query(model: type[ModelT], request: Request) -> ModelT:
    """Match the legacy parse_qs(...)[key][0] behavior for repeated keys."""
    values: dict[str, str | int] = {}
    for key in request.query_params.keys():
        nonempty = [value for value in request.query_params.getlist(key) if value != ""]
        if key in model.model_fields and nonempty:
            values[key] = nonempty[0]
    try:
        for key, value in tuple(values.items()):
            annotation = model.model_fields[key].annotation
            if annotation is int or int in get_args(annotation):
                values[key] = int(value)
        return model.model_validate(values)
    except (ValidationError, ValueError):
        raise ValueError("Invalid query parameters") from None


def _body(body: ContractModel) -> dict[str, JsonValue]:
    return cast(dict[str, JsonValue], body.model_dump(mode="json", exclude_unset=True))


def _runtime(context: ApiContext) -> WorkRuntime:
    runtime = context.runtime
    if runtime is None:
        raise ValueError("The agent runtime is unavailable")
    return cast(WorkRuntime, runtime)


def _canvas(context: ApiContext) -> CanvasPort:
    return cast(CanvasPort, context.canvas)


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/tool-requests", response_model=ToolRequestList | VersionedRuntimeRecord,
                responses={400: {"model": ErrorResponse}})
    def tool_requests(request: Request, documented: RequestIdQuery = Depends()) -> Response:
        query = _query(RequestIdQuery, request)
        agent = query.agent
        request_id = query.request_id
        action: Body = {"action": "get", "request_id": request_id} if request_id else {"action": "list"}
        return context.send(request, _runtime(context).request_action(agent, action))

    @router.post("/api/tool-requests/cancel", response_model=ToolRequestList | VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}})
    def cancel_tool_request(request: Request, body: ToolRequestCancelBody) -> Response:
        return context.send(request, _runtime(context).request_action(
            body.agent, {"action": "cancel", "request_id": body.request_id},
        ))

    @router.get("/api/questions", response_model=QuestionHistory, responses={400: {"model": ErrorResponse}})
    def questions(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, _runtime(context).question_history(query.agent))

    @router.get("/api/workspace", response_model=WorkspaceView, responses={400: {"model": ErrorResponse}})
    def workspace(request: Request, documented: WorkspaceQuery = Depends()) -> Response:
        query = _query(WorkspaceQuery, request)
        if query.view in {"work", "inbox", "annotations"}:
            value = _runtime(context).workspace_part(query.agent, query.view)
        elif query.view == "full":
            value = _runtime(context).workspace_snapshot(query.agent, view="full")
        else:
            raise ValueError("Unknown workspace view")
        return context.send(request, value, etag=True)

    @router.get("/api/workspace/tasks", response_model=WorkspaceTaskFeed,
                responses={400: {"model": ErrorResponse}})
    def workspace_tasks(request: Request, documented: TaskFeedQuery = Depends()) -> Response:
        query = _query(TaskFeedQuery, request)
        cursor = json.loads(query.cursor) if query.cursor else None
        before = json.loads(query.before) if query.before else None
        started = time.perf_counter() if query.timing == "1" else None
        result = _runtime(context).workspace_task_feed(query.agent, cursor=cursor, before=before,
                                                       limit=query.limit)
        timing = {"task-feed": (time.perf_counter() - started) * 1000} if started is not None else None
        return context.send(request, result, etag=True, server_timing=timing)

    @router.get("/api/work", response_model=WorkList, responses={400: {"model": ErrorResponse}})
    def work(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, _runtime(context).work_action(query.agent, {"action": "list"}))

    @router.post("/api/work", response_model=WorkItem | WorkList,
                 responses={400: {"model": ErrorResponse}})
    def work_action(request: Request, body: WorkBody) -> Response:
        values = _body(body)
        return context.send(request, _runtime(context).work_action(body.agent, values, body.id))

    @router.get("/api/queue", response_model=QueueView, responses={400: {"model": ErrorResponse}})
    def queue(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, _runtime(context).queue_action(query.agent))

    @router.post("/api/queue", response_model=MutationReceipt, responses={400: {"model": ErrorResponse}})
    def queue_action(request: Request, body: QueueBody) -> Response:
        values = _body(body)
        return context.send(request, _runtime(context).queue_action(body.agent, values))

    @router.get("/api/messages/receipts", response_model=MessageReceipts, responses={400: {"model": ErrorResponse}})
    def message_receipts(request: Request, documented: ReceiptQuery = Depends()) -> Response:
        query = _query(ReceiptQuery, request)
        try:
            ids = TypeAdapter(list[StrictStr]).validate_json(query.ids)
        except ValidationError:
            raise ValueError("Invalid receipt ID list") from None
        return context.send(request, _runtime(context).user_delivery_receipts(query.agent, ids))

    @router.get("/api/changes", response_model=ChangesResponse, responses={400: {"model": ErrorResponse}})
    def changes(request: Request, documented: ChangesQuery = Depends()) -> Response:
        query = _query(ChangesQuery, request)
        return context.send(request, _runtime(context).changes(query.agent, scope=query.scope), etag=True)

    @router.get("/api/plan", response_model=PlanView, responses={400: {"model": ErrorResponse}})
    def plan(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, _runtime(context).plan_action(query.agent), etag=True)

    @router.post("/api/plan", response_model=PlanView, responses={400: {"model": ErrorResponse}})
    def save_plan(request: Request, body: PlanBody) -> Response:
        values = _body(body)
        return context.send(request, _runtime(context).plan_action(body.agent, values))

    @router.get("/api/panel", response_model=ProgressPanel, responses={400: {"model": ErrorResponse}})
    def panel(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, _runtime(context).get_panel(query.agent))

    @router.get("/api/profiles", response_model=ProfileList, responses={400: {"model": ErrorResponse}})
    def profiles(request: Request) -> Response:
        return context.send(request, _runtime(context).profiles())

    @router.post("/api/profiles", response_model=ProfileMutation,
                 responses={400: {"model": ErrorResponse}})
    def update_profiles(request: Request, body: ProfileBody) -> Response:
        return context.send(request, _runtime(context).profiles(_body(body)))

    @router.get("/api/rules", response_model=RuleList, responses={400: {"model": ErrorResponse}})
    def rules(request: Request) -> Response:
        return context.send(request, _runtime(context).rules(), etag=True)

    @router.post("/api/rules", response_model=RuleMutation, responses={400: {"model": ErrorResponse}})
    def update_rules(request: Request, body: RuleBody) -> Response:
        return context.send(request, _runtime(context).rules(_body(body)))

    @router.get("/api/task", response_model=VersionedRuntimeRecord,
                responses={400: {"model": ErrorResponse}})
    def task_detail(request: Request, documented: TaskIdQuery = Depends()) -> Response:
        query = _query(TaskIdQuery, request)
        return context.send(request, _runtime(context).task_detail(query.id))

    @router.get("/api/complaint", response_model=VersionedRuntimeRecord,
                responses={400: {"model": ErrorResponse}})
    def complaint_detail(request: Request, documented: TaskIdQuery = Depends()) -> Response:
        query = _query(TaskIdQuery, request)
        return context.send(request, _runtime(context).complaint_detail(query.id))

    @router.get("/api/agent-chat", response_model=ChatRead, responses={400: {"model": ErrorResponse}})
    def agent_chat(request: Request, documented: AgentChatQuery = Depends()) -> Response:
        query = _query(AgentChatQuery, request)
        return context.send(request, _runtime(context).chat_read(query.room, before=query.before,
                                                        after=query.after, limit=query.limit))

    @router.get("/api/messages", response_model=MessageHistory, responses={400: {"model": ErrorResponse}})
    def messages(request: Request, documented: AgentRoomQuery = Depends()) -> Response:
        query = _query(AgentRoomQuery, request)
        return context.send(request, _canvas(context).messages(query.room))

    @router.post("/api/annotation", response_model=VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}})
    def annotation(request: Request, body: AnnotationBody) -> Response:
        values = _body(body)
        return context.send(request, _runtime(context).annotate(body.agent, values))

    @router.post("/api/organization", response_model=VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}})
    def organization(request: Request, body: OrganizationBody) -> Response:
        return context.send(request, _runtime(context).chat_organization(body.id, _body(body)))

    @router.post("/api/panel/layout", response_model=PanelLayoutResult,
                 responses={400: {"model": ErrorResponse}, 409: {"model": ErrorResponse}})
    def save_panel_layout(request: Request, body: PanelLayoutBody) -> Response:
        from codex_progress_layout import LayoutConflict, record_layout

        try:
            layout_writer = cast(Callable[[WorkRuntime, Body], object], record_layout)
            return context.send(request, layout_writer(_runtime(context), _body(body)))
        except LayoutConflict as error:
            return context.send(request, {"error": str(error)}, status=409)

    @router.post("/api/messages", response_model=MessageReceipt, responses={400: {"model": ErrorResponse}})
    def post_message(request: Request, body: ManagedMessageBody) -> Response:
        values = _body(body)
        runtime = _runtime(context)
        canvas = _canvas(context)
        room, message_id = body.room, body.id
        if runtime.federation().has_room(room):
            result = runtime.federation().user_message(room, body.text, message_id)
            result["id"] = message_id
            return context.send(request, result)
        with runtime.db() as db:
            managed = db.execute("SELECT 1 FROM runtime_agents WHERE id=?", (room,)).fetchone()
        if managed:
            result = runtime.send(room, body.text, message_id,
                                  delivery=body.delivery, assets=body.assets)
            with canvas.lock, canvas.connect() as db:
                db.execute("INSERT OR IGNORE INTO messages VALUES (?,?,?,?,?,?)",
                           (result["id"], room, "user", body.text.strip(),
                            time.time(), json.dumps({room: result["status"]})))
            return context.send(request, result)
        return context.send(request, canvas.post(room, body.text, message_id))

    @router.post("/api/room/delete", response_model=MutationReceipt,
                 responses={400: {"model": ErrorResponse}})
    def delete_room(request: Request, body: RoomDeleteBody) -> Response:
        return context.send(request, _runtime(context).hide_room(body.id))

    @router.post("/api/complaints", response_model=VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}, 409: {"model": ErrorResponse}})
    def complaints(request: Request, body: ComplaintBody) -> Response:
        values = _body(body)
        key = body.id
        if body.action == "respond":
            from codex_runtime import ComplaintConflict

            try:
                return context.send(request, _runtime(context).complaint_response_from_user(values, "user:" + key))
            except ComplaintConflict as error:
                return context.send(request, {"error": str(error)}, status=409)
        return context.send(request, _runtime(context).complaint(body.lead, {"action": "submit", "text": body.text},
                                                       "user:" + key, user=True))

    @router.post("/api/questions/delete", response_model=MutationReceipt,
                 responses={400: {"model": ErrorResponse}})
    def delete_question(request: Request, body: QuestionBody) -> Response:
        return context.send(request, _runtime(context).delete_question(body.id))

    @router.post("/api/questions/defer", response_model=MutationReceipt,
                 responses={400: {"model": ErrorResponse}})
    def defer_question(request: Request, body: QuestionDeferBody) -> Response:
        return context.send(request, _runtime(context).defer_question(body.id, body.deferred))

    @router.post("/api/answer", response_model=AnswerResult,
                 responses={400: {"model": ErrorResponse}})
    def answer(request: Request, body: AnswerBody) -> Response:
        return context.send(request, _runtime(context).answer(body.id, _body(body)))

    @router.post("/api/chats", response_model=ChatCreated,
                 responses={400: {"model": ErrorResponse}})
    def create_chat(request: Request, body: ChatCreateBody) -> Response:
        return context.send(request, _canvas(context).create_chat(body.name, body.members, body.id))

    @router.post("/api/connections", response_model=ChatReceipt,
                 responses={400: {"model": ErrorResponse}})
    def connect_chat(request: Request, body: ConnectionBody) -> Response:
        return context.send(request, _canvas(context).connect_chat(body.source, body.target, body.connected))

    return router
