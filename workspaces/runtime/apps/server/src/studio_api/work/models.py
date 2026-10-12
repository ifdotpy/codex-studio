"""Typed HTTP contracts for workspace and messaging routes."""

from __future__ import annotations

from types import UnionType
from typing import Annotated, Any, Callable, Literal, Union, cast, get_args, get_origin, get_type_hints
from typing_extensions import NotRequired, Required, TypedDict, TypeAliasType

from pydantic import ConfigDict, Field, RootModel, StrictBool, StrictFloat, StrictInt, StrictStr, create_model, model_validator

from codex_records import (
    AgentRecord as AgentRecordContract,
    AmbiguousToolRequestResult,
    MissingToolRequestResult,
    MonitorRecord as MonitorRecordContract,
    MovedToolRequestResult,
    ComplaintRecord as ComplaintRecordContract,
    RequestRecord as RequestRecordContract,
    RuleRecord as RuleRecordContract,
    ToolRequestRecord as ToolRequestRecordContract,
    WorkViewRecord as WorkViewRecordContract,
    WorkArchiveIntentRecord,
    WorkArchiveRecord,
    WorkDecisionRecord,
    WorkResultRecord,
)
from studio_api.models import ContractModel, JsonValue, ResponseModel
from studio_api.history.models import CheckpointSummary, TranscriptAsset
from studio_api.sync.models import (
    MonitorEntityDto,
    ComplaintResponseDto,
    ComplaintStatus,
    RoomEntityDto,
    TaskKind,
    TaskStatus,
    TaskEntityDto,
)

JsonObjectResponse = TypeAliasType("JsonObjectResponse", dict[str, JsonValue])


_JSON_OBJECT_CONTRACT = get_args(
    get_type_hints(AgentRecordContract, include_extras=True)["frozenNativeParams"]
)[0]


class AgentQuery(ContractModel):
    agent: StrictStr | None = None


class AgentRoomQuery(ContractModel):
    room: StrictStr = ""


class WorkspaceQuery(AgentQuery):
    view: Literal["full", "work", "inbox", "annotations"] = "full"


class TaskFeedQuery(AgentQuery):
    cursor: StrictStr | None = None
    before: StrictStr | None = None
    limit: int = 100
    timing: Literal["0", "1"] = "0"


class TaskIdQuery(ContractModel):
    id: StrictStr = ""


class AgentChatQuery(ContractModel):
    room: StrictStr = ""
    before: int | None = None
    after: int | None = None
    limit: int = 100


class ReceiptQuery(AgentQuery):
    ids: StrictStr = "[]"


class RequestIdQuery(AgentQuery):
    request_id: StrictStr | None = None


class ToolRequestCancelBody(ContractModel):
    agent: StrictStr
    request_id: StrictStr = Field(min_length=1, max_length=1000)


class ChangesQuery(AgentQuery):
    scope: Literal["chat"] | None = None


class RulesQuery(AgentQuery):
    pass


class OperationBody(ContractModel):
    agent: StrictStr | None = None
    id: StrictStr | None = None


class WorkBody(OperationBody):
    action: Literal["list", "create", "update", "claim", "submit", "cancel", "accept", "reject"]
    task_id: StrictStr | None = None
    version: StrictInt | None = None
    title: StrictStr | None = Field(default=None, max_length=160)
    description: StrictStr | None = Field(default=None, max_length=64000)
    owner: StrictStr | None = None
    dependencies: list[StrictStr] | None = Field(default=None, max_length=100)
    status: Literal["ready", "blocked"] | None = None
    result: StrictStr | None = Field(default=None, max_length=12000)
    checks: StrictStr | None = Field(default=None, max_length=12000)
    revision: StrictStr | None = Field(default=None, max_length=200)
    files: list[StrictStr] | None = Field(default=None, max_length=50)
    reason: StrictStr | None = Field(default=None, max_length=12000)


