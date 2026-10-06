"""Declared JSON record shapes stored by the orchestration runtime.

These types describe persisted values. Renderer DTOs and runtime projections
remain separate contracts; known disagreements are checked in
``test_codex_records.py``.
"""

from typing import Literal, NotRequired, Protocol, TypedDict, overload
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import sqlite3
    from collections.abc import Iterable


JsonValue = str | int | float | bool | None | list["JsonValue"] | dict[str, "JsonValue"]
JsonObject = dict[str, JsonValue]

AgentStatusValue = Literal[
    "unknown", "idle", "queued", "starting", "running", "waiting", "approval",
    "completed", "interrupted", "failed", "blocked", "paused", "parked",
    "abandoned", "capacity-retry",
]
AgentProviderValue = Literal["codex", "claude"]
AgentRoleValue = Literal["lead", "worker", "orchestrator", "agent", "implementer", "reviewer"]
AgentModeValue = Literal["multi", "single"]
ImageWorkspacePhase = Literal["read_only", "ready", "fallback", "archived"]
WorktreePreparation = Literal["waiting", "preparing"]
WorkStatusValue = Literal["ready", "running", "blocked", "review", "accepted", "cancelled"]
ComplaintStatusValue = Literal["open", "in_progress", "resolved", "declined"]
WorkspaceOperationPhase = Literal[
    "capture_pending", "capture_running", "reserved", "running", "provider_pending",
    "provider_ready", "local_mutation", "recovery_required", "completed", "failed",
]
NativeReleasePhaseValue = Literal["checking", "unsubscribing", "released", "unknown", "blocked", "resumed"]
UsageResumeStatusValue = Literal["scheduled", "started", "cancelled", "unknown"]
UsageResumeCauseValue = Literal["usage_limit", "rate_limit", "auth"]
AccountTransferStatusValue = Literal["pending", "completed", "cancelled"]
AccountTransferScopeValue = Literal["team", "subagents"]
NativeStatusPhaseValue = Literal["safety", "retrying", "auth"]
NativeStatusValue = Literal["idle", "notLoaded", "unsubscribed", "notSubscribed"]


class SupervisorIdentityRecord(TypedDict):
    handle: NotRequired[str]
    generation: NotRequired[int]
    stateDir: NotRequired[str]


class StartModelSettingsRecord(TypedDict):
    id: NotRequired[str]
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    connectionId: NotRequired[str]
    threadId: NotRequired[str]
    settings: NotRequired[JsonObject]
    status: NotRequired[str]


class StartActionIdentityRecord(TypedDict):
    accountKey: str
    threadId: str
    epoch: int


class StartAttemptRecord(TypedDict):
    id: NotRequired[str]
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    events: NotRequired[list[JsonValue]]
    submitted: NotRequired[bool]
    created: NotRequired[float]
    action: NotRequired[str]
    activeAtReservation: NotRequired[bool]
    connectionId: NotRequired[str]
    threadId: NotRequired[str]
    turnId: NotRequired[str]
    observedTurnId: NotRequired[str]
    nativeOperationId: NotRequired[str]
    settingsFixed: NotRequired[bool]
    supervisorIdentity: NotRequired[SupervisorIdentityRecord | None]
    modelSettings: NotRequired[StartModelSettingsRecord]
    actionIdentity: NotRequired["StartActionIdentityRecord"]
    actionRequestId: NotRequired[str]
    capacityRetryId: NotRequired[str]
    claudeInputRequest: NotRequired[JsonObject]
    claudeRetryOf: NotRequired[str]
    executionOutcome: NotRequired[Literal["unknown", "unsent", "rejected"]]
    notSubmittedReason: NotRequired[str]
    prepareError: NotRequired[JsonValue]
    responseError: NotRequired[JsonValue]
    retiredEvents: NotRequired[list[JsonValue]]
    reviewTarget: NotRequired[JsonObject]


class RestartRecoveryRecord(TypedDict):
    stage: NotRequired[str]
    at: NotRequired[float]
    autoWake: NotRequired[bool]
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    threadId: NotRequired[str]
    turnId: NotRequired[str]
    eventId: NotRequired[str]
    reconciledAt: NotRequired[float]
    observedAt: NotRequired[float]
    outcome: NotRequired[str]
    reason: NotRequired[str]
    reattachedAt: NotRequired[float]
    supersededAt: NotRequired[float]
    startAttempt: NotRequired[StartAttemptRecord]


class DisconnectRecoveryRecord(TypedDict):
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    threadId: NotRequired[str]
    turnId: NotRequired[str]
    startAttempt: NotRequired[StartAttemptRecord]
    connectionId: NotRequired[str]
    autoWake: NotRequired[bool]
    supervisor: NotRequired[JsonObject]
    at: NotRequired[float]
    source: NotRequired[str]


class ConnectionRecoveryRecord(TypedDict):
    source: NotRequired[str]
    at: NotRequired[float]
    turnId: NotRequired[str]
    outcome: NotRequired[str]
    previousError: NotRequired[JsonValue]
    automatic: NotRequired[bool]
    supervisor: NotRequired[JsonObject]
    queuedInputPreserved: NotRequired[bool]
    attemptId: NotRequired[str]
    eventIds: NotRequired[list[str]]
    completionDelivered: NotRequired[bool]
    holdOperations: NotRequired[list[str]]


