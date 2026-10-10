"""Wire models for desktop status, diagnostics, and directory browsing."""
from __future__ import annotations

from typing import Annotated, Literal

from pydantic import Field

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel
from studio_api.models import SupervisorIdentity as SupervisorIdentity
from studio_api.sync.models import AgentStatus, ReviewTargetDto, SnapshotModelSettingsDto


class NativeRuntimeState(ContractStrEnum):
    CHECKING = "checking"
    READY = "ready"
    FAILED = "failed"


class DesktopQuery(ContractModel):
    account_key: str = "default"


class DirectoriesQuery(ContractModel):
    path: str | None = None
    server: str | None = None
    request_id: str | None = Field(default=None, min_length=1, max_length=128)


class NativeProvider(ContractStrEnum):
    CODEX = "codex"
    CLAUDE = "claude"


class NativeAccountState(ContractStrEnum):
    WAITING = "waiting"
    UPDATING = "updating"
    CURRENT = "current"
    FAILED = "failed"


class NativeCandidateState(ContractStrEnum):
    DISCOVERED = "discovered"
    REJECTED = "rejected"
    APPROVED = "approved"


class ProcessKind(ContractStrEnum):
    CLAUDE_BRIDGE = "claude_bridge"
    CODE_MODE_HOST = "code_mode_host"
    CODEX_APP_SERVER = "codex_app_server"
    CLAUDE_CLI = "claude_cli"
    NODE_REPL = "node_repl"
    ANARLOG_MCP = "anarlog_mcp"
    STUDIO_BACKEND = "studio_backend"
    HEADLESS_BROWSER = "headless_browser"
    PYTHON = "python"
    NODE = "node"
    SHELL = "shell"
    OTHER = "other"


class AttributionComponent(ContractStrEnum):
    PROCESS = "process"
    RUNTIME = "runtime"
    CODEX = "codex"
    CLAUDE = "claude"
    CLAUDE_BRIDGE = "claude_bridge"
    CODE_MODE_HOST = "code_mode_host"
    CODEX_APP_SERVER = "codex_app_server"
    CLAUDE_CLI = "claude_cli"
    NODE_REPL = "node_repl"
    ANARLOG_MCP = "anarlog_mcp"
    STUDIO_BACKEND = "studio_backend"
    HEADLESS_BROWSER = "headless_browser"
    PYTHON = "python"
    NODE = "node"
    SHELL = "shell"
    OTHER = "other"


class AttributionOperation(ContractStrEnum):
    PS = "ps"
    RESIDENT = "resident"
    NATIVE_STATUS = "nativeStatus"
    LOCK_SAMPLE = "lockSample"


class NativeBinary(ContractModel):
    path: str
    sourcePath: str | None = None
    version: str
    sha256: str | None = None
    bundleSha256: str | None = None
    validatedAt: float | None = None
    approvalRevision: int | None = None
    checks: dict[str, JsonValue] | None = None
    companions: dict[str, JsonValue] | None = None
    sourceIdentity: dict[str, JsonValue] | None = None


class NativeCandidate(ContractModel):
    path: str
    status: NativeCandidateState
    version: str | None = None
    identity: dict[str, JsonValue] | None = None
    companionIdentity: dict[str, JsonValue] | None = None
    error: str | None = None


class NativeAccountUpdate(ContractModel):
    status: NativeAccountState
    version: str | None = None
    pid: int | None = None
    targetVersion: str
    reason: str | None = None
    updatedAt: float


class NativeRuntimeStatus(ContractModel):
    status: NativeRuntimeState
    checkedAt: float | None = None
    selected: NativeBinary | None = None
    candidates: list[NativeCandidate]
    accounts: dict[str, NativeAccountUpdate]
    error: str | None = None


class ProviderVersionState(ContractStrEnum):
    CHECKING = "checking"
    CURRENT = "current"
    OUTDATED = "outdated"
    UNKNOWN = "unknown"
    ERROR = "error"


