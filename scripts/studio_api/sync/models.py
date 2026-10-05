"""Strict public DTOs for the durable sync projection and its streams."""

from __future__ import annotations

from typing import Annotated, Literal
from pydantic import ConfigDict, Field, TypeAdapter, field_validator, model_validator

from studio_api.system.models import SupervisorIdentity

from studio_api.models import (
    ContractModel,
    ContractStrEnum,
    JsonValue,
    ResponseModel,
    SyncEntity as SyncEntity,
)


class AgentStatus(ContractStrEnum):
    UNKNOWN = "unknown"
    IDLE = "idle"
    QUEUED = "queued"
    STARTING = "starting"
    RUNNING = "running"
    WAITING = "waiting"
    APPROVAL = "approval"
    COMPLETED = "completed"
    INTERRUPTED = "interrupted"
    FAILED = "failed"
    BLOCKED = "blocked"
    PAUSED = "paused"
    PARKED = "parked"
    ABANDONED = "abandoned"
    CAPACITY_RETRY = "capacity-retry"


class AgentSource(ContractStrEnum):
    MANAGED = "managed"
    REGISTERED = "registered"
    APP_SERVER = "app-server"
    ORCHESTRATOR_REFERENCE = "orchestrator-reference"


class AgentProvider(ContractStrEnum):
    CODEX = "codex"
    CLAUDE = "claude"


class AgentRole(ContractStrEnum):
    LEAD = "lead"
    WORKER = "worker"
    ORCHESTRATOR = "orchestrator"
    AGENT = "agent"
    IMPLEMENTER = "implementer"
    REVIEWER = "reviewer"


class GoalStatus(ContractStrEnum):
    ACTIVE = "active"
    COMPLETE = "complete"
    BLOCKED = "blocked"
    PAUSED = "paused"
    BUDGET_LIMITED = "budgetLimited"
    USAGE_LIMITED = "usageLimited"


class AgentMode(ContractStrEnum):
    MULTI = "multi"
    SINGLE = "single"


class AgentActivity(ContractModel):
    phase: str | None = None
    at: float | None = None
    tools: list[ActiveToolDto] | None = None


class NativeProviderError(ContractModel):
    """Known app-server error fields plus JSON-safe provider extensions."""
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    code: int | str | None = None
    message: str | None = None
    data: JsonValue | None = None
    codexErrorInfo: str | dict[str, JsonValue] | list[JsonValue] | None = None
    additionalDetails: str | None = None


class AgentNativeStatus(ContractModel):
    phase: Literal["safety", "retrying", "auth"] | None = None
    error: NativeProviderError | str | None = None
    message: str | None = None
    turnId: str | None = None
    at: float | None = None


class NativeThreadBlockDto(ContractModel):
    threadId: str | None = None
    error: NativeProviderError | None = None


class NativeTurnErrorDto(ContractModel):
    turnId: str | None = None
    error: NativeProviderError | None = None


class NativeSafetyStage(ContractStrEnum):
    TURNS = "turns"
    ITEMS = "items"
    INTERRUPT = "interrupt"
    VERIFY_TURNS = "verify_turns"
    VERIFY_ITEMS = "verify_items"
    FORK = "fork"
    START = "start"
    UNKNOWN = "unknown"
    RUNNING = "running"
    FAILED = "failed"
    CANCELLED = "cancelled"


class NativeSafetyRetryDto(ContractModel):
    id: str | None = None
    stage: NativeSafetyStage | None = None
    model: str | None = None
    turnId: str | None = None
    epoch: int | None = None
    accountKey: str | None = None
    created: float | None = None
    updated: float | None = None
    error: str | None = None
    newThreadId: str | None = None
    acceptedTurnId: str | None = None
    requestId: str | None = None
    rpcMethod: str | None = None


class NativeSafetyBufferingDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    turnId: str | None = None
    threadId: str | None = None
    accountKey: str | None = None
    connectionId: str | None = None
    at: float | None = None
    dismissed: bool | None = None
    responseStarted: bool | None = None
    showBufferingUi: bool | None = None
    fasterModel: str | None = None


class ContextUsageDto(ContractModel):
    tokens: int | None
    window: int | None
    at: float | None = None


class ReadStateDto(ContractModel):
    threadId: str
    turnId: str
    read: bool
    revision: int


class ContextRepairWaitDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    error: str
    scope: str
    at: float | None = None
    readOnly: bool | None = None
    action: str | None = None


class ConnectionCheckDto(ContractModel):
    epoch: int | None = None
    accountKey: str | None = None
    threadId: str | None = None
    turnId: str | None = None
    at: float
    previousError: str | None = None
    nativeState: str | None = None
    restartTurnStatus: str | None = None
    readError: str | None = None


class CapacityRetryCause(ContractStrEnum):
    SERVER_OVERLOADED = "serverOverloaded"
    INTERNAL_SERVER_ERROR = "internalServerError"
    HTTP_CONNECTION_FAILED = "httpConnectionFailed"
    RESPONSE_STREAM_CONNECTION_FAILED = "responseStreamConnectionFailed"
    RESPONSE_STREAM_DISCONNECTED = "responseStreamDisconnected"
    RESPONSE_TOO_MANY_FAILED_ATTEMPTS = "responseTooManyFailedAttempts"


class CapacityRetryStatus(ContractStrEnum):
    SCHEDULED = "scheduled"
    STARTING = "starting"
    FINISHED = "finished"
    CANCELLED = "cancelled"
    UNKNOWN = "unknown"
    EXHAUSTED = "exhausted"
    FAILED = "failed"