class QueueCancelBody(OperationBody):
    action: Literal["cancel"]
    request_id: StrictStr | None = Field(default=None, max_length=200)
    message_id: StrictStr | None = None
    expected_revision: StrictStr | None = None
    expectedText: StrictStr | None = None

    @model_validator(mode="after")
    def has_message_identity(self) -> QueueCancelBody:
        if not self.id and not self.message_id:
            raise ValueError("Supply a queued message ID")
        return self


class QueueEditBody(OperationBody):
    action: Literal["edit"]
    request_id: StrictStr | None = Field(default=None, max_length=200)
    message_id: StrictStr | None = None
    expected_revision: StrictStr | None = None
    expectedText: StrictStr | None = None
    text: StrictStr

    @model_validator(mode="after")
    def has_message_identity(self) -> QueueEditBody:
        if not self.id and not self.message_id:
            raise ValueError("Supply a queued message ID")
        return self


class QueueFirstBody(OperationBody):
    action: Literal["first", "send_now"]
    request_id: StrictStr | None = Field(default=None, max_length=200)
    message_id: StrictStr | None = None
    expected_revision: StrictStr | None = None
    expectedText: StrictStr | None = None

    @model_validator(mode="after")
    def has_message_identity(self) -> QueueFirstBody:
        if not self.id and not self.message_id:
            raise ValueError("Supply a queued message ID")
        if self.action == "send_now" and not self.request_id:
            raise ValueError("Supply a request ID to change queue delivery")
        return self


class QueueReorderBody(OperationBody):
    action: Literal["reorder"]
    request_id: StrictStr | None = Field(default=None, max_length=200)
    expected_revision: StrictStr
    ordered_ids: list[StrictStr]


QueueBody = Annotated[
    QueueCancelBody | QueueEditBody | QueueFirstBody | QueueReorderBody,
    Field(discriminator="action"),
]


class PlanBody(OperationBody):
    text: StrictStr
    version: StrictInt


class AnnotationBody(OperationBody):
    path: StrictStr
    line: StrictInt
    text: StrictStr
    turnId: StrictStr | None = None


class OrganizationBody(ContractModel):
    id: StrictStr
    read_state: "ReadStateBody | None" = None
    project_folder: StrictStr | None = None
    project_path: StrictStr | None = None
    expected_revision: StrictInt | None = None
    expected_folder: StrictStr | None = None
    pinned: StrictBool | None = None
    archived: StrictBool | None = None
    project: StrictStr | None = None


class ReadStateBody(ContractModel):
    thread_id: StrictStr
    turn_id: StrictStr
    read: StrictBool
    expected_revision: StrictInt


class MessageBody(ContractModel):
    id: StrictStr
    room: StrictStr
    text: StrictStr


class ManagedMessageBody(MessageBody):
    delivery: Literal["queue", "steer", "after_tool", "after_turn"] = "queue"
    assets: list[JsonValue] = Field(default_factory=list)


class ChatCreateBody(ContractModel):
    id: StrictStr
    name: StrictStr = Field(min_length=1, max_length=100)
    members: list[StrictStr] = Field(max_length=100)


class ConnectionBody(ContractModel):
    source: StrictStr
    target: StrictStr
    connected: StrictBool = True


class RoomDeleteBody(ContractModel):
    id: StrictStr


class QuestionBody(ContractModel):
    id: StrictStr
    deferred: StrictBool | None = None


class QuestionDeferBody(QuestionBody):
    deferred: StrictBool


class AnswerBody(ContractModel):
    """Provider question answers may carry provider-specific answer fields."""

    id: StrictStr
    decision: Literal["answer", "accept", "decline", "cancel"] | None = None
    answers: dict[StrictStr, JsonValue] | None = None
    content: dict[StrictStr, JsonValue] | None = None
    # A window on another Mac accepts a sign-in link: its own server holds the redirect port.
    forward: StrictBool | None = None
    model_config = {"extra": "allow"}
    __pydantic_extra__: dict[str, JsonValue]