class ContextRepairSourceRecord(TypedDict):
    id: NotRequired[str]
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    threadId: NotRequired[str]
    turnId: NotRequired[str]
    attemptId: NotRequired[str]


class ContextRepairSnapshotRecord(TypedDict):
    sourcePath: NotRequired[str]
    sourceSha256: NotRequired[str]
    sourceBytes: NotRequired[int]
    importThreadId: NotRequired[str]
    savedBytes: NotRequired[int]
    eventIds: NotRequired[list[str]]
    terminalTurnId: NotRequired[str | None]
    terminalCompletedAt: NotRequired[float | None]
    ancestry: NotRequired[list[JsonValue]]
    copyPath: NotRequired[str]
    copySha256: NotRequired[str]
    copyBytes: NotRequired[int]
    sourceFileIdentity: NotRequired[list[JsonValue]]


class HistoricalInputProofRecord(TypedDict):
    events: NotRequired[list[JsonValue]]
    terminalTurnId: NotRequired[str]
    terminalCompletedAt: NotRequired[float]
    source: NotRequired[ContextRepairSourceRecord]
    sourceSha256: NotRequired[str]
    deliveryOutcome: NotRequired[str]


class ContextRepairSourceCleanupRecord(TypedDict):
    phase: NotRequired[str]
    accountKey: NotRequired[str]
    threadId: NotRequired[str]
    connectionId: NotRequired[str]
    submittedAt: NotRequired[float]
    updatedAt: NotRequired[float]


class ContextRepairRecord(TypedDict):
    id: NotRequired[str]
    agent: NotRequired[str]
    phase: NotRequired[str]
    created: NotRequired[float]
    updated: NotRequired[float]
    compactions: NotRequired[int]
    connectionId: NotRequired[str]
    settings: NotRequired[JsonObject]
    snapshot: NotRequired[ContextRepairSnapshotRecord]
    source: NotRequired[ContextRepairSourceRecord]
    checkedEventIds: NotRequired[list[str]]
    previousRepairedEventIds: NotRequired[list[str]]
    repairedEventIds: NotRequired[list[str]]
    newThreadId: NotRequired[str]
    result: NotRequired[JsonValue]
    error: NotRequired[JsonValue]
    rpcId: NotRequired[str | int]
    rpcMethod: NotRequired[str]
    copyCleanup: NotRequired[JsonObject]
    historicalInputProof: NotRequired[HistoricalInputProofRecord]
    sourceCleanup: NotRequired[ContextRepairSourceCleanupRecord]
    historicalInputs: NotRequired[list[JsonValue]]


class ContextRepairWaitRecord(TypedDict):
    error: NotRequired[JsonValue]
    scope: NotRequired[str]
    source: NotRequired[ContextRepairSourceRecord]
    events: NotRequired[list[JsonValue]]
    nextCheckAt: NotRequired[float]
    at: NotRequired[float]
    firstAt: NotRequired[float]
    checks: NotRequired[int]
    action: NotRequired[str]
    actionAt: NotRequired[float]
    actionId: NotRequired[str]
    historyCheckAt: NotRequired[float]
    historyCheckId: NotRequired[str]
    historyCheckOwner: NotRequired[str]
    lastHistoryCheck: NotRequired[JsonObject]
    taskCheckAt: NotRequired[float]
    taskCheckId: NotRequired[str]
    taskCheckOwner: NotRequired[str]
    lastTaskCheckAt: NotRequired[float]
    lastTaskCheckError: NotRequired[JsonValue]


class NativeReleaseRecord(TypedDict):
    id: NotRequired[str]
    phase: NotRequired[NativeReleasePhaseValue]
    accountKey: NotRequired[str]
    threadId: NotRequired[str]
    connectionId: NotRequired[str]
    at: NotRequired[float]
    submittedAt: NotRequired[float]
    releasedAt: NotRequired[float]
    closedAt: NotRequired[float]
    resumedAt: NotRequired[float]
    error: NotRequired[JsonValue]
    nativeStatus: NotRequired[str]
    resetPending: NotRequired[bool]
    resetActorEpoch: NotRequired[int]
    nextAttemptAt: NotRequired[float]
    inspectionPhase: NotRequired[str]
    inspectionError: NotRequired[JsonValue]
    targetEpoch: NotRequired[int]
    targetParentId: NotRequired[str]
    targetRootId: NotRequired[str]


class PendingSettingsRecord(TypedDict):
    model: NotRequired[str | None]
    effort: NotRequired[str | None]
    fastMode: NotRequired[bool | None]
    daybreakEnabled: NotRequired[bool | None]


class WorkerDefaultsRecord(TypedDict):
    model: str | None
    effort: str | None
    fastMode: bool
    daybreakEnabled: NotRequired[bool]
    cyberAccessProgram: NotRequired[str | None]
    accountKey: NotRequired[str | None]