class CapacityRetryDto(ContractModel):
    id: str | None = None
    threadId: str | None = None
    turnId: str | None = None
    accountKey: str | None = None
    epoch: int | None = None
    cause: CapacityRetryCause | None = None
    status: CapacityRetryStatus | None = None
    dueAt: float | None = None
    retryAt: float | None = None
    acceptedTurnId: str | None = None
    claimedAt: float | None = None
    attempt: int | None = None
    maxAttempts: int | None = None
    cwd: str | None = None
    settings: CapacityRetrySettingsDto | None = None
    updatedAt: float | None = None
    waits: int | None = None
    taskClaims: list[str] | None = None
    reason: str | None = None


class UsageResumeCause(ContractStrEnum):
    USAGE_LIMIT = "usage_limit"
    RATE_LIMIT = "rate_limit"
    AUTH = "auth"


class UsageResumeStatus(ContractStrEnum):
    SCHEDULED = "scheduled"
    STARTED = "started"
    CANCELLED = "cancelled"
    UNKNOWN = "unknown"


class UsageResumeDto(ContractModel):
    id: str | None = None
    status: UsageResumeStatus | None = None
    accountKey: str | None = None
    threadId: str | None = None
    epoch: int | None = None
    turnId: str | None = None
    cause: UsageResumeCause | None = None
    failedAt: float | None = None
    authAttempt: int | None = None
    authRefreshMarker: str | None = None
    dueAt: float | None = None
    plannedAt: float | None = None
    resetAt: float | None = None
    proofAt: float | None = None
    startedAt: float | None = None
    lastCheckedAt: float | None = None
    waitingForAuth: bool | None = None
    reason: str | None = None
    updatedAt: float | None = None
    taskClaims: list[str] | None = None


class RequestQuestionOptionDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    label: str | None = None
    value: JsonValue | None = None
    description: str | None = None


class RequestQuestionDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    id: str
    question: str
    options: list[RequestQuestionOptionDto] | None = None
    multiSelect: bool | None = None
    isSecret: bool | None = None


class RequestSchemaPropertyDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    title: str | None = None
    type: str | list[str] | None = None
    enum: list[JsonValue] | None = None
    items: RequestSchemaPropertyDto | None = None
    isSecret: bool | None = None
    writeOnly: bool | None = None
    format: str | None = None


class RequestSchemaDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    properties: dict[str, RequestSchemaPropertyDto]
    required: list[str] | None = None


class RequestParamsDto(ContractModel):
    """Known request UI fields and typed JSON-safe native protocol extensions."""
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    threadId: str | None = None
    turnId: str | None = None
    questions: list[RequestQuestionDto] | None = None
    requestedSchema: RequestSchemaDto | None = None
    reason: str | None = None
    message: str | None = None
    command: str | list[str] | None = None
    cwd: str | None = None
    permissions: dict[str, JsonValue] | None = None
    changes: JsonValue | None = None
    url: str | None = None


class RequestPreviewDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    command: str | list[str] | None = None
    cwd: str | None = None
    permissions: dict[str, JsonValue] | None = None
    changes: JsonValue | None = None
    url: str | None = None


class AgentStartAttempt(ContractModel):
    prepareError: str | None = None
    responseError: str | None = None


class NativeReleasePhase(ContractStrEnum):
    CHECKING = "checking"
    UNSUBSCRIBING = "unsubscribing"
    RELEASED = "released"
    UNKNOWN = "unknown"
    BLOCKED = "blocked"
    RESUMED = "resumed"


class AgentNativeRelease(ContractModel):
    phase: NativeReleasePhase | None = None
    resetPending: bool | None = None


class AgentNativeStatusValue(ContractStrEnum):
    IDLE = "idle"
    NOT_LOADED = "notLoaded"
    UNSUBSCRIBED = "unsubscribed"
    NOT_SUBSCRIBED = "notSubscribed"


class ActiveToolDto(ContractModel):
    id: str
    type: str
    name: str


class SnapshotNativeRelease(AgentNativeRelease):
    id: str | None = None
    threadId: str | None = None
    accountKey: str | None = None
    connectionId: str | None = None
    at: float | None = None
    submittedAt: float | None = None
    resumedAt: float | None = None
    releasedAt: float | None = None
    closedAt: float | None = None
    nativeStatus: AgentNativeStatusValue | None = None
    error: str | None = None
    resetReason: str | None = None
    resetBy: str | None = None


class WorkerDefaultsDto(ContractModel):
    model: str | None
    effort: str | None
    fastMode: bool
    daybreakEnabled: bool = False
    accountKey: str | None = None
    cyberAccessProgram: str | None = None


class ReviewDefaultsDto(ContractModel):
    model: str | None
    effort: str | None


class ExecutionSettingsDto(ContractModel):
    model: str | None = None
    effort: str | None = None
    nativeEffort: str | None = None
    fastMode: bool | None = None
    daybreakEnabled: bool | None = None
    cyberAccessProgram: str | None = None
    accountKey: str | None = None
    updatedAt: float | None = None


class AccountTransferScope(ContractStrEnum):
    TEAM = "team"
    SUBAGENTS = "subagents"


class AccountTransferStatus(ContractStrEnum):
    PENDING = "pending"
    COMPLETED = "completed"
    CANCELLED = "cancelled"


class AccountTransferInterruptedDto(ContractModel):
    id: str
    name: str | None = None
    reason: str | None = None


class AccountTransferLeftOnSourceDto(ContractModel):
    id: str
    name: str | None = None
    provider: AgentProvider | None = None
    reason: str | None = None


class AccountTransferBlockedDto(ContractModel):
    id: str
    name: str | None = None
    reason: str | None = None


