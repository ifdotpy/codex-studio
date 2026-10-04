"""Workspace and messaging endpoints backed directly by the existing services."""

from __future__ import annotations

import json
import time
from typing import TYPE_CHECKING, TypeVar, cast, get_args

from fastapi import APIRouter, Depends, Request
from pydantic import ValidationError
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


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get("/api/tool-requests", response_model=ToolRequestList | VersionedRuntimeRecord,
                responses={400: {"model": ErrorResponse}})
    def tool_requests(request: Request, documented: RequestIdQuery = Depends()) -> Response:
        query = _query(RequestIdQuery, request)
        agent = query.agent
        request_id = query.request_id
        action = {"action": "get", "request_id": request_id} if request_id else {"action": "list"}
        return context.send(request, context.runtime.request_action(agent, action))

    @router.post("/api/tool-requests/cancel", response_model=ToolRequestList | VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}})
    def cancel_tool_request(request: Request, body: ToolRequestCancelBody) -> Response:
        return context.send(request, context.runtime.request_action(
            body.agent, {"action": "cancel", "request_id": body.request_id},
        ))

    @router.get("/api/questions", response_model=QuestionHistory, responses={400: {"model": ErrorResponse}})
    def questions(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, context.runtime.question_history(query.agent))

    @router.get("/api/workspace", response_model=WorkspaceView, responses={400: {"model": ErrorResponse}})
    def workspace(request: Request, documented: WorkspaceQuery = Depends()) -> Response:
        query = _query(WorkspaceQuery, request)
        if query.view in {"work", "inbox", "annotations"}:
            value = context.runtime.workspace_part(query.agent, query.view)
        else:
            value = context.runtime.workspace_snapshot(query.agent, view=query.view)
        return context.send(request, value, etag=True)

    @router.get("/api/workspace/tasks", response_model=WorkspaceTaskFeed,
                responses={400: {"model": ErrorResponse}})
    def workspace_tasks(request: Request, documented: TaskFeedQuery = Depends()) -> Response:
        query = _query(TaskFeedQuery, request)
        cursor = json.loads(query.cursor) if query.cursor else None
        before = json.loads(query.before) if query.before else None
        started = time.perf_counter() if query.timing == "1" else None
        result = context.runtime.workspace_task_feed(query.agent, cursor=cursor, before=before,
                                                     limit=query.limit)
        timing = {"task-feed": (time.perf_counter() - started) * 1000} if started is not None else None
        return context.send(request, result, etag=True, server_timing=timing)

    @router.get("/api/work", response_model=WorkList, responses={400: {"model": ErrorResponse}})
    def work(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, context.runtime.work_action(query.agent, {"action": "list"}))

    @router.post("/api/work", response_model=WorkItem, responses={400: {"model": ErrorResponse}})
    def work_action(request: Request, body: WorkBody) -> Response:
        values = _body(body)
        return context.send(request, context.runtime.work_action(values.get("agent"), values, values.get("id")))

    @router.get("/api/queue", response_model=QueueView, responses={400: {"model": ErrorResponse}})
    def queue(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, context.runtime.queue_action(query.agent))

    @router.post("/api/queue", response_model=MutationReceipt, responses={400: {"model": ErrorResponse}})
    def queue_action(request: Request, body: QueueBody) -> Response:
        values = _body(body)
        return context.send(request, context.runtime.queue_action(values.get("agent"), values))

    @router.get("/api/messages/receipts", response_model=MessageReceipts, responses={400: {"model": ErrorResponse}})
    def message_receipts(request: Request, documented: ReceiptQuery = Depends()) -> Response:
        query = _query(ReceiptQuery, request)
        ids = json.loads(query.ids)
        return context.send(request, context.runtime.user_delivery_receipts(query.agent, ids))

    @router.get("/api/changes", response_model=ChangesResponse, responses={400: {"model": ErrorResponse}})
    def changes(request: Request, documented: ChangesQuery = Depends()) -> Response:
        query = _query(ChangesQuery, request)
        return context.send(request, context.runtime.changes(query.agent, scope=query.scope), etag=True)

    @router.get("/api/plan", response_model=PlanView, responses={400: {"model": ErrorResponse}})
    def plan(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, context.runtime.plan_action(query.agent), etag=True)

    @router.post("/api/plan", response_model=PlanView, responses={400: {"model": ErrorResponse}})
    def save_plan(request: Request, body: PlanBody) -> Response:
        values = _body(body)
        return context.send(request, context.runtime.plan_action(values.get("agent"), values))

    @router.get("/api/panel", response_model=ProgressPanel, responses={400: {"model": ErrorResponse}})
    def panel(request: Request, documented: AgentQuery = Depends()) -> Response:
        query = _query(AgentQuery, request)
        return context.send(request, context.runtime.get_panel(query.agent))

    @router.get("/api/profiles", response_model=ProfileList, responses={400: {"model": ErrorResponse}})
    def profiles(request: Request) -> Response:
        return context.send(request, context.runtime.profiles())

    @router.post("/api/profiles", response_model=ProfileMutation,
                 responses={400: {"model": ErrorResponse}})
    def update_profiles(request: Request, body: ProfileBody) -> Response:
        return context.send(request, context.runtime.profiles(_body(body)))

    @router.get("/api/rules", response_model=RuleList, responses={400: {"model": ErrorResponse}})
    def rules(request: Request) -> Response:
        return context.send(request, context.runtime.rules(), etag=True)

    @router.post("/api/rules", response_model=RuleMutation, responses={400: {"model": ErrorResponse}})
    def update_rules(request: Request, body: RuleBody) -> Response:
        return context.send(request, context.runtime.rules(_body(body)))

    @router.get("/api/task", response_model=VersionedRuntimeRecord,
                responses={400: {"model": ErrorResponse}})
    def task_detail(request: Request, documented: TaskIdQuery = Depends()) -> Response:
        query = _query(TaskIdQuery, request)
        return context.send(request, context.runtime.task_detail(query.id))

    @router.get("/api/complaint", response_model=VersionedRuntimeRecord,
                responses={400: {"model": ErrorResponse}})
    def complaint_detail(request: Request, documented: TaskIdQuery = Depends()) -> Response:
        query = _query(TaskIdQuery, request)
        return context.send(request, context.runtime.complaint_detail(query.id))

    @router.get("/api/agent-chat", response_model=ChatRead, responses={400: {"model": ErrorResponse}})
    def agent_chat(request: Request, documented: AgentChatQuery = Depends()) -> Response:
        query = _query(AgentChatQuery, request)
        return context.send(request, context.runtime.chat_read(query.room, before=query.before,
                                                      after=query.after, limit=query.limit))

    @router.get("/api/messages", response_model=MessageHistory, responses={400: {"model": ErrorResponse}})
    def messages(request: Request, documented: AgentRoomQuery = Depends()) -> Response:
        query = _query(AgentRoomQuery, request)
        return context.send(request, context.canvas.messages(query.room))

    @router.post("/api/annotation", response_model=VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}})
    def annotation(request: Request, body: AnnotationBody) -> Response:
        values = _body(body)
        return context.send(request, context.runtime.annotate(values.get("agent"), values))

    @router.post("/api/organization", response_model=VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}})
    def organization(request: Request, body: OrganizationBody) -> Response:
        return context.send(request, context.runtime.chat_organization(body.id, _body(body)))

    @router.post("/api/panel/layout", response_model=PanelLayoutResult,
                 responses={400: {"model": ErrorResponse}, 409: {"model": ErrorResponse}})
    def save_panel_layout(request: Request, body: PanelLayoutBody) -> Response:
        from codex_progress_layout import LayoutConflict, record_layout

        try:
            return context.send(request, record_layout(context.runtime, _body(body)))
        except LayoutConflict as error:
            return context.send(request, {"error": str(error)}, status=409)

    @router.post("/api/messages", response_model=MessageReceipt, responses={400: {"model": ErrorResponse}})
    def post_message(request: Request, body: ManagedMessageBody) -> Response:
        values = _body(body)
        runtime = context.runtime
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
            with context.canvas.lock, context.canvas.connect() as db:
                db.execute("INSERT OR IGNORE INTO messages VALUES (?,?,?,?,?,?)",
                           (result["id"], room, "user", body.text.strip(),
                            time.time(), json.dumps({room: result["status"]})))
            return context.send(request, result)
        return context.send(request, context.canvas.post(room, body.text, message_id))

    @router.post("/api/room/delete", response_model=MutationReceipt,
                 responses={400: {"model": ErrorResponse}})
    def delete_room(request: Request, body: RoomDeleteBody) -> Response:
        return context.send(request, context.runtime.hide_room(body.id))

    @router.post("/api/complaints", response_model=VersionedRuntimeRecord,
                 responses={400: {"model": ErrorResponse}, 409: {"model": ErrorResponse}})
    def complaints(request: Request, body: ComplaintBody) -> Response:
        values = _body(body)
        key = body.id
        if body.action == "respond":
            from codex_runtime import ComplaintConflict

            try:
                return context.send(request, context.runtime.complaint_response_from_user(values, "user:" + key))
            except ComplaintConflict as error:
                return context.send(request, {"error": str(error)}, status=409)
        return context.send(request, context.runtime.complaint(body.lead, {"action": "submit", "text": body.text},
                                                       "user:" + key, user=True))

    @router.post("/api/questions/delete", response_model=MutationReceipt,
                 responses={400: {"model": ErrorResponse}})
    def delete_question(request: Request, body: QuestionBody) -> Response:
        return context.send(request, context.runtime.delete_question(body.id))

    @router.post("/api/questions/defer", response_model=MutationReceipt,
                 responses={400: {"model": ErrorResponse}})
    def defer_question(request: Request, body: QuestionDeferBody) -> Response:
        return context.send(request, context.runtime.defer_question(body.id, body.deferred))

    @router.post("/api/answer", response_model=AnswerResult,
                 responses={400: {"model": ErrorResponse}})
    def answer(request: Request, body: AnswerBody) -> Response:
        return context.send(request, context.runtime.answer(body.id, _body(body)))

    @router.post("/api/chats", response_model=ChatCreated,
                 responses={400: {"model": ErrorResponse}})
    def create_chat(request: Request, body: ChatCreateBody) -> Response:
        return context.send(request, context.canvas.create_chat(body.name, body.members, body.id))

    @router.post("/api/connections", response_model=ChatReceipt,
                 responses={400: {"model": ErrorResponse}})
    def connect_chat(request: Request, body: ConnectionBody) -> Response:
        return context.send(request, context.canvas.connect_chat(body.source, body.target, body.connected))

    return router