class ComplaintBody(ContractModel):
    id: StrictStr = Field(min_length=1, max_length=200)
    action: Literal["submit", "respond"] = "submit"
    lead: StrictStr | None = None
    text: StrictStr = Field(min_length=1, max_length=12000)
    complaint_id: StrictStr | None = None
    status: Literal["in_progress", "resolved", "declined"] | None = None
    version: StrictInt | None = None


class ProfileBody(ContractModel):
    action: Literal["save", "delete"] = "save"
    isNew: StrictBool | None = None
    id: StrictStr | None = None
    name: StrictStr | None = Field(default=None, max_length=100)
    role: Literal["reviewer", "implementer"] = "reviewer"
    model: StrictStr | None = Field(default=None, max_length=100)
    effort: Literal["low", "medium", "high", "xhigh", "max", "ultra"] | None = None
    instructions: StrictStr = Field(default="", max_length=16000)


class RuleBody(ContractModel):
    action: Literal["save", "pause", "resume", "delete"] = "save"
    agent: StrictStr
    isNew: StrictBool | None = None
    id: StrictStr | None = None
    rootId: StrictStr | None = None
    epoch: StrictInt | None = None
    name: StrictStr | None = Field(default=None, max_length=120)
    kind: Literal["interval", "once", "file", "event", "low_workers"] = "interval"
    intervalSeconds: StrictInt = Field(default=60, ge=10, le=31536000)
    stallTimeoutSeconds: StrictInt = Field(default=1800, ge=0, le=31536000)
    stall_timeout_seconds: StrictInt | None = Field(default=None, ge=0, le=31536000)
    at: StrictInt | StrictFloat | None = None
    nextAt: StrictInt | StrictFloat | None = None
    event: Literal["worker_completed", "monitor_exit", "work_review", "complaint"] = "worker_completed"
    command: StrictStr = Field(default="", max_length=12000)
    text: StrictStr = Field(default="", max_length=12000)
    path: StrictStr | None = Field(default=None, max_length=4096)
    livenessCommand: StrictStr = Field(default="", max_length=12000)
    minimumWorkers: StrictInt = Field(default=8, ge=1, le=255)
    durationMinutes: StrictInt = Field(default=30, ge=1, le=525600)
    status: Literal["active", "paused"] | None = None
    created: StrictInt | StrictFloat | None = None
    inFlight: StrictBool | None = None
    checks: StrictInt | None = None
    wakes: StrictInt | None = None
    fileActivityAt: StrictInt | StrictFloat | None = None
    fileGeneration: StrictInt | None = None
    stallWakeGeneration: StrictInt | None = None
    fingerprint: list[StrictInt] | None = None
    error: StrictStr | None = None
    lowSince: StrictInt | StrictFloat | None = None
    alerted: StrictBool | None = None
    lastAt: StrictInt | StrictFloat | None = None
    lastEvent: StrictStr | None = None
    eventText: StrictStr | None = None
    stallProbe: StrictBool | None = None
    stallEventKey: StrictStr | None = None
    stallText: StrictStr | None = None
    lastStallFinished: StrictInt | StrictFloat | None = None
    lastStallExitCode: StrictInt | None = None
    lastStallError: StrictStr | None = None
    lastExitCode: StrictInt | None = None
    lastOutput: StrictStr | None = None
    lastFinished: StrictInt | StrictFloat | None = None
    activeWorkers: StrictInt | None = None


class PanelLayoutBody(ContractModel):
    agent: StrictStr
    client: StrictStr
    sequence: StrictInt
    renderer: Literal["progress-markdown-v2", "progress-markdown-v1"]
    revision: StrictStr
    width: StrictInt | StrictFloat
    height: StrictInt | StrictFloat
    contentWidth: StrictInt | StrictFloat
    contentHeight: StrictInt | StrictFloat
    fits: StrictBool
    reason: Literal["overflow", "unsupported"] | None
    overflowX: StrictInt | StrictFloat | None = None
    overflowY: StrictInt | StrictFloat | None = None
    totalLines: StrictInt | None = None
    visibleLines: StrictInt | None = None
    lastVisibleLine: StrictStr | None = None
    lastVisibleHeading: StrictStr | None = None