class AccountTransferSummaryDto(ContractModel):
    """Public summary projected onto an agent by AccountTransfers.save."""

    id: str | None = None
    targetAccountKey: str | None = None
    status: AccountTransferStatus | None = None
    updated: float | None = None
    scope: AccountTransferScope | None = None
    finishHistory: bool | None = None
    total: int | None = None
    completed: int | None = None
    moved: int | None = None
    nativeHistoryPending: int | None = None
    movingNow: int | None = None
    canFinishHistory: bool | None = None
    interrupted: list[AccountTransferInterruptedDto] | None = None
    leftOnSource: list[AccountTransferLeftOnSourceDto] | None = None
    blocked: list[AccountTransferBlockedDto] | None = None
    waitingCount: int | None = None
    waiting: str | None = None
    needsAttention: bool | None = None
    canRetry: bool | None = None


class ConvertedFromLeadDto(ContractModel):
    """User conversion provenance persisted when a peer lead becomes a worker."""

    requestId: str
    by: Literal["user"]
    at: float
    oldRootId: str
    rootId: str


class CapacityRetrySettingsDto(ExecutionSettingsDto):
    """Settings captured by Runtime.preparation_settings for a retry receipt."""

    yoloMode: bool | None = None
    profileInstructions: str | None = None
    role: AgentRole | None = None


class NativeActionIdentityDto(ContractModel):
    accountKey: str
    threadId: str
    epoch: int


class SnapshotModelSettingsDto(ContractModel):
    id: str
    epoch: int
    accountKey: str
    threadId: str
    connectionId: str
    settings: CapacityRetrySettingsDto
    status: Literal["submitted", "acknowledged"]


class ReviewUncommittedTargetDto(ContractModel):
    type: Literal["uncommittedChanges"]


class ReviewBaseBranchTargetDto(ContractModel):
    type: Literal["baseBranch"]
    branch: str


class ReviewCommitTargetDto(ContractModel):
    type: Literal["commit"]
    sha: str
    title: str | None = None


class ReviewCustomTargetDto(ContractModel):
    type: Literal["custom"]
    instructions: str


ReviewTargetDto = Annotated[
    ReviewUncommittedTargetDto
    | ReviewBaseBranchTargetDto
    | ReviewCommitTargetDto
    | ReviewCustomTargetDto,
    Field(discriminator="type"),
]


class SnapshotStartAttempt(AgentStartAttempt):
    supervisorIdentity: SupervisorIdentity | None = None
    id: str | None = None
    epoch: int | None = None
    events: list[str] | None = None
    action: Literal["review", "capacity", "compact", "safety"] | None = None
    submitted: bool | None = None
    activeAtReservation: bool | None = None
    turnId: str | None = None
    observedTurnId: str | None = None
    nativeOperationId: str | None = None
    accountKey: str | None = None
    connectionId: str | None = None
    threadId: str | None = None
    created: float | None = None
    settingsFixed: bool | None = None
    capacityRetryId: str | None = None
    actionRequestId: str | None = None
    actionIdentity: NativeActionIdentityDto | None = None
    reviewTarget: ReviewTargetDto | None = None
    modelSettings: SnapshotModelSettingsDto | None = None
    executionOutcome: Literal["unknown", "unsent", "rejected"] | None = None
    notSubmittedReason: str | None = None
    retiredEvents: list[str] | None = None
    completedAt: float | None = None


class SnapshotNativeToolCatalogDto(ContractModel):
    threadId: str | None = None
    digest: str


class SnapshotNativeNameIdentityDto(ContractModel):
    accountKey: str
    threadId: str
    name: str


class SnapshotNativeNameFailureDto(ContractModel):
    identity: SnapshotNativeNameIdentityDto
    error: str
    attempts: int
    retryAt: float


class AgentOverview(ContractModel):
    task: str | None = None
    taskTruncated: bool | None = None
    result: str | None = None
    resultTruncated: bool | None = None
    resultTurnId: str | None = None
    resultFile: str | None = None


