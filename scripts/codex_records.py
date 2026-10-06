"""Declared JSON record shapes stored by the orchestration runtime.

These types describe persisted values. Renderer DTOs and runtime projections
remain separate contracts; known disagreements are checked in
``test_codex_records.py``.
"""

from typing import Any, Literal, NotRequired, Protocol, TypedDict


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
    actionIdentity: NotRequired[str]
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
    dueAt: NotRequired[float]
    reason: NotRequired[str]


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


class NativeNameSyncedRecord(TypedDict):
    accountKey: NotRequired[str]
    name: NotRequired[str]
    threadId: NotRequired[str]


class NativeNameFailureRecord(TypedDict):
    identity: NotRequired[JsonObject]
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


class BrowserRecoveryRecord(TypedDict):
    stage: NotRequired[str]
    at: NotRequired[float]
    turnId: NotRequired[str]
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
    activeTools: NotRequired[list[JsonValue]]
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
    budgetStartWait: NotRequired[JsonObject]
    budgetActionWait: NotRequired[JsonObject]
    lastBudgetWait: NotRequired[JsonObject]
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
    turnRecovery: NotRequired[JsonObject]
    connectionCheck: NotRequired[ConnectionCheckRecord]
    readState: NotRequired[ReadStateRecord]
    parkedEvent: NotRequired[str]
    parkAfterTurn: NotRequired[JsonObject]
    parkReceipt: NotRequired[JsonObject]
    parkSequence: NotRequired[int]
    cancelledPark: NotRequired[JsonObject]
    liveSteerAttempt: NotRequired[JsonObject]
    liveSteerRejectedTurnId: NotRequired[str]
    steerRejectedTurnId: NotRequired[str]
    queueNotice: NotRequired[JsonObject]
    workspaceOperation: NotRequired[str | None]
    checkpointHistoryHead: NotRequired[str]
    restoredCheckpoint: NotRequired[str]
    worktreeCleanup: NotRequired[WorktreeCleanupRecord]
    cleanedWorktree: NotRequired[JsonObject]
    lastWorkspaceWait: NotRequired[JsonObject]
    workerBaseRef: NotRequired[str | None]
    workerBaseCommit: NotRequired[str | None]
    workerBaseBehindMain: NotRequired[int | None]
    workerBaseMainRef: NotRequired[str | None]
    accountHistory: NotRequired[list[JsonValue]]
    deliveredMode: NotRequired[JsonObject]
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


class RoomRecord(TypedDict):
    id: str
    kind: Literal["broadcast", "private", "shared", "federated"]
    members: NotRequired[list[str]]
    rootId: NotRequired[str]
    updated: float
    userHidden: bool