class ProviderVersion(ContractModel):
    id: str
    accountKey: str
    # New provider integrations may report names beyond codex and claude.
    provider: str
    status: ProviderVersionState
    runningVersion: str | None
    installedVersion: str | None
    configuredVersion: str | None
    baseline: str | None
    error: str | None
    message: str | None
    at: float | None


class ProviderVersionWarning(ContractModel):
    id: str
    accountKey: str
    provider: str
    version: str
    baseline: str
    message: str
    at: float


class ProviderVersions(ContractModel):
    checkedAt: float | None
    providers: list[ProviderVersion]
    warnings: list[ProviderVersionWarning]


class BrowserStatus(ContractModel):
    enabled: bool
    reason: str | None = None
    skillRoot: str | None = None


class LiveUpdateState(ContractStrEnum):
    IDLE = "idle"
    APPLYING = "applying"
    APPLIED = "applied"
    WAITING = "waiting"
    FAILED = "failed"


class LiveUpdateStatus(ContractModel):
    """Stable status and receipt written by the local live-patch manager."""

    status: LiveUpdateState
    pid: int | None = None
    managerId: str | None = None
    scope: str | None = None
    receiptError: str | None = None
    manifestId: str | None = None
    manifestHash: str | None = None
    attempt: int | None = None
    checkedAt: float | None = None
    appliedAt: float | None = None
    patchStatus: str | None = None
    error: str | None = None


class SupervisorRecovery(ContractModel):
    degraded: bool | None = None
    fallbackReady: bool | None = None
    blocked: JsonValue = None


class SupervisorHandle(ContractModel):
    id: str
    pid: int | None
    signature: str
    startTime: str | None
    sequence: int
    acknowledged: int
    bufferedBytes: int
    backpressure: bool
    stdoutReaderAlive: bool | None = None
    stdoutReaderError: str | None = None
    persistenceErrors: dict[str, str] | None = None


class SupervisorHealth(ContractModel):
    protocol: int
    stateDir: str
    handles: list[SupervisorHandle]
    journalLimitBytes: int
    outputLimitBytesPerHandle: int | None = None
    durability: str
    recovery: SupervisorRecovery


class SupervisorError(ContractModel):
    error: str


class RestartEnvironmentKey(ContractStrEnum):
    CODEX_HOME = "CODEX_HOME"
    CODEX_CANVAS_CWD = "CODEX_CANVAS_CWD"
    CODEX_CANVAS_CONCURRENCY = "CODEX_CANVAS_CONCURRENCY"
    CODEX_BIN = "CODEX_BIN"
    SHELL = "SHELL"
    LANG = "LANG"
    LC_ALL = "LC_ALL"
    CODEX_AGENTS_SUPERVISOR_MODE = "CODEX_AGENTS_SUPERVISOR_MODE"


class DesktopResponse(ResponseModel):
    application: str
    protocol: int
    mobileProtocol: int
    backendBuild: str
    nativeRuntime: NativeRuntimeStatus | None
    providerVersions: ProviderVersions
    browser: BrowserStatus | None
    liveUpdate: LiveUpdateStatus | None
    restartEnvironment: dict[RestartEnvironmentKey, str]
    publicOrigin: str | None
    pid: int
    supervisorMode: bool
    supervisorRequired: bool
    supervisorError: str | None
    supervisor: SupervisorHealth | SupervisorError | None
    supervisorFallback: bool
    supervisorNotice: str | None
    stateDir: str
    linuxVm: dict[str, JsonValue] | None = None


class HostResources(ContractModel):
    cpuCount: int | None
    totalMemoryBytes: int | None
    availableMemoryBytes: int | None


class ProcessRecord(ContractModel):
    pid: int
    parentPid: int
    kind: ProcessKind
    rssMiB: float
    cpuPercent: float


class ProcessKindTotals(ContractModel):
    count: int
    rssMiB: float
    cpuPercent: float