class AgentEntityDto(ContractModel):
    id: str
    name: str | None = None
    manualName: bool | None = None
    status: AgentStatus | None = None
    source: AgentSource | None = None
    kind: Literal["agent"] | None = None
    parentId: str | None = None
    rootId: str | None = None
    threadId: str | None = None
    orchestratorId: str | None = None
    orchestratorName: str | None = None
    isLead: bool | None = None
    role: AgentRole | None = None
    sharedRoomId: str | None = None
    model: str | None = None
    provider: AgentProvider | None = None
    # Reasoning effort levels come from the selected model's capability catalog.
    effort: str | None = None
    fastMode: bool | None = None
    concurrency: int | None = None
    accountKey: str | None = None
    cwd: str | None = None
    worktree: bool | str | None = None
    worktreePreparation: Literal["waiting", "preparing"] | None = None
    imageWorkspace: bool | None = None
    imageWorkspaceReady: bool | None = None
    imageWorkspacePhase: str | None = None
    imageWorkspaceError: str | None = None
    imageWorkspaceRepo: str | None = None
    imageWorkspaceBaseRepo: str | None = None
    imageWorkspaceRelative: str | None = None
    imageWorkspaceStartCommit: str | None = None
    created: float | None = None
    updated: float | None = None
    turnId: str | None = None
    turnStatus: AgentStatus | None = None
    inFlight: bool | None = None
    compactions: int | None = None
    tokensUsed: int | None = None
    contextUsage: ContextUsageDto | None = None
    error: NativeProviderError | str | None = None
    tail: str | None = None
    canSend: bool | None = None
    launcherAlive: bool | None = None
    empty: bool | None = None
    yoloMode: bool | None = None
    agentMode: AgentMode | None = None
    agentModeRevision: int | None = None
    agentModeSupported: bool | None = None
    subagentConcurrencyVersion: int | None = None
    workerDefaults: WorkerDefaultsDto | None = None
    reviewDefaults: ReviewDefaultsDto | None = None
    parkedEvent: str | None = None
    pendingSettings: ExecutionSettingsDto | None = None
    pendingSettingsAccountKey: str | None = None
    queuedSettings: ExecutionSettingsDto | None = None
    quickCreate: JsonValue | None = None
    nativeThreadBlock: NativeThreadBlockDto | None = None
    daybreakEnabled: bool | None = None
    accountTransfer: AccountTransferSummaryDto | None = None
    convertedFromLead: ConvertedFromLeadDto | None = None
    overview: AgentOverview | None = None
    nativeRelease: SnapshotNativeRelease | None = None
    activity: AgentActivity | None = None
    nativeStatus: AgentNativeStatus | AgentNativeStatusValue | None = None
    nativeSafetyBuffering: NativeSafetyBufferingDto | None = None
    nativeSafetyRetry: NativeSafetyRetryDto | None = None
    nativeTurnError: NativeTurnErrorDto | None = None
    connectionCheck: ConnectionCheckDto | None = None
    readState: ReadStateDto | None = None
    nativeLimitErrorAt: float | None = None
    startAttempt: SnapshotStartAttempt | None = None
    panelVersion: int | None = None
    panelDataVersion: int | None = None
    unreadCount: int | None = None
    lastReadAt: float | None = None
    deletedAt: float | None = None
    autoWake: bool | None = None
    voiceState: JsonValue | None = None
    nativeError: str | None = None
    retryAt: float | None = None
    hasUnread: bool | None = None
    hasQuestion: bool | None = None
    hasApproval: bool | None = None
    statusDetail: str | None = None
    lastAnswer: str | None = None
    lastCompletedTurn: str | None = None
    nextTurnSettingsSupported: bool | None = None
    readStateSupported: bool | None = None
    pinned: bool | None = None
    archived: bool | None = None
    projectFolder: str | None = None
    projectFolderRevision: int | None = None
    project: str | None = None


class StartOutcomeHoldDto(ContractModel):
    stage: Literal["held"]
    at: float
    attemptId: str
    threadId: str
    connectionId: str
    evidence: Literal["complete_history_absent_idle_twice_journal_drained"]


class TurnRecoveryDto(ContractModel):
    at: float
    turnId: str | None
    outcome: Literal["input_absent", "idle", "completed", "failed", "interrupted"]
    source: Literal["replaced_native_child", "native_thread_read"]
    attemptId: str | None = None
    latestTurnId: str | None = None


class SnapshotAgentDto(AgentEntityDto):
    """Full renderer snapshot agent, including named runtime/native metadata."""

    imageWorkspaceRelative: str | None = None
    imageWorkspaceStartCommit: str | None = None
    imageWorkspaceSnapshotCommit: str | None = None
    imageWorkspaceMount: str | None = None
    imageWorkspaceNoticeSent: str | None = None
    imageWorkspaceNoticeText: str | None = None
    imageWorkspaceNoticeError: str | None = None
    imageWorkspaceCollect: dict[str, JsonValue] | None = None
    startOutcomeHold: StartOutcomeHoldDto | None = None
    turnRecovery: TurnRecoveryDto | None = None

    kind: Literal["agent"]
    wave: str | None = None
    runId: str | None = None
    launcherPid: int | None = None
    launcherAlive: bool | None = None
    events: int | None = None
    graphAlias: str | None = None
    reportedAt: float | None = None
    epoch: int | None = None
    turnEpoch: int | None = None
    maxAgents: int | None = None
    maxAgentsExplicit: bool | None = None
    tokenBudget: int | None = None
    usageResumeEnabled: bool | None = None
    compactionsObservedOnly: bool | None = None
    profileId: str | None = None
    profileInstructions: str | None = None
    worktreeReady: bool | None = None
    worktreeWarning: str | None = None
    checkpointError: str | None = None
    tokenUsageAccounting: Literal["provisional", "responseRecords"] | None = None
    workerBaseRef: str | None = None
    workerBaseCommit: str | None = None
    workerBaseBehindMain: bool | None = None
    workerBaseMainRef: str | None = None
    nativeEffort: str | None = None
    needsTitle: bool | None = None
    quickCreateRequest: str | None = None
    contextRepair: JsonValue | None = None
    contextRepairHistory: list[JsonValue] | None = None
    lastContextRepairCheck: dict[str, JsonValue] | None = None
    lastContextRepairWait: dict[str, JsonValue] | None = None
    connectionRecovery: dict[str, JsonValue] | None = None
    nativeNameSynced: bool | None = None
    capacity: JsonValue | None = None
    transfer: JsonValue | None = None
    accountTransferState: JsonValue | None = None
    nativeFailureHold: JsonValue | None = None
    supervisorRestore: JsonValue | None = None
    lastCompletedTurnStatus: AgentStatus | None = None
    activityPhase: str | None = None
    nativeToolCatalog: SnapshotNativeToolCatalogDto | None = None
    nativeNameFailure: SnapshotNativeNameFailureDto | None = None
    executionSettingsAccountKey: str | None = None
    accountTransferId: str | None = None
    accountId: str | None = None
    acceptedAt: float | None = None
    importedFrom: str | None = None
    activeTools: JsonValue | None = None
    answers: JsonValue | None = None
    assets: list[str] | None = None
    branch: JsonValue | None = None
    browserRecovery: JsonValue | None = None
    budgetActionWait: JsonValue | None = None
    budgetBlocked: JsonValue | None = None
    cancelledPark: JsonValue | None = None
    capacityRetry: CapacityRetryDto | None = None
    usageResume: UsageResumeDto | None = None
    claudeOptions: JsonValue | None = None
    complaintMisses: int | None = None
    complaintsPresented: list[str] | None = None
    content: str | None = None
    contextRepairWait: ContextRepairWaitDto | None = None
    cyberAccessProgram: str | None = None
    decision: str | None = None
    delivery: JsonValue | None = None
    disconnectRecovery: JsonValue | None = None
    expectedModeRevision: int | None = None
    lastCompletedTurnError: NativeProviderError | str | None = None
    lastEvent: str | None = None
    lastUpdated: float | None = None
    lazyAccountTransfer: JsonValue | None = None
    liveSteerAttempt: JsonValue | None = None
    liveSteerRejectedTurnId: str | None = None
    livenessCommand: str | None = None
    nativeReview: JsonValue | None = None
    nativeSafetyRetry: NativeSafetyRetryDto | None = None
    nativeToolRefreshId: str | None = None
    nativeToolUpdate: JsonValue | None = None
    nextTurn: JsonValue | None = None
    overviewFile: str | None = None
    portableHistory: JsonValue | None = None
    preparedContext: JsonValue | None = None
    prepareAttempt: JsonValue | None = None
    previous: JsonValue | None = None
    queueNotice: JsonValue | None = None
    rateLimitResetCredits: JsonValue | None = None
    rateLimits: JsonValue | None = None
    rateLimitsByLimitId: JsonValue | None = None
    requestId: str | None = None
    restartRecovery: JsonValue | None = None
    restoredCheckpoint: JsonValue | None = None
    reuseEmpty: bool | None = None
    signedIn: bool | None = None
    steerRejectedTurnId: str | None = None
    transcriptItemId: str | None = None
    version: int | None = None
    workspaceOperation: JsonValue | None = None
    agentOwner: str | None = None
    goalStatus: GoalStatus | None = None
    requestedModel: str | None = None
    capacityRetries: int | None = None
    pendingSubmission: JsonValue | None = None
    lastTurnStatus: str | None = None
    lastCompletedTurnId: str | None = None
    currentMessageId: str | None = None
    mailboxError: str | None = None
    agentArchive: JsonValue | None = None
    command: str | None = None
    interactive: bool | None = None
    modelEventProjection: JsonValue | None = None
    requestedDelivery: str | None = None