class RuntimeHost(Protocol):
    """Runtime surface consumed by mixins; intentionally excludes implementation state."""

    WORKSPACE_OPERATION_ACTIVE: Any
    _accepted_archive_index_ready: Any
    _accepted_archive_scan_after: Any
    _accepted_archive_tick_at: Any
    _analytics_history_cursor: Any
    _analytics_history_guard: Any
    _analytics_history_ids: Any
    _analytics_history_paths: Any
    _analytics_history_record: Any
    _analytics_history_save: Any
    _analytics_history_schema_ready: Any
    _analytics_rollout_path: Any
    _archive_accepted_owner: Any
    _archive_child_result_event: Any
    _archive_work_result: Any
    _check_search_rows_batch: Any
    _delete_search_next: Any
    _finish_accepted_action: Any
    _image_file_size: Any
    _index_search_next: Any
    _list_tool_requests: Any
    _read_image_file: Any
    _release_work_after: Any
    _request_agent_states: Any
    _run_accepted_archive: Any
    _search_cleanup_batch: Any
    _search_excerpt: Any
    _search_migration_batch: Any
    _search_migration_run: Any
    _search_migration_verify_and_switch: Any
    _search_phase: Any
    _stage_event_resources: Any
    _token_rate_observation_batch: Any
    _token_rate_observation_limit: Any
    _turn_recovery_busy: Any
    _turn_recovery_checked: Any
    _turn_recovery_results: Any
    _work_action: Any
    _work_continuation_pending: Any
    _workspace_complaints: Any
    _workspace_idle_snapshot: Any
    _workspace_operation: Any
    _workspace_operation_id: Any
    _workspace_operations: Any
    _workspace_provider_rejected: Any
    _workspace_provider_result: Any
    _workspace_records: Any
    _workspace_requests: Any
    _workspace_restore_signature: Any
    _workspace_rules: Any
    _workspace_source: Any
    _workspace_work: Any
    accounts: Any
    analytics: Any
    analytics_agent: Any
    analytics_budget_capture: Any
    analytics_connection: Any
    analytics_detail_page: Any
    analytics_event: Any
    analytics_history_db: Any
    analytics_history_health: Any
    analytics_history_init: Any
    analytics_history_start: Any
    analytics_history_step: Any
    analytics_history_thread: Any
    analytics_limit_changed: Any
    analytics_model_payload: Any
    analytics_read_connection: Any
    analytics_safe: Any
    analytics_store_item: Any
    analytics_turn_errors: Any
    answer: Any
    apply_orphan_recovery: Any
    apply_turn_recovery: Any
    assert_workspace_available: Any
    asset_record: Any
    asset_view: Any
    baseline_missing: Any
    branch_locked: Any
    cache_input_tokens: Any
    cache_pairs: Any
    cancel_monitor: Any
    capability_cache: Any
    capacity_check: Any
    capacity_retry: Any
    capacity_run: Any
    capacity_save: Any
    capacity_started: Any
    capacity_wait: Any
    capture_checkpoint: Any
    changed: Any
    chat_rooms: Any
    checked_actor: Any
    checked_actor_in_own_db: Any
    checkpoint_after_turn: Any
    checkpoint_capture: Any
    checkpoint_summary: Any
    child_stopped_event: Any
    closed: Any
    complaint_message: Any
    complaint_needs_response: Any
    complaint_recipient: Any
    connect: Any
    connection_current: Any
    connection_ids: Any
    continuation_work_claims: Any
    continuation_work_claims_valid: Any
    count: Any
    create: Any
    db: Any
    db_path: Any
    defer_preparation: Any
    delivery_executor: Any
    deltas: Any
    dispatch_active_slots: Any
    duration_values: Any
    durations: Any
    enqueue: Any
    enqueue_recovery_event: Any
    exact: Any
    failed: Any
    file_fingerprint: Any
    finish_tool_request: Any
    git: Any
    hold_unknown_start: Any
    index_item: Any
    input_measurements: Any
    item: Any
    legacy: Any
    limit: Any
    limits: Any
    loaded: Any
    lock: Any
    low_workers_tick: Any
    metrics: Any
    model_known_context: Any
    model_page: Any
    model_peers_directory: Any
    model_saved_message: Any
    monitor: Any
    monitor_auto_approved: Any
    named_agents: Any
    new_thread_params: Any
    notification: Any
    offline_accounts: Any
    operation_receipt: Any
    output_measurements: Any
    panel_guidance: Any
    parent_event: Any
    peak_context: Any
    peak_percent: Any
    percent: Any
    permanent_worker_hold: Any
    points: Any
    pool: Any
    preparation_settings: Any
    prepare: Any
    prepare_locks: Any
    profiles: Any
    progress_file: Any
    project_account: Any
    project_directory: Any
    project_room_ids: Any
    project_worker_base: Any
    projects: Any
    rate_limits_for: Any
    read_db: Any
    recent_monitors: Any
    recent_tasks: Any
    reconcile_orphan_busy: Any
    reconcile_start_receipt: Any
    recovery_pool: Any
    remember_role_text: Any
    reported_change_files: Any
    reported_changes: Any
    reported_plan: Any
    restore_absent_start: Any
    role_guidance: Any
    role_update: Any
    root: Any
    rule_finished: Any
    rule_owner_recovery_pending: Any
    rules_action: Any
    run_native_action: Any
    run_rule: Any
    run_turn_recovery: Any
    save_receipt: Any
    search_is_indexed: Any
    search_migration_error: Any
    search_migration_last_batch_bytes: Any
    search_migration_thread: Any
    send: Any
    servers: Any
    setup_search_rows: Any
    snapshot_tree: Any
    start_accepted: Any
    submit_reserved: Any
    sync_agent_rooms: Any
    task_brief: Any
    task_detail: Any
    team_agents: Any
    thread_config: Any
    tool_definitions: Any
    tool_request: Any
    tool_request_actor: Any
    tool_request_key: Any
    tool_result: Any
    unanswered_complaints: Any
    usage_resume_auth_marker: Any
    usage_resume_cancel: Any
    usage_resume_record: Any
    usage_resume_save: Any
    values: Any
    width: Any
    work_action: Any
    work_by_id: Any
    work_dependency_statuses: Any
    work_dependent_records: Any
    work_records: Any
    work_view: Any
    worker_defaults: Any
    workspace_blockers: Any
    workspace_path: Any

    def agent(self, agent_id: str, db: Any = ...) -> AgentRecord: ...

    def records(self, db: Any, table: str = "agents", *, shared: bool = False) -> Any: ...

    def put(self, db: Any, table: str, record: Any, **kwargs: Any) -> None: ...