class AgentArchiveRecord(TypedDict):
    at: NotRequired[float]
    by: NotRequired[str]
    reason: NotRequired[str]
    epoch: NotRequired[int]
    cleanupPending: NotRequired[bool]
    unknownToolRequests: NotRequired[list[JsonValue]]
    unassignedWork: NotRequired[list[JsonValue]]


class LazyAccountTransferSourceRecord(TypedDict):
    provider: NotRequired[AgentProviderValue]
    model: NotRequired[str | None]
    effort: NotRequired[str | None]
    nativeEffort: NotRequired[str | None]
    fastMode: NotRequired[bool | None]
    daybreakEnabled: NotRequired[bool | None]
    cyberAccessProgram: NotRequired[JsonValue]
    claudeOptions: NotRequired[JsonObject | None]
    status: NotRequired[str]
    error: NotRequired[JsonValue]
    nativeFailureHold: NotRequired[JsonValue]
    pendingSettings: NotRequired[PendingSettingsRecord | None]
    pendingSettingsAccountKey: NotRequired[str | None]
    executionSettingsAccountKey: NotRequired[str]
    workerDefaults: NotRequired[WorkerDefaultsRecord]


class LazyAccountTransferRecord(TypedDict):
    id: NotRequired[str]
    sourceAccountKey: NotRequired[str]
    sourceThreadId: NotRequired[str]
    sourceState: NotRequired[LazyAccountTransferSourceRecord]


class CapacityRetryRecord(TypedDict):
    id: NotRequired[str]
    status: NotRequired[str]
    dueAt: NotRequired[float | None]
    reason: NotRequired[str | None]
    acceptedTurnId: NotRequired[str]
    claimedAt: NotRequired[float]
    taskClaims: NotRequired[list[JsonValue]]
    attempt: NotRequired[int]
    cause: NotRequired[str]
    cwd: NotRequired[str]
    epoch: NotRequired[int]
    maxAttempts: NotRequired[int]
    settings: NotRequired[JsonObject]
    threadId: NotRequired[str]
    turnId: NotRequired[str]
    updatedAt: NotRequired[float]
    accountKey: NotRequired[str]


class UsageResumeRecord(TypedDict):
    id: NotRequired[str]
    status: NotRequired[UsageResumeStatusValue]
    cause: NotRequired[UsageResumeCauseValue]
    accountKey: NotRequired[str]
    threadId: NotRequired[str]
    epoch: NotRequired[int]
    turnId: NotRequired[str]
    failedAt: NotRequired[float]
    authAttempt: NotRequired[int]
    authRefreshMarker: NotRequired[str | None]
    dueAt: NotRequired[float | None]
    plannedAt: NotRequired[float | None]
    resetAt: NotRequired[float | None]
    proofAt: NotRequired[float]
    startedAt: NotRequired[float]
    lastCheckedAt: NotRequired[float | None]
    waitingForAuth: NotRequired[bool]
    reason: NotRequired[str | None]
    updatedAt: NotRequired[float]
    taskClaims: NotRequired[list[str]]


class AgentActivityRecord(TypedDict):
    phase: NotRequired[str]
    at: NotRequired[float]
    tools: NotRequired[list["ActiveToolRecord"]]


class ActiveToolRecord(TypedDict):
    id: str
    type: str
    name: str


class AgentNativeStatusRecord(TypedDict):
    phase: NotRequired[NativeStatusPhaseValue]
    turnId: NotRequired[str]
    at: NotRequired[float]
    message: NotRequired[str]
    error: NotRequired[str | JsonObject | None]


class NativeNameIdentityRecord(TypedDict):
    accountKey: str
    threadId: str | None
    name: str


class NativeNameSyncedRecord(TypedDict):
    accountKey: NotRequired[str]
    name: NotRequired[str]
    threadId: NotRequired[str]


class NativeNameFailureRecord(TypedDict):
    identity: NotRequired[NativeNameIdentityRecord]
    error: NotRequired[JsonValue]
    attempts: NotRequired[int]
    retryAt: NotRequired[float]


class ContextUsageRecord(TypedDict):
    tokens: int
    window: int
    at: NotRequired[float]


class ReviewDefaultsRecord(TypedDict):
    model: str | None
    effort: str | None


class ConvertedFromLeadRecord(TypedDict):
    requestId: str
    by: Literal["user"]
    at: float
    oldRootId: str
    rootId: str


class AccountTransferInterruptedRecord(TypedDict):
    id: str
    name: NotRequired[str | None]
    reason: NotRequired[str | None]


class AccountTransferLeftOnSourceRecord(TypedDict):
    id: str
    name: NotRequired[str | None]
    provider: NotRequired[AgentProviderValue | None]
    reason: NotRequired[str | None]


class AccountTransferBlockedRecord(TypedDict):
    id: str
    name: NotRequired[str | None]
    reason: NotRequired[str | None]