class RoomKind(ContractStrEnum):
    PRIVATE = "private"
    BROADCAST = "broadcast"
    FEDERATED = "federated"


class RadioStatus(ContractStrEnum):
    IDLE = "idle"
    WAITING = "waiting"
    SPEAKING = "speaking"
    STOPPING = "stopping"
    BLOCKED = "blocked"


class RoomLastMessage(ContractModel):
    seq: int
    text: str
    sender: str
    created: float


class _RoomRadioIdentity(ContractModel):
    identity: tuple[str | None, int | None, int]

    @field_validator("identity", mode="before")
    @classmethod
    def accept_runtime_identity(cls, value: object) -> object:
        # codex_radio persists this fixed tuple in JSON, which is read back as a list.
        return tuple(value) if isinstance(value, list) else value


class RoomRadioActive(_RoomRadioIdentity):
    eventId: str
    agentId: str
    epoch: int
    threadId: str | None
    through: int
    turnId: str | None = None
    interruptRequested: bool | None = None
    questionContinuationPlanned: bool | None = None


class RoomRadioSeen(_RoomRadioIdentity):
    """Per-agent transcript cursor persisted by the shared-radio runtime."""

    seq: int


class RoomRadio(ContractModel):
    direct: bool | None = None
    teamId: str
    revision: int
    status: RadioStatus
    speaker: str | None
    next: list[str]
    active: RoomRadioActive | None
    error: str | None
    seen: dict[str, RoomRadioSeen] | None = None


class RoomEntityDto(ContractModel):
    id: str
    name: str | None = None
    kind: RoomKind | None = None
    members: list[str] | None = None
    rootId: str | None = None
    updated: float | None = None
    userHidden: bool | None = None
    projectPath: str | None = None
    radio: RoomRadio | None = None
    peerTeamId: str | None = None
    peerTeamName: str | None = None
    lastMessage: RoomLastMessage | None = None


class RemoteRoomMember(ContractModel):
    id: str
    name: str | None = None
    role: str | None = None
    status: str | None = None


class LocalRoomParticipantDto(ContractModel):
    id: str
    role: Literal["lead", "agent"]
    name: str | None = None
    status: str | None = None


class SnapshotRoomDto(RoomEntityDto):
    federated: bool | None = None
    peerId: str | None = None
    peerLabel: str | None = None
    localMembers: list[str] | None = None
    remoteMembers: list[RemoteRoomMember] | None = None
    localParticipants: list[LocalRoomParticipantDto] | None = None
    customName: str | None = None


class TaskStatus(ContractStrEnum):
    RUNNING = "running"
    STARTING = "starting"
    APPROVAL = "approval"
    QUEUED = "queued"
    WAITING = "waiting"
    COMPLETED = "completed"
    FAILED = "failed"
    INTERRUPTED = "interrupted"
    CANCELLED = "cancelled"
    LOST = "lost"
    UNKNOWN = "unknown"


class TaskKind(ContractStrEnum):
    MONITOR = "monitor"
    COMMAND = "command"
    TOOL = "tool"


