"""Typed HTTP contracts for workspace and messaging routes."""

from __future__ import annotations

from typing import Literal

from pydantic import ConfigDict, Field, RootModel, StrictBool, StrictFloat, StrictInt, StrictStr

from studio_api.models import ContractModel, JsonValue, ResponseModel


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
    request_id: StrictStr


class ChangesQuery(AgentQuery):
    scope: Literal["chat"] | None = None


class OperationBody(ContractModel):
    agent: StrictStr | None = None
    id: StrictStr | None = None


class WorkBody(OperationBody):
    action: Literal["list", "create", "update", "claim", "submit", "cancel", "accept", "reject"]
    task_id: StrictStr | None = None
    version: StrictInt | None = None
    title: StrictStr | None = None
    description: StrictStr | None = None
    owner: StrictStr | None = None
    dependencies: list[StrictStr] | None = None
    status: Literal["ready", "blocked"] | None = None
    result: StrictStr | None = None
    checks: StrictStr | None = None
    revision: StrictStr | None = None
    files: list[StrictStr] | None = None
    reason: StrictStr | None = None


class QueueBody(OperationBody):
    action: Literal["cancel", "edit", "first", "reorder"]
    request_id: StrictStr | None = None
    message_id: StrictStr | None = None
    expected_revision: StrictStr | None = None
    expectedText: StrictStr | None = None
    text: StrictStr | None = None
    ordered_ids: list[StrictStr] | None = None


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
    name: StrictStr
    members: list[StrictStr]


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
    text: StrictStr
    complaint_id: StrictStr | None = None
    status: Literal["in_progress", "resolved", "declined"] | None = None
    version: StrictInt | None = None


class ProfileBody(ContractModel):
    action: Literal["save", "delete"] = "save"
    isNew: StrictBool | None = None
    id: StrictStr | None = None
    name: StrictStr | None = None
    role: Literal["reviewer", "implementer"] = "reviewer"
    model: StrictStr | None = None
    effort: Literal["low", "medium", "high", "xhigh", "max", "ultra"] | None = None
    instructions: StrictStr = ""


class RuleBody(ContractModel):
    action: Literal["save", "pause", "resume", "delete"] = "save"
    agent: StrictStr
    isNew: StrictBool | None = None
    id: StrictStr | None = None
    rootId: StrictStr | None = None
    epoch: StrictInt | None = None
    name: StrictStr | None = None
    kind: Literal["interval", "once", "file", "event", "low_workers"] = "interval"
    intervalSeconds: StrictInt = 60
    stallTimeoutSeconds: StrictInt = 1800
    stall_timeout_seconds: StrictInt | None = None
    at: StrictInt | StrictFloat | None = None
    nextAt: StrictInt | StrictFloat | None = None
    event: Literal["worker_completed", "monitor_exit", "work_review", "complaint"] = "worker_completed"
    command: StrictStr = ""
    text: StrictStr = ""
    path: StrictStr | None = None
    livenessCommand: StrictStr = ""
    minimumWorkers: StrictInt = 8
    durationMinutes: StrictInt = 30
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
    text: StrictStr
    kind: Literal["user", "followup"]
    status: Literal["pending"]
    created: StrictInt | StrictFloat
    assets: list[JsonValue] = Field(default_factory=list)
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


class PlanView(ResponseModel):
    id: StrictStr
    rootId: StrictStr
    text: StrictStr
    version: StrictInt
    updated: StrictInt | StrictFloat | None
    steps: list[JsonValue] = Field(default_factory=list)


class AgentRequest(ResponseModel):
    id: StrictStr
    agent: StrictStr
    method: StrictStr
    status: Literal["pending", "answered", "answering", "uncertain", "expired"]
    createdAt: StrictInt | StrictFloat | None = None
    answeredAt: StrictInt | StrictFloat | None = None
    answeredBy: StrictStr | None = None
    decision: StrictStr | None = None
    deferred: StrictBool | None = None
    deferredAt: StrictInt | StrictFloat | None = None
    deferredBy: StrictStr | None = None
    questions: list[JsonValue] = Field(default_factory=list)
    answerHistory: list[JsonValue] | None = None
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


class ChatRead(ResponseModel):
    room: JsonValue
    messages: list[JsonValue]
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


class ToolRequestList(ResponseModel):
    requests: list[VersionedRuntimeRecord]


class RuleList(ResponseModel):
    rules: list[VersionedRuntimeRecord]


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


class WorkspaceTaskFeed(ResponseModel):
    tasks: list[VersionedRuntimeRecord]
    cursor: JsonValue | None = None
    hasMore: StrictBool
    hasMoreChanges: StrictBool | None = None
    nextBefore: JsonValue | None = None
    reset: StrictBool | None = None


class ChatReceipt(ResponseModel):
    id: StrictStr
    connected: StrictBool


class ChatCreated(ResponseModel):
    id: StrictStr


class WorkspaceView(ResponseModel):
    work: list[WorkItem] | None = None
    annotations: list[VersionedRuntimeRecord] | None = None
    checkpoints: list[VersionedRuntimeRecord] | None = None
    plans: list[PlanView] | None = None
    rules: list[VersionedRuntimeRecord] | None = None
    inbox: list[VersionedRuntimeRecord] | None = None
    tasks: list[VersionedRuntimeRecord] | None = None
    tasksHistoryLimit: StrictInt | None = None
    monitors: list[VersionedRuntimeRecord] | None = None


class PanelLayoutReport(ContractModel):
    client: StrictStr
    sequence: StrictInt
    renderer: StrictStr
    revision: StrictStr
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