class AccountTransferSummaryRecord(TypedDict):
    id: NotRequired[str | None]
    targetAccountKey: NotRequired[str | None]
    status: NotRequired[AccountTransferStatusValue | None]
    updated: NotRequired[float | None]
    scope: NotRequired[AccountTransferScopeValue | None]
    finishHistory: NotRequired[bool | None]
    total: NotRequired[int | None]
    completed: NotRequired[int | None]
    moved: NotRequired[int | None]
    nativeHistoryPending: NotRequired[int | None]
    movingNow: NotRequired[int | None]
    canFinishHistory: NotRequired[bool | None]
    interrupted: NotRequired[list[AccountTransferInterruptedRecord] | None]
    leftOnSource: NotRequired[list[AccountTransferLeftOnSourceRecord] | None]
    blocked: NotRequired[list[AccountTransferBlockedRecord] | None]
    waitingCount: NotRequired[int | None]
    waiting: NotRequired[str | None]
    needsAttention: NotRequired[bool | None]
    canRetry: NotRequired[bool | None]


class NativeSafetyBufferingRecord(TypedDict):
    turnId: NotRequired[str]
    threadId: NotRequired[str]
    accountKey: NotRequired[str]
    connectionId: NotRequired[str]
    at: NotRequired[float]
    dismissed: NotRequired[bool]
    responseStarted: NotRequired[bool]
    showBufferingUi: NotRequired[bool]
    fasterModel: NotRequired[str]


class NativeSafetyRetryRecord(TypedDict):
    id: NotRequired[str]
    stage: NotRequired[Literal[
        "turns", "items", "interrupt", "verify_turns", "verify_items", "fork",
        "start", "unknown", "running", "failed", "cancelled",
    ]]
    model: NotRequired[str]
    turnId: NotRequired[str]
    created: NotRequired[float]
    updated: NotRequired[float]
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    error: NotRequired[str | None]
    newThreadId: NotRequired[str]
    acceptedTurnId: NotRequired[str]
    requestId: NotRequired[str]
    rpcMethod: NotRequired[str]


class NativeTurnErrorRecord(TypedDict):
    turnId: NotRequired[str]
    error: NotRequired[JsonObject | None]


class NativeThreadBlockRecord(TypedDict):
    threadId: NotRequired[str]
    error: NotRequired[str]


class ConnectionCheckRecord(TypedDict):
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    threadId: NotRequired[str]
    turnId: NotRequired[str]
    at: float
    previousError: NotRequired[str | None]
    nativeState: NotRequired[str]
    restartTurnStatus: NotRequired[str]
    readError: NotRequired[str | None]


class ReadStateRecord(TypedDict):
    threadId: str
    turnId: str
    read: bool
    revision: int


class NativeReviewRecord(TypedDict):
    id: NotRequired[str]
    status: NotRequired[str]
    created: NotRequired[float]
    updated: NotRequired[float]
    result: NotRequired[JsonValue]
    error: NotRequired[JsonValue]
    actorId: NotRequired[str]
    args: NotRequired[JsonObject]
    requestId: NotRequired[str]
    response: NotRequired[JsonValue]
    startedAt: NotRequired[float]
    target: NotRequired[JsonObject]


class BrowserRecoveryRequestRecord(TypedDict):
    id: NotRequired[str]
    method: NotRequired[str]
    params: NotRequired[JsonObject]
    submittedAt: NotRequired[float]
    receivedAt: NotRequired[float]


class BrowserRecoveryRecord(TypedDict):
    id: NotRequired[str]
    stage: NotRequired[str]
    at: NotRequired[float]
    itemId: NotRequired[str]
    turnId: NotRequired[str]
    threadId: NotRequired[str | None]
    epoch: NotRequired[int]
    accountKey: NotRequired[str]
    connectionId: NotRequired[str | None]
    reason: NotRequired[str]
    createdAt: NotRequired[float]
    verifiedAt: NotRequired[float]
    verificationItem: NotRequired[str]
    failedAt: NotRequired[float]
    startedAt: NotRequired[float]
    resumedAt: NotRequired[float]
    requestId: NotRequired[str]
    nativeRequest: NotRequired[BrowserRecoveryRequestRecord]
    outcome: NotRequired[str]
    error: NotRequired[JsonValue]


class EmptyTransferRecoveryRecord(TypedDict):
    id: NotRequired[str]
    phase: NotRequired[str]
    sourceAccountKey: NotRequired[str]
    destinationAccountKey: NotRequired[str]
    sourceThreadId: NotRequired[str]
    destinationThreadId: NotRequired[str]
    connectionId: NotRequired[str]
    created: NotRequired[float]
    updated: NotRequired[float]
    submittedAt: NotRequired[float]
    completedAt: NotRequired[float]
    error: NotRequired[JsonValue]
    sourceState: NotRequired[LazyAccountTransferSourceRecord]
    requestId: NotRequired[str]
    epoch: NotRequired[int]
    threadId: NotRequired[str]
    turnId: NotRequired[str]


class WorktreeCleanupRecord(TypedDict):
    path: NotRequired[str]
    phase: NotRequired[str]
    at: NotRequired[float]
    error: NotRequired[str]


class WorkspaceOperationAgentRecord(TypedDict):
    id: NotRequired[str]
    agent: NotRequired[str]
    kind: NotRequired[str]
    phase: NotRequired[str]
    created: NotRequired[float]
    updated: NotRequired[float]
    error: NotRequired[JsonValue]
    cwd: NotRequired[str]
    epoch: NotRequired[int]
    turnId: NotRequired[str]