class TaskEntityDto(ContractModel):
    id: str
    turnId: str | None = None
    agent: str | None = None
    kind: TaskKind | None = None
    status: TaskStatus | None = None
    created: float | None = None
    finished: float | None = None
    name: str | None = None
    command: str | None = None
    query: str | None = None
    cwd: str | None = None
    processId: str | None = None
    durationMs: int | float | None = None
    timeout_ms: int | None = None
    interactive: bool | None = None
    stdinClosed: bool | None = None
    stdinCloseRequested: str | None = None
    stdinError: str | None = None
    cancelRequested: bool | None = None
    exitCode: int | None = None
    bytes: int | None = None
    log: str | None = None
    outputTruncated: bool | None = None


class SnapshotTaskDto(TaskEntityDto):
    agent: str
    kind: TaskKind
    status: TaskStatus
    created: float
    type: str | None = None
    itemId: str | None = None
    server: str | None = None
    startedAtMs: float | None = None
    completedAtMs: float | None = None
    arguments: str | None = None
    tail: str | None = None
    error: str | None = None


class MonitorEntityDto(ContractModel):
    id: str
    agent: str
    status: TaskStatus | None = None
    created: float
    finished: float | None = None
    name: str | None = None
    command: str | None = None
    cwd: str | None = None
    processId: str | None = None
    durationMs: int | None = None
    timeout_ms: int | None = None
    interactive: bool | None = None
    stdinClosed: bool | None = None
    stdinCloseRequested: str | None = None
    stdinError: str | None = None
    cancelRequested: bool | None = None
    exitCode: int | None = None
    tail: str | None = None
    error: str | None = None
    bytes: int | None = None
    log: str | None = None
    outputTruncated: bool | None = None
    ruleId: str | None = None


class ComplaintStatus(ContractStrEnum):
    OPEN = "open"
    IN_PROGRESS = "in_progress"
    ANSWERED = "answered"
    RESOLVED = "resolved"
    DECLINED = "declined"
    STOPPED = "stopped"


class ComplaintEntityDto(ContractModel):
    id: str
    leadId: str | None = None
    author: str | None = None
    authorName: str | None = None
    leadName: str | None = None
    title: str | None = None
    status: ComplaintStatus | None = None
    needsResponse: bool | None = None
    created: float | None = None
    readAt: float | None = None
    leadStopped: bool | None = None
    leadDeleted: bool | None = None
    recipient: Literal["user", "lead"] | None = None
    version: int | None = None


class ComplaintResponseDto(ContractModel):
    id: str
    author: str
    text: str
    status: ComplaintStatus
    at: float


class SnapshotComplaintDto(ComplaintEntityDto):
    updated: float | None = None
    text: str | None = None
    responses: list[ComplaintResponseDto] | None = None


class RequestStatus(ContractStrEnum):
    PENDING = "pending"
    BLOCKED = "blocked"
    ANSWERING = "answering"
    ANSWERED = "answered"
    DECLINED = "declined"
    RESOLVED = "resolved"
    EXPIRED = "expired"
    SUBMITTED = "submitted"
    ACKNOWLEDGED = "acknowledged"
    FAILED = "failed"
    UNCERTAIN = "uncertain"


class RequestEntityDto(ContractModel):
    id: str
    method: str | None = None
    agent: str | None = None
    accountKey: str | None = None
    connectionId: str | None = None
    epoch: int | None = None
    threadId: str | None = None
    turnId: str | None = None
    status: RequestStatus | None = None
    created: float | None = None
    createdAt: float | None = None
    at: float | None = None
    updated: float | None = None
    updatedAt: float | None = None
    deferred: bool | None = None
    error: str | None = None
    result: JsonValue | None = None
    params: RequestParamsDto | None = None
    preview: RequestPreviewDto | None = None
    title: str | None = None


class RuleEntityDto(ContractModel):
    id: str
    agent: str | None = None
    name: str | None = None
    enabled: bool | None = None
    description: str | None = None


class RuleKind(ContractStrEnum):
    INTERVAL = "interval"
    ONCE = "once"
    FILE = "file"
    EVENT = "event"
    LOW_WORKERS = "low_workers"


class RuleStatus(ContractStrEnum):
    ACTIVE = "active"
    PAUSED = "paused"


class RuleSnapshotDto(RuleEntityDto):
    rootId: str | None = None
    epoch: int | None = None
    kind: RuleKind | None = None
    intervalSeconds: int | None = None
    nextAt: float | None = None
    at: float | None = None
    path: str | None = None
    event: str | None = None
    command: str | None = None
    stallTimeoutSeconds: int | None = None
    livenessCommand: str | None = None
    fileActivityAt: float | None = None
    fileGeneration: int | None = None
    stallWakeGeneration: int | None = None
    text: str | None = None
    status: RuleStatus | None = None
    created: float | None = None
    inFlight: bool | None = None
    checks: int | None = None
    wakes: int | None = None
    fingerprint: JsonValue | None = None
    minimumWorkers: int | None = None
    durationMinutes: int | None = None
    lowSince: float | None = None
    alerted: bool | None = None
    error: str | None = None
    restartCheck: JsonValue | None = None


class ProjectFolder(ContractModel):
    id: str
    name: str
    parentId: str | None = None


class ProjectPeerTeamDto(ContractModel):
    id: str
    name: str
    members: list[str]


class ProjectEntityDto(ContractModel):
    id: str
    path: str | None = None
    name: str | None = None
    created: float | None = None
    updated: float | None = None
    accountKey: str | None = None
    accountRevision: int | None = None
    accountKeys: list[str] | None = None
    organizationRevision: int | None = None
    peerTeamsRevision: int | None = None
    folders: list[ProjectFolder] | None = None
    peerTeams: list[ProjectPeerTeamDto] | None = None