class ProcessTree(ContractModel):
    rootPid: int
    processes: list[ProcessRecord]
    kinds: dict[ProcessKind, ProcessKindTotals]
    totalRssMiB: float


class ResourceAttribution(ContractModel):
    component: AttributionComponent
    operation: AttributionOperation
    count: int
    logicalReadBytes: int | None = None
    residentBytes: int | None = None
    durationMs: float | None = None


class QueueSizes(ContractModel):
    """Queue labels include generated account aliases and queue names."""

    model_config = {"extra": "allow"}
    __pydantic_extra__: dict[str, int | None]


class RuntimeLockSample(ContractModel):
    waitMs: float
    holder: str | None


class RuntimeLockOperation(ContractModel):
    callSite: str
    count: int
    totalWaitMs: float
    totalHoldMs: float
    waitMs: "LockMetricSummary"
    holdMs: "LockMetricSummary"


class LockMetricSummary(ContractModel):
    p50: float
    p95: float
    max: float
    samples: int


class SqliteMetric(ContractModel):
    transactions: int
    longTransactions: int
    writeWaits: int
    writeWaitMs: float
    lockErrors: int
    longestTransactionMs: float
    p50TransactionMs: float | None = None
    p95TransactionMs: float | None = None
    sites: dict[str, SqliteMetric] = Field(default_factory=dict)


class SqliteFrame(ContractModel):
    file: str
    function: str
    line: int


class SqliteObservation(ContractModel):
    at: float
    durationMs: float
    frames: list[SqliteFrame]


class SqliteTransactionState(ContractStrEnum):
    ACTIVE = "active"
    COMMITTED = "committed"
    ROLLED_BACK = "rolledBack"
    ENDED = "ended"


class SqliteDatabase(ContractStrEnum):
    CANVAS = "canvas.sqlite3"
    ANALYTICS = "analytics.sqlite3"
    OTHER = "other"


class SqliteStatement(ContractStrEnum):
    BEGIN = "BEGIN"
    INSERT = "INSERT"
    UPDATE = "UPDATE"
    DELETE = "DELETE"
    REPLACE = "REPLACE"
    CREATE = "CREATE"
    DROP = "DROP"
    ALTER = "ALTER"
    WITH = "WITH"
    SAVEPOINT = "SAVEPOINT"
    OTHER = "other"


class SqliteTransaction(ContractModel):
    id: str
    pid: int
    site: str
    database: SqliteDatabase
    threadId: int
    nativeThreadId: int
    threadName: str
    originFrames: list[SqliteFrame]
    firstStatement: SqliteStatement
    originRecorded: bool
    startedAtMonotonic: float
    startedAt: float
    observations: list[SqliteObservation]
    durationMs: float
    state: SqliteTransactionState
    frames: list[SqliteFrame] | None = None
    finishedAt: float | None = None


class SqliteJournalStatus(ContractStrEnum):
    NOT_WRITTEN = "notWritten"
    WRITTEN = "written"
    ERROR = "error"


class SqliteJournal(ContractModel):
    status: SqliteJournalStatus
    at: float | None = None
    previousSnapshotAvailable: bool | None = None
    errorType: str | None = None


class SqliteTracking(ContractModel):
    tracked: int
    limit: int
    untrackedStarts: int


class SqliteHistory(ContractModel):
    coverageStartedAt: float
    thresholdMs: float
    recent: list[SqliteTransaction]
    longest: list[SqliteTransaction]
    journal: SqliteJournal
    activeTracking: SqliteTracking


class SqliteActiveTransaction(ContractModel):
    site: str
    startedAtMonotonic: float
    durationMs: float
    threads: list[SqliteThread]


class SqliteThread(ContractModel):
    threadId: int
    threadName: str
    frames: list[SqliteFrame]


class SqliteActiveScan(ContractModel):
    threads: int
    frames: int
    locals: int
    truncated: bool


class SqliteContention(ContractModel):
    transaction: SqliteMetric | None = None
    writeWait: SqliteMetric | None = None
    lockError: SqliteMetric | None = None
    activeTransactions: list[SqliteActiveTransaction]
    activeTransactionScan: SqliteActiveScan
    slowTransactions: SqliteHistory