class WorkspaceWaitRecord(TypedDict):
    attemptId: NotRequired[str]
    at: NotRequired[float]
    events: NotRequired[list[JsonValue]]
    blockers: NotRequired[list[JsonValue]]


class CleanedWorktreeRecord(TypedDict):
    branch: NotRequired[str]
    bytes: NotRequired[int | None]
    head: NotRequired[str]
    identity: NotRequired[list[JsonValue]]
    relative: NotRequired[str]
    repo: NotRequired[str]
    root: NotRequired[str]
    missing: NotRequired[bool]
    note: NotRequired[str]


class DeliveredModeRecord(TypedDict):
    epoch: NotRequired[list[str | int | None]]
    version: NotRequired[str]
    revision: NotRequired[int]


class ParkReceiptRecord(TypedDict):
    event: NotRequired[str]
    epoch: NotRequired[int]
    sequence: NotRequired[int]
    cancelledAtEpoch: NotRequired[int]


class TurnRecoveryRecord(TypedDict):
    at: NotRequired[float]
    turnId: NotRequired[str | None]
    latestTurnId: NotRequired[str | None]
    outcome: NotRequired[str]
    source: NotRequired[str]
    attemptId: NotRequired[str]


class BudgetWaitRecord(TypedDict):
    attemptId: NotRequired[str]
    agentId: NotRequired[str]
    accountKey: NotRequired[str]
    epoch: NotRequired[int]
    threadId: NotRequired[str]
    events: NotRequired[list[JsonValue]]
    action: NotRequired[bool | str | None]
    actionRequestId: NotRequired[str]
    actionIdentity: NotRequired[StartActionIdentityRecord]
    error: NotRequired[str]
    at: NotRequired[float]
    status: NotRequired[str]
    finishedAt: NotRequired[float]
    admission: NotRequired[JsonValue]


class CleanedImageWorkspaceRecord(TypedDict):
    repo: NotRequired[str]
    relative: NotRequired[str]
    branch: NotRequired[str]
    head: NotRequired[str]
    restoreHeads: NotRequired[dict[str, str]]
    bytes: NotRequired[int]
    collect: NotRequired[JsonObject]


class ImageWorkspaceCleanupResultRecord(TypedDict):
    state: NotRequired[str]
    bytes: NotRequired[int]
    reason: NotRequired[str]
    restoreHeads: NotRequired[dict[str, str]]
    collect: NotRequired[JsonObject]


class NativeToolSourceRecord(TypedDict):
    threadId: NotRequired[str]
    epoch: NotRequired[int]
    digest: NotRequired[str]


class NativeToolUpdateRecord(TypedDict):
    status: NotRequired[str]
    message: NotRequired[str]
    source: NotRequired[NativeToolSourceRecord]


class PortableHistorySourceRecord(TypedDict):
    agentId: NotRequired[str]
    accountKey: NotRequired[str]
    epoch: NotRequired[int]
    threadId: NotRequired[str]


class PortableHistoryRecord(TypedDict):
    version: NotRequired[int]
    path: NotRequired[str]
    sha256: NotRequired[str]
    bytes: NotRequired[int]
    counts: NotRequired[dict[str, int]]
    source: NotRequired[PortableHistorySourceRecord]
    transferId: NotRequired[str]


class AgentDraftRecord(TypedDict):
    text: NotRequired[str]
    prefixText: NotRequired[str]
    assets: NotRequired[list[JsonValue]]