class PeerTeamEntityDto(ContractModel):
    id: str
    name: str | None = None
    projectPath: str | None = None
    members: list[str] | None = None
    revision: int | None = None


class ChatEntityDto(ContractModel):
    id: str
    name: str | None = None
    members: list[str] | None = None
    kind: Literal["chat"] | None = None
    messageCount: int | None = None
    tail: str | None = None
    lastMessageAt: float | None = None


class EdgeEntityDto(ContractModel):
    id: str
    source: str | None = None
    target: str | None = None
    kind: Literal["chat", "spawn"] | None = None


class EventEntityDto(ContractModel):
    id: str
    agent: str | None = None
    kind: str | None = None
    status: str | None = None
    created: float | None = None
    error: str | None = None


class WorkEntityDto(ContractModel):
    id: str
    rootId: str | None = None
    agent: str | None = None
    status: Literal["ready", "running", "blocked", "review", "accepted", "cancelled"] | None = None
    title: str | None = None


class WorkResultDto(ContractModel):
    id: str
    agent: str
    text: str
    checks: str
    revision: str
    files: list[str]
    created: float
    runId: str | None = None
    attemptId: str | None = None
    resultFile: str | None = None


class WorkDecisionDto(ContractModel):
    decision: Literal["cancel", "accept", "reject"]
    reason: str
    by: str
    owner: str | None = None
    resultId: str | None = None
    created: float


class WorkSnapshotDto(WorkEntityDto):
    description: str | None = None
    owner: str | None = None
    dependencies: list[str] | None = None
    created: float | None = None
    updated: float | None = None
    version: int | None = None
    results: list[WorkResultDto] | None = None
    decisions: list[WorkDecisionDto] | None = None
    createdBy: str | None = None
    blockedBy: list[str] | None = None
    displayStatus: str | None = None


class RateLimitWindowDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    usedPercent: float | None = None
    resetsAt: float | None = None
    windowDurationMins: float | None = None