class WorkItem(ResponseModel):
    """work_view spreads stored records; retain their unknown top-level and nested fields."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue]

    id: StrictStr
    rootId: StrictStr
    title: StrictStr
    description: StrictStr = ""
    owner: StrictStr | None = None
    dependencies: list[StrictStr] = Field(default_factory=list)
    status: Literal["ready", "blocked", "running", "review", "accepted", "cancelled"]
    created: StrictInt | StrictFloat
    updated: StrictInt | StrictFloat
    version: StrictInt
    results: list[WorkResultRecord] = Field(default_factory=list)
    decisions: list[WorkDecisionRecord] = Field(default_factory=list)
    createdBy: StrictStr | None = None
    blockedBy: list[StrictStr] = Field(default_factory=list)
    displayStatus: StrictStr | None = None
    archive: WorkArchiveRecord | None = None
    archivePending: StrictBool | None = None
    archiveIntent: WorkArchiveIntentRecord | None = None
    releases: list[JsonValue] | None = None


class WorkList(ResponseModel):
    items: list[WorkItem]
    tasks: list[WorkItem]


class QueueItem(ContractModel):
    id: StrictStr
    error: str | None = None
    text: StrictStr
    kind: Literal["user", "followup"]
    status: Literal["pending"]
    created: StrictInt | StrictFloat
    assets: list[TranscriptAsset] = Field(default_factory=list)
    delivery: Literal["queue", "steer", "after_tool", "after_turn"] = "queue"
    requestedDelivery: Literal["queue", "steer", "after_tool", "after_turn"] = "queue"
    acceptedAt: StrictInt | StrictFloat | None = None


class QueueCapabilities(ContractModel):
    reorder: StrictBool
    receipts: StrictBool


class QueueView(ResponseModel):
    items: list[QueueItem]
    revision: StrictStr
    capabilities: QueueCapabilities


class NativePlanPayload(ContractModel):
    """Known fields plus provider-specific fields from native plan notifications."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue]
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    explanation: StrictStr | None = None
    plan: list[JsonValue] = Field(default_factory=list)