class AgentRecord(TypedDict):
    """JSON object stored in ``runtime_agents.record`` after mode projection."""

    id: str
    rootId: str
    epoch: int
    status: AgentStatusValue
    name: NotRequired[str]
    prompt: NotRequired[str]
    cwd: NotRequired[str]
    role: NotRequired[AgentRoleValue]
    parentId: NotRequired[str | None]
    threadId: NotRequired[str | None]
    accountKey: NotRequired[str]
    executionSettingsAccountKey: NotRequired[str]
    provider: NotRequired[AgentProviderValue]
    model: NotRequired[str | None]
    effort: NotRequired[str | None]
    nativeEffort: NotRequired[str | None]
    fastMode: NotRequired[bool | None]
    daybreakEnabled: NotRequired[bool | None]
    yoloMode: NotRequired[bool]
    isLead: NotRequired[bool]
    autoWake: NotRequired[bool]
    inFlight: NotRequired[bool]
    turnId: NotRequired[str | None]
    turnEpoch: NotRequired[int]
    tokensUsed: NotRequired[int]
    compactions: NotRequired[int]
    compactionsObservedOnly: NotRequired[bool]
    events: NotRequired[int]
    created: NotRequired[float]
    updated: NotRequired[float]
    error: NotRequired[str | JsonObject | None]
    tail: NotRequired[str]
    worktree: NotRequired[bool]
    worktreeReady: NotRequired[bool]
    worktreePreparation: NotRequired[WorktreePreparation | None]
    worktreeWarning: NotRequired[str | None]
    imageWorkspace: NotRequired[bool]
    imageWorkspaceReady: NotRequired[bool]
    imageWorkspacePhase: NotRequired[ImageWorkspacePhase | None]
    imageWorkspaceError: NotRequired[str | None]
    imageWorkspaceRepo: NotRequired[str | None]
    imageWorkspaceBaseRepo: NotRequired[str | None]
    imageWorkspaceRelative: NotRequired[str | None]
    imageWorkspaceStartCommit: NotRequired[str | None]
    imageWorkspaceSnapshotCommit: NotRequired[str | None]
    imageWorkspaceMount: NotRequired[str | None]
    imageWorkspaceNoticeSent: NotRequired[str | None]
    imageWorkspaceNoticeText: NotRequired[str | None]
    imageWorkspaceNoticeError: NotRequired[str | None]
    imageWorkspaceCollect: NotRequired[JsonObject]
    concurrency: NotRequired[int]
    maxAgents: NotRequired[int]
    maxAgentsExplicit: NotRequired[bool]
    tokenBudget: NotRequired[int | None]
    usageResumeEnabled: NotRequired[bool]
    contextUsage: NotRequired[ContextUsageRecord | None]
    activeTools: NotRequired[list[ActiveToolRecord]]
    activity: NotRequired[AgentActivityRecord | None]
    activityPhase: NotRequired[str]
    nativeStatus: NotRequired[AgentNativeStatusRecord | NativeStatusValue | None]
    nativeSafetyBuffering: NotRequired[NativeSafetyBufferingRecord]
    nativeSafetyRetry: NotRequired[NativeSafetyRetryRecord]
    nativeTurnError: NotRequired[NativeTurnErrorRecord]
    nativeFailureHold: NotRequired[bool | JsonObject]
    nativeLimitErrorAt: NotRequired[float]
    nativeNameSynced: NotRequired[NativeNameSyncedRecord | None]
    nativeNameFailure: NotRequired[NativeNameFailureRecord]
    nativeRelease: NotRequired[NativeReleaseRecord | None]
    startAttempt: NotRequired[StartAttemptRecord]
    startOutcomeHold: NotRequired[JsonObject]
    restartRecovery: NotRequired[RestartRecoveryRecord]
    disconnectRecovery: NotRequired[DisconnectRecoveryRecord]
    connectionRecovery: NotRequired[ConnectionRecoveryRecord]
    contextRepair: NotRequired[ContextRepairRecord]
    contextRepairWait: NotRequired[ContextRepairWaitRecord]
    lastContextRepairWait: NotRequired[JsonObject]
    lastContextRepairCheck: NotRequired[JsonObject]
    contextRepairHistory: NotRequired[list[JsonValue]]
    browserRecovery: NotRequired[BrowserRecoveryRecord]
    nativeReview: NotRequired[NativeReviewRecord]
    supervisorRestore: NotRequired[JsonObject]
    pendingSettings: NotRequired[PendingSettingsRecord]
    pendingSettingsAccountKey: NotRequired[str | None]
    workerDefaults: NotRequired[WorkerDefaultsRecord]
    reviewDefaults: NotRequired[ReviewDefaultsRecord]
    agentArchive: NotRequired[AgentArchiveRecord]
    accountTransfer: NotRequired[AccountTransferSummaryRecord | None]
    accountTransferId: NotRequired[str]
    accountTransferState: NotRequired[JsonValue]
    lazyAccountTransfer: NotRequired[LazyAccountTransferRecord]
    emptyTransferRecovery: NotRequired[EmptyTransferRecoveryRecord]
    capacityRetry: NotRequired[CapacityRetryRecord]
    capacityRetryCount: NotRequired[int]
    usageResume: NotRequired[UsageResumeRecord]
    budgetBlocked: NotRequired[JsonObject]
    budgetStartWait: NotRequired[BudgetWaitRecord]
    budgetActionWait: NotRequired[BudgetWaitRecord]
    lastBudgetWait: NotRequired[BudgetWaitRecord]
    claudeInputRequest: NotRequired[JsonObject]
    claudePreInputRetry: NotRequired[JsonObject]
    claudeOptions: NotRequired[JsonObject]
    nativeResponseTurn: NotRequired[str]
    nativeToolCatalog: NotRequired[JsonObject]
    quickCreate: NotRequired[bool]
    quickCreateRequest: NotRequired[str | None]
    needsTitle: NotRequired[bool]
    projectFolder: NotRequired[str]
    projectFolderRevision: NotRequired[int]
    project: NotRequired[str]
    profileId: NotRequired[str | None]
    profileInstructions: NotRequired[str]
    sandbox: NotRequired[JsonObject | None]
    approvalPolicy: NotRequired[str]
    cyberAccessProgram: NotRequired[JsonValue]
    lastAnswer: NotRequired[str]
    lastCompletedTurn: NotRequired[str]
    lastCompletedTurnStatus: NotRequired[AgentStatusValue | None]
    lastCompletedTurnError: NotRequired[JsonValue]
    lastUpdated: NotRequired[float]
    turnRecovery: NotRequired[TurnRecoveryRecord]
    connectionCheck: NotRequired[ConnectionCheckRecord]
    readState: NotRequired[ReadStateRecord]
    parkedEvent: NotRequired[str]
    parkAfterTurn: NotRequired[bool]
    parkReceipt: NotRequired[ParkReceiptRecord]
    parkSequence: NotRequired[int]
    cancelledPark: NotRequired[ParkReceiptRecord]
    liveSteerAttempt: NotRequired[JsonObject]
    liveSteerRejectedTurnId: NotRequired[str]
    steerRejectedTurnId: NotRequired[str]
    queueNotice: NotRequired[JsonObject]
    workspaceOperation: NotRequired[str | None]
    workspaceReservationId: NotRequired[str]
    checkpointHistoryHead: NotRequired[str]
    restoredCheckpoint: NotRequired[str]
    worktreeCleanup: NotRequired[WorktreeCleanupRecord]
    cleanedWorktree: NotRequired[CleanedWorktreeRecord]
    lastWorkspaceWait: NotRequired[WorkspaceWaitRecord]
    workerBaseRef: NotRequired[str | None]
    workerBaseCommit: NotRequired[str | None]
    workerBaseBehindMain: NotRequired[int | None]
    workerBaseMainRef: NotRequired[str | None]
    accountHistory: NotRequired[list[JsonValue]]
    deliveredMode: NotRequired[DeliveredModeRecord]
    agentMode: NotRequired[AgentModeValue]
    agentModeRevision: NotRequired[int]
    agentModeSupported: NotRequired[bool]
    agentModeChangedAt: NotRequired[float]
    agentModeChangedBy: NotRequired[str]
    subagentConcurrencyVersion: NotRequired[int]
    accountId: NotRequired[str]
    deletedAt: NotRequired[float]
    archived: NotRequired[bool]
    pinned: NotRequired[bool]
    sharedRoomId: NotRequired[str]
    manualName: NotRequired[bool]
    convertedFromLead: NotRequired[ConvertedFromLeadRecord]
    importedFrom: NotRequired[str]
    needsAttention: NotRequired[bool]
    tokenUsageAccounting: NotRequired[str]
    nativeName: NotRequired[str]
    nativeNameSyncError: NotRequired[str]
    workerBaseStatus: NotRequired[str]
    deliveredModeVersion: NotRequired[int]
    nativeOperation: NotRequired[JsonObject]
    branch: NotRequired[str]
    checkpointError: NotRequired[str | None]
    complaintMisses: NotRequired[int]
    complaintsPresented: NotRequired[list[str]]
    lastEvent: NotRequired[str]
    prepareAttempt: NotRequired[str]
    preparedContext: NotRequired[JsonObject]
    profile: NotRequired[JsonObject | None]
    reviewArchiveAttempts: NotRequired[int]
    reviewArchiveError: NotRequired[str]
    reviewArchiveNextAt: NotRequired[float]
    reviewArchiveScheduled: NotRequired[str]
    authResumeAttempt: NotRequired[int]
    cleanedImageWorkspace: NotRequired[CleanedImageWorkspaceRecord]
    imageWorkspaceCleanupResult: NotRequired[ImageWorkspaceCleanupResultRecord]
    imageWorkspaceBaseError: NotRequired[str]
    nativeToolRefreshId: NotRequired[str]
    nativeToolUpdate: NotRequired[NativeToolUpdateRecord]
    portableHistory: NotRequired[PortableHistoryRecord]
    queueMutationRevision: NotRequired[int]
    forkedFrom: NotRequired[str]
    sourceMessage: NotRequired[str]
    draft: NotRequired[AgentDraftRecord]