class RateLimitBucketDto(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    limitId: str | None = None
    limitName: str | None = None
    planType: str | None = None
    rateLimitReachedType: str | None = None
    primary: RateLimitWindowDto | None = None
    secondary: RateLimitWindowDto | None = None


class RateLimitsDataDto(ContractModel):
    """Known renderer-facing rate-limit fields with provider JSON extensions."""
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    rateLimits: RateLimitBucketDto | None = None
    rateLimitsByLimitId: dict[str, RateLimitBucketDto] | None = None
    rateLimitResetCredits: JsonValue | None = None
    accountId: str | None = None
    status: str | None = None
    signedIn: bool | None = None
    ordinaryUsageAllowed: bool | None = None


class AccountRateLimitsDto(ContractModel):
    """Account-specific read envelope; provider rate-limit data stays JSON."""
    accountKey: str
    at: float | None
    readAt: float | None = None
    data: RateLimitsDataDto | None = None
    error: str | None = None


class NativeNoticeDto(ContractModel):
    """Account notices and provider-version advisories shown in the UI."""
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    id: str | None = None
    accountKey: str | None = None
    connectionId: str | None = None
    provider: str | None = None
    version: str | None = None
    baseline: str | None = None
    message: str | None = None
    details: JsonValue | None = None
    at: float | None = None


class SidebarOrderDto(ContractModel):
    revision: int
    groups: dict[str, list[str]] | None


class WorkspaceEntityDto(ContractModel):
    connected: bool | None = None
    rateLimits: AccountRateLimitsDto | None = None
    rateLimitsByAccount: dict[str, AccountRateLimitsDto] | None = None
    nativeNotices: list[NativeNoticeDto] | None = None
    projectOrganizationVersion: int | None = None
    sidebarOrder: SidebarOrderDto | None = None
    peerTeamsVersion: int | None = None
    tasksHistoryLimit: int | None = None
    stateDir: str | None = None


class SnapshotProjectDto(ProjectEntityDto):
    workerBaseRef: str | None = None
    workerBaseRevision: int | None = None


class SnapshotChatDto(ChatEntityDto):
    pass


class SnapshotEdgeDto(EdgeEntityDto):
    pass


class RuntimeSnapshot(ResponseModel):
    agents: list[SnapshotAgentDto]
    projects: list[SnapshotProjectDto]
    projectOrganizationVersion: int
    sidebarOrder: SidebarOrderDto | None = None
    peerTeamsVersion: int
    peerTeams: list[PeerTeamEntityDto]
    tasks: list[SnapshotTaskDto]
    tasksHistoryLimit: int
    monitors: list[MonitorEntityDto]
    requests: list[RequestEntityDto]
    rooms: list[SnapshotRoomDto]
    complaints: list[SnapshotComplaintDto]
    work: list[WorkSnapshotDto] | None = None
    rules: list[RuleSnapshotDto]
    rateLimits: AccountRateLimitsDto
    nativeNotices: list[NativeNoticeDto]
    rateLimitsByAccount: dict[str, AccountRateLimitsDto]
    events: list[EventEntityDto]
    connected: bool


class SnapshotChatGroupDto(ContractModel):
    id: str
    name: str
    members: list[str]
    kind: Literal["chat"]
    messageCount: int
    tail: str
    lastMessageAt: float | None


SnapshotNodeDto = Annotated[
    SnapshotAgentDto | SnapshotChatGroupDto,
    Field(discriminator="kind"),
]


class StateSnapshot(ResponseModel):
    token: str
    stateDir: str
    threads: list[SnapshotAgentDto]
    chats: list[SnapshotChatGroupDto]
    nodes: list[SnapshotNodeDto]
    edges: list[SnapshotEdgeDto]
    at: float
    runtime: RuntimeSnapshot | None


class SyncProtocolResponse(ResponseModel):
    protocolVersion: Literal[1]
    supportedVersions: list[Literal[1, 2, 3]]
    capabilities: list[str]
    scopes: list[str]
    pullEndpoint: Literal["/api/sync/pull"]
    streamEndpoint: Literal["/api/sync/stream"]
    maxEntityPage: int
    maxOtherPage: int
    maxStreamDocuments: int
    maxStreamBytes: int


class SyncIdentityResponse(ResponseModel):
    workspaceId: str
    syncProtocol: Literal[2]
    chatState: bool | None = None


class SyncStreamQuery(ContractModel):
    model_config = ConfigDict(extra="forbid", strict=False, validate_assignment=True, populate_by_name=True)

    protocol: str | None = None
    scope: str | None = None
    after: int | None = None
    resources: str | None = Field(
        default=None,
        description="Protocol 3 JSON-encoded array of ResourceRef values",
    )


class TranscriptStreamQuery(ContractModel):
    id: str | None = None


class SyncGenerations(ContractModel):
    state: int
    transcripts: int
    drafts: int


class SyncGenerationState(ResponseModel):
    protocol: Literal[2]
    workspaceId: str
    syncProtocol: Literal[2]
    chatState: bool | None = None
    generations: SyncGenerations
    transcriptRevisions: dict[str, int] | None = None


class SyncCheckpoint(ContractModel):
    seq: int


class SyncPullResponse(ResponseModel):
    workspaceId: str
    generation: int
    documents: list[SyncDocument]
    checkpoint: SyncCheckpoint
    reset: Literal[False] | None = None
    floor: int | None = None
    maxSeq: int | None = None
    initialHigh: int | None = None


class SyncPullResetResponse(ResponseModel):
    workspaceId: str
    generation: int
    reset: Literal[True]
    floor: int
    maxSeq: int


class DraftDocumentInput(ContractModel):
    id: str = Field(max_length=300)
    payload: str
    deleted: bool = Field(default=False, alias="_deleted")
    seq: int | None = None


class DraftPayload(ContractModel):
    """Extensible browser-owned draft document; unknown values are JSON metadata."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    device: str
    session: str
    text: str = Field(max_length=2_000_000)
    id: str | None = None
    alternatives: list[str] = Field(default_factory=list)
    updated: int | float | None = None


_DRAFT_ASSUMED_PAYLOAD_ADAPTER: TypeAdapter[JsonValue] = TypeAdapter(JsonValue)


class DraftPushRow(ContractModel):
    newDocumentState: DraftDocumentInput
    assumedMasterState: DraftDocumentInput | None = None

    @field_validator("newDocumentState")
    @classmethod
    def validate_payload(cls, document: DraftDocumentInput) -> DraftDocumentInput:
        DraftPayload.model_validate_json(document.payload)
        return document

    @field_validator("assumedMasterState")
    @classmethod
    def validate_assumed_payload(cls, document: DraftDocumentInput | None) -> DraftDocumentInput | None:
        if document is not None:
            _DRAFT_ASSUMED_PAYLOAD_ADAPTER.validate_json(document.payload)
        return document


class DraftPushRequest(ContractModel):
    rows: list[DraftPushRow] = Field(max_length=100)

    @model_validator(mode="after")
    def validate_identities(self) -> DraftPushRequest:
        for row in self.rows:
            new = row.newDocumentState
            payload = DraftPayload.model_validate_json(new.payload)
            if not payload.device or not payload.session:
                raise ValueError("Draft identity does not match its key")
            key = new.id
            if len(key) > 300:
                raise ValueError("Invalid draft")
            prefix, suffix = f"{payload.device}:", f":{payload.session}"
            writer = key[len(prefix) : -len(suffix)] if key.startswith(prefix) and key.endswith(suffix) else ""
            valid_key = key == f"{payload.device}:{payload.session}" or (bool(writer) and ":" not in writer)
            if not valid_key or (payload.id is not None and payload.id != key):
                raise ValueError("Draft identity does not match its key")
        return self


class TokenRateStreamEvent(ContractModel):
    protocol: Literal[2]
    workspaceId: str
    rateLimits: JsonValue


class TranscriptStreamDelta(ContractModel):
    id: str
    append: str | None = None
    replace: JsonValue | None = None


class TranscriptStreamUpdate(ContractModel):
    version: int
    replaceAll: bool = Field(alias="replace")
    items: list[TranscriptStreamDelta]
    order: list[str] | None = None
    truncated: bool | None = None
    unavailable: str | None = None
    tail: str | None = None
    # Runtime.transcript has additional status fields documented by Codex app-server.
    model: str | None = None
    turnId: str | None = None
    threadId: str | None = None
    error: str | None = None


class EntityCollection(ContractStrEnum):
    AGENT = "agent"
    ROOM = "room"
    TASK = "task"
    MONITOR = "monitor"
    COMPLAINT = "complaint"
    REQUEST = "request"
    RULE = "rule"
    PROJECT = "project"
    PEER_TEAM = "peerTeam"
    CHAT = "chat"
    EDGE = "edge"
    EVENT = "event"
    WORK = "work"
    WORKSPACE = "workspace"


class SyncEntityPayload(ContractModel):
    collection: EntityCollection
    id: str
    value: JsonValue


class SyncDocument(ContractModel):
    id: str
    payload: str
    seq: int
    deleted: bool = Field(alias="_deleted")