class PlanView(ResponseModel):
    """Known fields; runtime_plans records are returned without projection."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue]

    id: StrictStr
    rootId: StrictStr
    text: StrictStr
    version: StrictInt
    updated: StrictInt | StrictFloat | None
    steps: list[JsonValue] = Field(default_factory=list)
    native: NativePlanPayload | None = None


class QuestionEntry(ContractModel):
    id: StrictStr
    question: StrictStr
    isSecret: StrictBool


class AnswerHistoryEntry(ContractModel):
    id: StrictStr
    question: StrictStr
    isSecret: StrictBool
    answer: JsonValue


class AgentRequest(ResponseModel):
    id: StrictStr
    agent: StrictStr
    method: Literal["agent/asyncQuestion", "item/tool/requestUserInput", "mcpServer/elicitation/request"]
    status: Literal["pending", "blocked", "answered", "answering", "uncertain", "expired", "declined", "failed"]
    createdAt: StrictInt | StrictFloat | None = None
    answeredAt: StrictInt | StrictFloat | None = None
    answeredBy: StrictStr | None = None
    decision: Literal["answer", "accept", "decline", "cancel"] | None = None
    deferred: StrictBool | None = None
    deferredAt: StrictInt | StrictFloat | None = None
    deferredBy: StrictStr | None = None
    questions: list[QuestionEntry] = Field(default_factory=list)
    answerHistory: list[AnswerHistoryEntry] | None = None
    answerError: StrictStr | None = None


class QuestionHistory(ResponseModel):
    items: list[AgentRequest]


class MutationReceipt(ResponseModel):
    id: StrictStr | None = None
    status: StrictStr | None = None
    deleted: StrictStr | list[StrictStr] | None = None
    deferred: StrictBool | None = None
    revision: StrictStr | None = None
    capabilities: QueueCapabilities | None = None


class MessageRecord(ContractModel):
    id: StrictStr
    room: StrictStr
    author: StrictStr
    text: StrictStr
    at: StrictInt | StrictFloat
    deliveries: dict[StrictStr, StrictStr]
    status: Literal["queued", "delivered", "failed", "uncertain"] | None = None
    error: StrictStr | None = None


class MessageReceipt(ResponseModel):
    id: StrictStr
    status: Literal["queued", "pending", "reserved", "dispatching", "delivered", "accepted", "sent", "uncertain", "failed", "cancelled", "stored_only"]
    error: StrictStr | None = None
    materialized: StrictBool | None = Field(default=None, description="The exact user input exists in the saved transcript.")
    room: StrictStr | None = None
    author: StrictStr | None = None
    text: StrictStr | None = None
    at: StrictInt | StrictFloat | None = None
    deliveries: dict[StrictStr, StrictStr] | None = None
    agent: StrictStr | None = None
    message: StrictStr | None = None
    waitingFor: list[JsonValue] | None = None


class MessageReceipts(ResponseModel):
    agent: StrictStr
    workspaceId: StrictStr | None = None
    items: list[MessageReceipt]


class MessageList(ResponseModel):
    items: list[MessageRecord]


class ReceiptList(ResponseModel):
    items: list[MessageReceipt]


class ChatMessage(ContractModel):
    seq: StrictInt
    id: StrictStr
    room: StrictStr
    sender: StrictStr
    senderName: StrictStr
    text: StrictStr
    created: StrictInt | StrictFloat
    deliveries: dict[StrictStr, StrictStr]


class ChatRead(ResponseModel):
    room: "RoomDetailDto"
    messages: list[ChatMessage]
    nextBefore: StrictInt | None = None
    nextAfter: StrictInt | None = None


class ReadStateResponse(ResponseModel):
    id: StrictStr
    threadId: StrictStr
    turnId: StrictStr
    read: StrictBool
    revision: StrictInt


class MessageHistory(RootModel[list[MessageRecord]]):
    pass


class _OpenRecordResponse(ResponseModel):
    """Response base for records whose stored extensions pass through."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


_RECORD_RESPONSE_MODELS: dict[tuple[type[Any], frozenset[str]], Any] = {}


def _response_annotation(annotation: object, name: str) -> Any:
    if annotation == _JSON_OBJECT_CONTRACT:
        return JsonObjectResponse
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in (NotRequired, Required):
        return origin[_response_annotation(args[0], name)]
    if isinstance(annotation, type) and hasattr(annotation, "__required_keys__"):
        record = cast(type[Any], annotation)
        return _record_response_model(f"{record.__name__}Response", record)
    if origin is Annotated:
        return Annotated[_response_annotation(args[0], name), *args[1:]]
    if origin in (Union, UnionType):
        return Union[tuple(_response_annotation(arg, name) for arg in args)]
    if origin is not None and args and origin is not Literal:
        converted = tuple(_response_annotation(arg, name) for arg in args)
        parameter = converted[0] if len(converted) == 1 else converted
        try:
            return origin[parameter]
        except TypeError:
            return annotation
    return annotation