class NativeCodexAccount(ContractModel):
    provider: Literal["codex"]
    loadedThreads: int


class NativeClaudeAccount(ContractModel):
    provider: Literal["claude"]
    liveQueries: int
    activeTurns: int
    backgroundQueries: int
    idleQueries: int
    idleLimitSeconds: int
    sessionCache: SessionCacheStats


class SessionCacheStats(ContractModel):
    cachedSessions: int
    retainedBytes: int
    pendingWrites: int
    journalBytes: int


class NativeErrorAccount(ContractModel):
    error: str


NativeProviderAccount = Annotated[
    NativeCodexAccount | NativeClaudeAccount,
    Field(discriminator="provider"),
]


class PayloadMigrationName(ContractStrEnum):
    CHECKPOINTS = "checkpoints"
    TOOL_REQUESTS = "tool_requests"
    TOOL_RESULTS = "tool_results"
    TASKS = "tasks"


class PayloadMigrationState(ContractStrEnum):
    PENDING = "pending"
    COMPLETE = "complete"
    RUNNING = "running"
    WAITING_FOR_SPACE = "waitingForSpace"
    ERROR = "error"


class PayloadMigration(ContractModel):
    cursor: int
    complete: bool
    status: PayloadMigrationState
    updated: float | None
    error: str | None


class SearchMigrationPhase(ContractStrEnum):
    BUILDING = "building"
    DROPPING = "dropping"
    WAITING_FOR_SPACE = "waiting_for_space"
    COMPLETE = "complete"


class SearchMigration(ContractModel):
    phase: SearchMigrationPhase
    cursor: int
    updated: float
    error: str | None
    lastBatchBytes: int | None


class TombstonePruneState(ContractStrEnum):
    NOT_STARTED = "notStarted"
    IDLE = "idle"
    RUNNING = "running"
    WAITING_FOR_LOCK = "waitingForLock"
    COMPLETE = "complete"
    ERROR = "error"


class TombstonePruning(ContractModel):
    status: TombstonePruneState
    updated: float
    deleted: int
    remaining: int | None = None
    error: str | None = None


class TombstoneMigration(ContractModel):
    count: int
    floor: int
    pruning: TombstonePruning


class AnalyticsFileMigrationState(ContractStrEnum):
    IDLE = "idle"
    CHECK_SPACE = "checkSpace"
    INSUFFICIENT_SPACE = "insufficientSpace"
    UNSUPPORTED_TABLES = "unsupportedTables"
    MISSING_TARGET_TABLE = "missingTargetTable"
    WAITING_FOR_SPACE = "waitingForSpace"
    COPY = "copy"
    RETIRE = "retire"
    COMPLETE = "complete"
    ERROR = "error"


class AnalyticsFileMigration(ContractModel):
    status: AnalyticsFileMigrationState
    updated: float | None = None
    error: str | None = None


class MigrationStatus(ContractModel):
    search: SearchMigration | None = None
    payloads: dict[PayloadMigrationName, PayloadMigration] | None = None
    entityTombstones: TombstoneMigration | None = None
    stateReadError: str | None = None
    analyticsFile: AnalyticsFileMigration


class AnalyticsCaptureStatus(ContractModel):
    available: bool | None = None
    limit: int | None = None
    byteLimit: int | None = None
    queued: int | None = None
    active: int | None = None
    bytes: int | None = None
    completed: int | None = None
    failed: int | None = None
    overflow: int | None = None
    pendingErrors: int | None = None
    lastError: str | None = None


