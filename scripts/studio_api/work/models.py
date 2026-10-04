"""Typed HTTP contracts for workspace and messaging routes."""

from __future__ import annotations

from typing import Annotated, Literal

from pydantic import ConfigDict, Field, RootModel, StrictBool, StrictFloat, StrictInt, StrictStr, model_validator

from studio_api.models import ContractModel, JsonValue, ResponseModel
from studio_api.history.models import CheckpointSummary, TranscriptAsset
from studio_api.sync.models import (
    MonitorEntityDto,
    ComplaintResponseDto,
    RuleKind,
    RuleStatus,
    SnapshotComplaintDto,
    SnapshotRoomDto,
    SnapshotTaskDto,
    TaskKind,
    TaskStatus,
    TaskEntityDto,
)


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
    id: StrictStr
    decision: Literal["answer", "accept", "decline", "cancel"] | None = None
    answers: dict[StrictStr, JsonValue] | None = None
    content: dict[StrictStr, JsonValue] | None = None
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
    results: list[JsonValue] = Field(default_factory=list)
    decisions: list[JsonValue] = Field(default_factory=list)
    createdBy: StrictStr | None = None
    blockedBy: list[StrictStr] = Field(default_factory=list)
    displayStatus: StrictStr | None = None
    archive: JsonValue | None = None
    archivePending: StrictBool | None = None
    archiveIntent: JsonValue | None = None
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
    """Known fields from native turn/plan/updated provider notifications."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue]
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    explanation: StrictStr | None = None
    plan: list[JsonValue] = Field(default_factory=list)


class PlanView(ResponseModel):
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
    room: SnapshotRoomDto
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


class VersionedRuntimeRecord(ResponseModel):
    """Typed runtime record; its versioned domain fields remain JSON-valued extras."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue]
    id: StrictStr | None = None
    agent: StrictStr | None = None
    rootId: StrictStr | None = None
    status: StrictStr | None = None
    kind: StrictStr | None = None
    created: StrictInt | StrictFloat | None = None
    updated: StrictInt | StrictFloat | None = None


class RuntimeRecords(ResponseModel):
    items: list[VersionedRuntimeRecord]


class AnnotationRecord(ContractModel):
    id: StrictStr
    agent: StrictStr
    rootId: StrictStr
    path: StrictStr
    line: StrictInt
    text: StrictStr
    created: StrictInt | StrictFloat
    turnId: StrictStr | None = None


class RuleRestartCheck(ContractModel):
    epoch: StrictInt
    checks: StrictInt
    monitorId: StrictStr


class RuleRecord(ContractModel):
    id: StrictStr
    agent: StrictStr | None = None
    rootId: StrictStr | None = None
    name: StrictStr | None = None
    description: StrictStr | None = None
    enabled: StrictBool | None = None
    epoch: StrictInt | None = None
    kind: RuleKind | None = None
    intervalSeconds: StrictInt | None = None
    nextAt: StrictInt | StrictFloat | None = None
    at: StrictInt | StrictFloat | None = None
    path: StrictStr | None = None
    event: StrictStr | None = None
    command: StrictStr | None = None
    stallTimeoutSeconds: StrictInt | None = None
    livenessCommand: StrictStr | None = None
    fileActivityAt: StrictInt | StrictFloat | None = None
    fileGeneration: StrictInt | None = None
    stallWakeGeneration: StrictInt | None = None
    text: StrictStr | None = None
    status: RuleStatus | None = None
    created: StrictInt | StrictFloat | None = None
    inFlight: StrictBool | None = None
    checks: StrictInt | None = None
    wakes: StrictInt | None = None
    fingerprint: list[StrictInt] | None = None
    minimumWorkers: StrictInt | None = None
    durationMinutes: StrictInt | None = None
    lowSince: StrictInt | StrictFloat | None = None
    alerted: StrictBool | None = None
    error: StrictStr | None = None
    restartCheck: RuleRestartCheck | None = None
    lastAt: StrictInt | StrictFloat | None = None
    lastExitCode: StrictInt | None = None
    lastOutput: StrictStr | None = None
    lastFinished: StrictInt | StrictFloat | None = None
    activeWorkers: StrictInt | None = None
    updated: StrictInt | StrictFloat | None = None
    stall_timeout_seconds: StrictInt | None = None
    eventText: StrictStr | None = None
    stallProbe: StrictBool | None = None
    stallEventKey: StrictStr | None = None
    stallText: StrictStr | None = None
    lastStallFinished: StrictInt | StrictFloat | None = None
    lastStallExitCode: StrictInt | None = None
    lastStallError: StrictStr | None = None
    lastEvent: StrictStr | None = None


class AnnotationReceipt(AnnotationRecord, ResponseModel):
    pass


class ComplaintDetailResponse(SnapshotComplaintDto, ResponseModel):
    id: StrictStr
    version: StrictInt
    text: StrictStr
    responses: list[ComplaintResponseDto]


class TaskDetailResponse(SnapshotTaskDto, ResponseModel):
    type: StrictStr | None = None
    itemId: StrictStr | None = None
    startedAtMs: StrictInt | StrictFloat | None = None
    completedAtMs: StrictInt | StrictFloat | None = None
    server: StrictStr | None = None
    outputTruncated: StrictBool | None = None


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
    requests: list[VersionedRuntimeRecord]


class RuleList(ResponseModel):
    rules: list[RuleRecord]


class RuleMutation(ResponseModel):
    deleted: StrictStr | None = None
    id: StrictStr | None = None
    agent: StrictStr | None = None
    rootId: StrictStr | None = None
    name: StrictStr | None = None
    kind: StrictStr | None = None
    status: StrictStr | None = None
    created: StrictInt | StrictFloat | None = None
    updated: StrictInt | StrictFloat | None = None
    command: StrictStr | None = None
    error: StrictStr | None = None
    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue]


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
    inbox: list[VersionedRuntimeRecord] | None = None
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


class AnswerResult(ResponseModel):
    status: Literal["answered"]
    replayed: StrictBool | None = None