def _record_response_model(
    name: str,
    record: type[Any],
    *,
    required_fields: frozenset[str] = frozenset(),
) -> Any:
    """Build an open response model from a canonical TypedDict record."""
    cache_key = (record, required_fields)
    if cache_key in _RECORD_RESPONSE_MODELS:
        return _RECORD_RESPONSE_MODELS[cache_key]
    fields: dict[str, tuple[Any, Any]] = {}
    hints = get_type_hints(record, include_extras=True)
    required = getattr(record, "__required_keys__")
    for field_name, annotation in hints.items():
        origin = get_origin(annotation)
        if origin in (NotRequired, Required):
            annotation = get_args(annotation)[0]
        annotation = _response_annotation(annotation, f"{name}{field_name.title()}")
        default = ... if field_name in required or field_name in required_fields else None
        fields[field_name] = (annotation, default)
    create_response_model = cast(Callable[..., Any], create_model)
    response_model = create_response_model(name, __base__=_OpenRecordResponse, **fields)
    _RECORD_RESPONSE_MODELS[cache_key] = response_model
    return response_model


PersistedToolRequest: Any = _record_response_model(
    "ToolRequestRecordResponse", ToolRequestRecordContract,
    required_fields=frozenset({"id", "stage"}),
)
AmbiguousToolRequestResponse: Any = _record_response_model("AmbiguousToolRequestResponse", AmbiguousToolRequestResult)
MissingToolRequestResponse: Any = _record_response_model("MissingToolRequestResponse", MissingToolRequestResult)
MovedToolRequestResponse: Any = _record_response_model("MovedToolRequestResponse", MovedToolRequestResult)

ToolRequestReceipt: Any = RootModel[  # type: ignore[misc]
    Annotated[
        Union[tuple((PersistedToolRequest, AmbiguousToolRequestResponse, MissingToolRequestResponse,
                     MovedToolRequestResponse))],
        Field(discriminator="stage"),
    ]
]


class AnnotationRecord(ContractModel):
    id: StrictStr
    agent: StrictStr
    rootId: StrictStr
    path: StrictStr
    line: StrictInt
    text: StrictStr
    created: StrictInt | StrictFloat
    turnId: StrictStr | None = None


AgentRecordResponse: Any = _record_response_model("AgentRecordResponse", AgentRecordContract)
ComplaintRecordResponse: Any = _record_response_model("ComplaintRecordResponse", ComplaintRecordContract)
RequestRecordResponse: Any = _record_response_model("RequestRecordResponse", RequestRecordContract)
MonitorRecordResponse: Any = _record_response_model("MonitorRecordResponse", MonitorRecordContract)
WorkViewRecordResponse: Any = _record_response_model("WorkViewRecordResponse", WorkViewRecordContract)
RuleRecord: Any = _record_response_model(
    "RuleRecordResponse", RuleRecordContract, required_fields=frozenset({"id", "kind"}),
)


class AnnotationReceipt(AnnotationRecord, ResponseModel):
    pass


class RoomDetailDto(RoomEntityDto):
    federated: StrictBool | None = None
    peerId: StrictStr | None = None
    remoteMembers: list["RemoteRoomMemberDto"] | None = None
    localParticipants: list["LocalRoomParticipantDto"] | None = None
    customName: StrictStr | None = None


class RemoteRoomMemberDto(ContractModel):
    id: StrictStr
    name: StrictStr | None = None
    role: StrictStr | None = None
    status: StrictStr | None = None


class LocalRoomParticipantDto(ContractModel):
    id: StrictStr
    role: Literal["lead", "agent"]
    name: StrictStr | None = None
    status: StrictStr | None = None


class ComplaintDetailResponse(ResponseModel):
    id: StrictStr
    leadId: StrictStr | None = None
    author: StrictStr | None = None
    authorName: StrictStr | None = None
    leadName: StrictStr | None = None
    title: StrictStr | None = None
    status: ComplaintStatus | None = None
    needsResponse: StrictBool | None = None
    created: StrictInt | StrictFloat | None = None
    readAt: StrictInt | StrictFloat | None = None
    leadStopped: StrictBool | None = None
    leadDeleted: StrictBool | None = None
    recipient: Literal["user", "lead"] | None = None
    version: StrictInt
    text: StrictStr
    responses: list[ComplaintResponseDto]
    updated: StrictInt | StrictFloat | None = None