class WorkArchiveRecord(TypedDict):
    reason: NotRequired[str]
    status: NotRequired[str]
    worktree: NotRequired[JsonObject]


class WorkRecord(TypedDict):
    id: str
    rootId: str
    status: WorkStatusValue
    title: str
    description: str
    owner: NotRequired[str | None]
    created: float
    updated: float
    createdBy: str
    version: int
    dependencies: list[str]
    decisions: list[JsonValue]
    results: list[JsonValue]
    archive: NotRequired[WorkArchiveRecord]
    archiveIntent: NotRequired[JsonObject]
    releases: NotRequired[list[JsonValue]]


class CheckpointRecord(TypedDict):
    id: str
    agent: str
    created: float
    commit: NotRequired[str]
    path: NotRequired[str]
    message: NotRequired[str]
    source: NotRequired[str]
    worktree: NotRequired[str]
    rootId: NotRequired[str]
    revision: NotRequired[int]
    error: NotRequired[str]
    data: NotRequired[JsonObject]
    automatic: NotRequired[bool]
    cwd: NotRequired[str]
    historyBoundary: NotRequired[int | None]
    historyDelta: NotRequired[list[JsonValue]]
    historyParent: NotRequired[str | None]
    label: NotRequired[str]
    ref: NotRequired[str]
    threadId: NotRequired[str | None]
    tree: NotRequired[str]
    turnId: NotRequired[str | None]