class ExecutionStatus(ContractStrEnum):
    PENDING = "pending"
    RUNNING = "running"
    ACCEPTED = "accepted"
    OBSERVED = "observed"
    UNKNOWN = "unknown"
    UNSENT = "unsent"
    REJECTED = "rejected"
    COMPLETED = "completed"
    FAILED = "failed"
    INTERRUPTED = "interrupted"
    CANCELLED = "cancelled"
    QUEUED = "queued"
    STARTING = "starting"
    APPROVAL = "approval"
    PAUSED = "paused"
    IDLE = "idle"
    SUBMITTED = "submitted"
    NOT_APPLIED = "not_applied"


class ExecutionAction(ContractStrEnum):
    REVIEW = "review"
    COMPACT = "compact"


class ExecutionActionIdentity(ContractModel):
    accountKey: str
    threadId: str
    epoch: int


class ExecutionInputEvent(ContractModel):
    id: str
    kind: str
    created: float
    assets: list[str]
    textHash: str


class ExecutionClaudeInput(ContractModel):
    input: list[dict[str, JsonValue]]
    source: dict[str, JsonValue]
    clientUserMessageId: str
    configuration: dict[str, JsonValue]
    events: list[ExecutionInputEvent | None]
    transcriptItemId: str
    textHash: str


class ExecutionInputRejection(ContractModel):
    id: str
    epoch: int
    accountKey: str
    threadId: str
    connectionId: str
    nativeOperationId: str
    events: list[str]
    agent: str
    turnId: str | None
    outcome: Literal["not_applied"]
    retry: bool


class ExecutionAttempt(ContractModel):
    id: str
    supervisorIdentity: SupervisorIdentity | None = None
    runId: str | None = None
    epoch: int | None = None
    activeAtReservation: bool | None = None
    settingsFixed: bool | None = None
    claudeRetryOf: str | None = None
    claudeInputRequest: ExecutionClaudeInput | None = None
    claudeInputRejection: ExecutionInputRejection | None = None
    accountKey: str | None = None
    events: list[str] | None = None
    submitted: bool | None = None
    created: float | None = None
    action: ExecutionAction | Literal["capacity", "safety"] | None = None
    capacityRetryId: str | None = None
    reviewTarget: ReviewTargetDto | None = None
    modelSettings: SnapshotModelSettingsDto | None = None
    actionRequestId: str | None = None
    actionIdentity: ExecutionActionIdentity | None = None
    nativeOperationId: str | None = None
    connectionId: str | None = None
    threadId: str | None = None
    turnId: str | None = None
    observedTurnId: str | None = None
    submission: ExecutionStatus | None = None
    prepareAttemptId: str | None = None
    turnStatus: ExecutionStatus | None = None
    resultRunId: str | None = None
    executionOutcome: ExecutionStatus | None = None
    notSubmittedReason: str | None = None
    prepareError: str | None = None
    responseError: str | None = None
    error: str | None = None


class ExecutionNodeKind(ContractStrEnum):
    MANAGED_WORKER = "managed_worker"
    NATIVE_CHILD = "native_child"
    BACKGROUND = "background"


class ExecutionNode(ContractModel):
    id: str
    runId: str
    kind: ExecutionNodeKind
    threadId: str | None = None
    turnId: str | None = None
    status: ExecutionStatus | AgentStatus | None = None
    error: str | dict[str, JsonValue] | None = None
    result: str | None = None
    parentTurnId: str | None = None
    agentId: str | None = None
    resultTurnId: str | None = None


class ExecutionEffectKind(ContractStrEnum):
    SPAWN = "spawn"
    TOOL_REQUESTS = "tool_requests"
    MONITORS = "monitors"
    REQUESTS = "requests"
    TASK_SUBMIT = "task_submit"


class ExecutionEffect(ContractModel):
    id: str
    runId: str
    kind: ExecutionEffectKind
    referenceId: str
    attemptId: str | None = None
    requestId: str | None = None
    status: ExecutionStatus | Literal["answered", "expired", "lost"] | None = None
    outcome: str | None = None
    resultReference: str | None = None
    taskId: str | None = None
    revision: str | None = None