class TaskDetailResponse(TaskEntityDto, ResponseModel):
    agent: StrictStr
    kind: TaskKind
    status: TaskStatus
    created: StrictInt | StrictFloat
    type: StrictStr | None = None
    itemId: StrictStr | None = None
    startedAtMs: StrictInt | StrictFloat | None = None
    completedAtMs: StrictInt | StrictFloat | None = None
    server: StrictStr | None = None
    outputTruncated: StrictBool | None = None
    arguments: StrictStr | None = None
    tail: StrictStr | None = None
    error: StrictStr | None = None


class WorkspaceTaskRecord(TaskEntityDto):
    agent: StrictStr
    kind: TaskKind
    status: TaskStatus
    created: StrictInt | StrictFloat
    itemId: StrictStr | None = None
    type: StrictStr | None = None
    server: StrictStr | None = None
    startedAtMs: StrictInt | StrictFloat | None = None
    completedAtMs: StrictInt | StrictFloat | None = None
    outputTruncated: StrictBool | None = None


class ToolRequestList(ResponseModel):
    requests: list[PersistedToolRequest]


class RuleList(ResponseModel):
    rules: list[RuleRecord]


class DeletedRuleReceipt(ResponseModel):
    deleted: StrictStr


RuleMutation = RuleRecord | DeletedRuleReceipt


class RequestInboxEntry(ResponseModel):
    id: StrictStr
    kind: Literal["request"]
    agent: StrictStr
    title: StrictStr
    text: StrictStr
    request: RequestRecordResponse


class ComplaintInboxEntry(ResponseModel):
    id: StrictStr
    kind: Literal["complaint"]
    agent: StrictStr
    title: StrictStr
    text: StrictStr
    complaint: ComplaintRecordResponse


class WorkInboxEntry(ResponseModel):
    id: StrictStr
    kind: Literal["work"]
    agent: StrictStr
    title: StrictStr
    text: StrictStr
    work: WorkViewRecordResponse


class AgentInboxEntry(ResponseModel):
    id: StrictStr
    kind: Literal["agent"]
    agent: StrictStr
    title: StrictStr
    text: StrictStr


class MonitorInboxEntry(ResponseModel):
    id: StrictStr
    kind: Literal["monitor"]
    agent: StrictStr
    title: StrictStr
    text: StrictStr
    monitor: MonitorRecordResponse


class RuleInboxEntry(ResponseModel):
    id: StrictStr
    kind: Literal["rule"]
    agent: StrictStr
    title: StrictStr
    text: StrictStr


WorkspaceInboxEntry = Annotated[
    RequestInboxEntry | ComplaintInboxEntry | WorkInboxEntry | AgentInboxEntry |
    MonitorInboxEntry | RuleInboxEntry,
    Field(discriminator="kind"),
]


class ProfileRecord(ContractModel):
    id: StrictStr
    name: StrictStr
    role: Literal["reviewer", "implementer"]
    model: StrictStr
    effort: Literal["low", "medium", "high", "xhigh", "max", "ultra"] | None
    instructions: StrictStr


class ProfileList(ResponseModel):
    profiles: list[ProfileRecord]


class ProfileMutation(ResponseModel):
    deleted: StrictStr | None = None
    id: StrictStr | None = None
    name: StrictStr | None = None
    role: Literal["reviewer", "implementer"] | None = None
    model: StrictStr | None = None
    effort: Literal["low", "medium", "high", "xhigh", "max", "ultra"] | None = None
    instructions: StrictStr | None = None


class ChangesFile(ContractModel):
    path: StrictStr
    status: StrictStr


class ChangesResponse(ResponseModel):
    files: list[ChangesFile]
    diff: StrictStr | None = None
    patch: StrictStr
    truncated: StrictBool | None = None
    revision: StrictStr | None = None
    git: StrictBool
    error: StrictStr | None = None
    scope: Literal["chat"] | None = None
    turnId: StrictStr | None = None
    reportedAt: StrictInt | StrictFloat | None = None