class ComplaintRecord(TypedDict):
    id: str
    leadId: str
    author: str
    text: str
    status: ComplaintStatusValue
    created: float
    updated: NotRequired[float]
    readAt: NotRequired[float | None]
    response: NotRequired[str]
    respondedAt: NotRequired[float]
    respondedBy: NotRequired[str]
    sourceType: NotRequired[str]
    recipient: NotRequired[str]
    responses: NotRequired[list[JsonValue]]
    version: NotRequired[int]


class ProjectRecord(TypedDict):
    id: str
    path: str
    name: str
    accountKey: str
    accountRevision: int
    created: float
    updated: NotRequired[float]
    accountKeys: NotRequired[list[str]]
    folders: NotRequired[list[JsonValue]]
    organizationRevision: NotRequired[int]
    peerTeams: NotRequired[list[JsonValue]]
    peerTeamsRevision: NotRequired[int]
    workerBaseRef: NotRequired[str]
    workerBaseRevision: NotRequired[int]


class AccountTransferRecord(TypedDict):
    id: str
    leadId: str
    created: float
    status: AccountTransferStatusValue
    scope: AccountTransferScopeValue
    targetAccountKey: str
    members: dict[str, JsonObject]
    requests: dict[str, JsonObject]
    updated: NotRequired[float]
    targetProvider: NotRequired[AgentProviderValue]


class WorkspaceOperationRecord(TypedDict):
    id: str
    agent: str
    kind: str
    phase: WorkspaceOperationPhase
    created: float
    updated: float
    cwd: str
    epoch: int
    turnId: str
    error: str | None
    source: NotRequired[JsonObject]


class RoomLastMessageRecord(TypedDict):
    seq: int
    text: str
    created: float
    sender: str


class RoomRecord(TypedDict):
    id: str
    kind: Literal["broadcast", "private", "shared", "federated"]
    members: NotRequired[list[str]]
    rootId: NotRequired[str]
    updated: float
    userHidden: bool
    customName: NotRequired[str]
    projectPath: NotRequired[str]
    created: NotRequired[float]
    name: NotRequired[str]
    lastMessage: NotRequired[RoomLastMessageRecord | None]


# Parallel slice rule: helpers that take Runtime annotate it as the quoted name
# "Runtime" imported under TYPE_CHECKING. A mixin declares a local Protocol
# extending RecordStore with only the accurately typed host attributes it uses.
# Do not add a shared catch-all runtime protocol here.
class RecordStore(Protocol):
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["agents"], *, shared: Literal[True]) -> tuple[AgentRecord, ...]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["agents"], *, shared: Literal[False] = False) -> list[AgentRecord]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["agents"], *, shared: bool) -> list[AgentRecord] | tuple[AgentRecord, ...]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["work"], *, shared: bool = False) -> list[WorkRecord]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["checkpoints"], *, shared: bool = False) -> list[CheckpointRecord]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["complaints"], *, shared: bool = False) -> list[ComplaintRecord]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["projects"], *, shared: bool = False) -> list[ProjectRecord]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["account_transfers"], *, shared: bool = False) -> list[AccountTransferRecord]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["workspace_operations"], *, shared: bool = False) -> list[WorkspaceOperationRecord]: ...
    @overload
    def records(self, db: "sqlite3.Connection", table: Literal["rooms"], *, shared: bool = False) -> list[RoomRecord]: ...

    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["agents"], record: AgentRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...
    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["work"], record: WorkRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...
    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["checkpoints"], record: CheckpointRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...
    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["complaints"], record: ComplaintRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...
    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["projects"], record: ProjectRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...
    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["account_transfers"], record: AccountTransferRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...
    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["workspace_operations"], record: WorkspaceOperationRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...
    @overload
    def put(self, db: "sqlite3.Connection", table: Literal["rooms"], record: RoomRecord, *, sync_rooms: bool = True, include_last_message: bool = False) -> None: ...

    def agent(self, key: str, db: "sqlite3.Connection | None" = None) -> AgentRecord: ...
    def team_agents(self, db: "sqlite3.Connection", root_id: str, *, include_deleted: bool = False, include_id: str | None = None) -> list[AgentRecord]: ...
    def named_agents(self, db: "sqlite3.Connection", agent_ids: "Iterable[object]") -> dict[str, AgentRecord]: ...
    @staticmethod
    def account_agents(db: "sqlite3.Connection", account_key: str) -> list[AgentRecord]: ...
    @staticmethod
    def thread_agents(db: "sqlite3.Connection", account_key: str, thread_id: str) -> list[AgentRecord]: ...
    @staticmethod
    def pending_restart_agents(db: "sqlite3.Connection") -> list[AgentRecord]: ...
    @staticmethod
    def descendant_agents(db: "sqlite3.Connection", root_id: str) -> list[AgentRecord]: ...
    @staticmethod
    def release_work_agents(db: "sqlite3.Connection") -> list[AgentRecord]: ...
    def scheduler_agents(self, db: "sqlite3.Connection") -> list[AgentRecord]: ...
    def agent_entity_view(self, db: "sqlite3.Connection", record: AgentRecord) -> dict[str, object]: ...
    def chat_rooms(self, db: "sqlite3.Connection", viewer: str | None = None, room_id: str | None = None, *, include_last_message: bool | None = None, include_peer_teams: bool = False) -> list[RoomRecord]: ...