class RecoveryStage(ContractStrEnum):
    INPUT_RESTORED = "input_restored"
    PENDING = "pending"
    SUPERSEDED = "superseded"
    FINISHED = "finished"
    CONTINUED = "continued"
    REATTACHED = "reattached"
    HELD = "held"


class ExecutionHeldOperation(ContractModel):
    id: str
    kind: str
    label: str


class ExecutionRecovery(ContractModel):
    epoch: int | None = None
    accountKey: str | None = None
    threadId: str | None = None
    turnId: str | None = None
    autoWake: bool | None = None
    at: float | None = None
    stage: RecoveryStage | None = None
    startAttempt: ExecutionAttempt | None = None
    attemptId: str | None = None
    eventIds: list[str] | None = None
    previousError: str | dict[str, JsonValue] | None = None
    connectionId: str | None = None
    supervisor: SupervisorIdentity | None = None
    source: str | None = None
    outcome: str | None = None
    status: ExecutionStatus | None = None
    error: str | dict[str, JsonValue] | None = None
    reason: str | None = None
    detail: str | None = None
    reattachedAt: float | None = None
    reconciledAt: float | None = None
    observedAt: float | None = None
    heldAt: float | None = None
    supersededAt: float | None = None
    eventId: str | None = None
    automatic: bool | None = None
    completionDelivered: bool | None = None
    holdOperations: list[ExecutionHeldOperation] | None = None


class ExecutionRun(ContractModel):
    id: str
    agent: str
    accountKey: str
    epoch: int
    threadId: str | None
    turnId: str | None
    created: float
    status: ExecutionStatus
    firstAttemptId: str | None
    rootAttemptId: str | None
    latestAttemptId: str | None
    finished: float | None = None
    result: str | None = None
    resultItemId: str | None = None
    error: str | dict[str, JsonValue] | None = None
    reconciled: bool | None = None
    restartRecovery: ExecutionRecovery | None = None
    disconnectRecovery: ExecutionRecovery | None = None
    connectionRecovery: ExecutionRecovery | None = None
    attempts: list[ExecutionAttempt]
    nodes: list[ExecutionNode]
    effects: list[ExecutionEffect]
    attemptIds: list[str]
    requestIds: list[str]
    inputEventIds: list[str]


class ExecutionLedger(ContractModel):
    runs: list[ExecutionRun]
    limit: int
    relatedLimit: int


class SupervisorDiagnostics(ContractModel):
    mode: bool
    fallback: bool
    notice: str | None
    health: SupervisorHealth | SupervisorError | None = None


class DiagnosticsResponse(ResponseModel):
    at: str
    processTree: ProcessTree
    hostResources: HostResources
    resourceAttribution: list[ResourceAttribution]
    nativeAccounts: dict[str, NativeProviderAccount | NativeErrorAccount]
    studioLoadedThreads: int
    queues: QueueSizes
    runtimeLockSamples: list[RuntimeLockSample]
    executions: ExecutionLedger
    sqliteContention: SqliteContention
    analyticsCapture: AnalyticsCaptureStatus
    migrations: MigrationStatus
    analyticsFileMigration: AnalyticsFileMigration
    searchMigrationError: str | None
    runtimeLockOperations: list[RuntimeLockOperation] | None = None
    supervisor: SupervisorDiagnostics


class DirectoryEntry(ContractModel):
    name: str
    path: str


class LinuxVMSettings(ContractModel):
    cpus: int = Field(ge=1, strict=True)
    memoryBytes: int = Field(ge=1024 ** 3, le=64 * 1024 ** 3, strict=True)
    systemDiskBytes: int = Field(ge=8 * 1024 ** 3, le=128 * 1024 ** 3, strict=True)
    dataDiskBytes: int = Field(ge=8 * 1024 ** 3, le=1024 * 1024 ** 3, strict=True)


class LinuxVMSettingsResponse(ResponseModel):
    settings: LinuxVMSettings
    state: str
    allocatedDiskBytes: int | None = None


class DirectoriesResponse(ResponseModel):
    path: str
    parent: str | None
    directories: list[DirectoryEntry]