class ProgressPanel(ResponseModel):
    id: StrictStr
    agent: StrictStr
    format: Literal["markdown"]
    markdown: StrictStr
    path: StrictStr
    revision: StrictStr | None
    updated: StrictInt | StrictFloat | None
    exists: StrictBool
    error: StrictStr | None


class TaskCursor(ContractModel):
    updated: StrictInt | StrictFloat
    id: StrictStr


class TaskHistoryCursor(ContractModel):
    created: StrictInt | StrictFloat
    id: StrictStr


class WorkspaceTaskFeed(ResponseModel):
    tasks: list[WorkspaceTaskRecord]
    cursor: TaskCursor | None = None
    hasMore: StrictBool
    hasMoreChanges: StrictBool | None = None
    nextBefore: TaskHistoryCursor | None = None
    reset: StrictBool | None = None


class ChatReceipt(ResponseModel):
    id: StrictStr
    connected: StrictBool


class ChatCreated(ResponseModel):
    id: StrictStr


class WorkspaceView(ResponseModel):
    work: list[WorkItem] | None = None
    annotations: list[AnnotationRecord] | None = None
    checkpoints: list[CheckpointSummary] | None = None
    plans: list[PlanView] | None = None
    rules: list[RuleRecord] | None = None
    inbox: list[WorkspaceInboxEntry] | None = None
    tasks: list[WorkspaceTaskRecord] | None = None
    tasksHistoryLimit: StrictInt | None = None
    monitors: list[MonitorEntityDto] | None = None


class PanelLayoutReport(ContractModel):
    client: StrictStr
    sequence: StrictInt
    renderer: StrictStr
    # Older layout feedback may include this redundant value. New reports do
    # not: their authoritative revision is stored on PanelLayoutResult.
    revision: StrictStr | None = None
    sha256: StrictStr | None = None
    width: StrictInt | StrictFloat | None = None
    height: StrictInt | StrictFloat | None = None
    contentWidth: StrictInt | StrictFloat | None = None
    contentHeight: StrictInt | StrictFloat | None = None
    overflowX: StrictInt | StrictFloat | None = None
    overflowY: StrictInt | StrictFloat | None = None
    totalLines: StrictInt | None = None
    visibleLines: StrictInt | None = None
    lastVisibleLine: StrictStr | None = None
    lastVisibleHeading: StrictStr | None = None
    fits: StrictBool
    reason: Literal["overflow", "unsupported"] | None
    measuredAt: StrictInt | StrictFloat | None = None
    expiresAt: StrictInt | StrictFloat | None = None


class PanelLayoutResult(ResponseModel):
    version: StrictInt
    agent: StrictStr
    revision: StrictStr
    sha256: StrictStr
    status: Literal["fits", "does_not_fit", "unmeasured", "empty", "clipped"]
    updatedAt: StrictInt | StrictFloat | None = None
    reports: list[PanelLayoutReport]


class BrowserForward(ContractModel):
    """The redirect port of a sign-in that a paired server approved for a window on this Mac."""
    server: StrictStr = Field(min_length=1, max_length=200)
    bridgeId: StrictStr = Field(pattern=r"^[0-9a-f]{32}$")
    host: Literal["127.0.0.1", "localhost", "::1"]
    port: StrictInt = Field(ge=1024, le=65535)
    path: StrictStr = Field(min_length=1, max_length=2048)
    expiresAt: StrictInt | StrictFloat
    token: StrictStr = Field(min_length=16, max_length=200)


class BrowserForwardResult(ResponseModel):
    status: Literal["forwarding"]
    replayed: StrictBool | None = None


class AnswerResult(ResponseModel):
    status: Literal["answered"]
    replayed: StrictBool | None = None
    # An accepted sign-in link (browser/open) that the window opens in the browser.
    openUrl: StrictStr | None = None
    # For a window on another Mac: the port that its own server holds (/api/browser/forward).
    forward: BrowserForward | None = None
