"""Wire models for desktop status, diagnostics, and directory browsing."""
from __future__ import annotations

from pydantic import Field

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel


class NativeRuntimeState(ContractStrEnum):
    CHECKING = "checking"
    READY = "ready"
    FAILED = "failed"


class DesktopQuery(ContractModel):
    account_key: str = "default"


class DirectoriesQuery(ContractModel):
    path: str | None = None


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
    """Live patch status has a typed stable core and permits patch-specific metadata."""

    model_config = {"extra": "allow"}
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


class SupervisorHealth(ContractModel):
    protocol: int
    stateDir: str
    handles: list[SupervisorHandle]
    journalLimitBytes: int
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
    browser: BrowserStatus | None
    liveUpdate: LiveUpdateStatus | None
    restartEnvironment: dict[RestartEnvironmentKey, str]
    publicOrigin: str
    pid: int
    supervisorMode: bool
    supervisorRequired: bool
    supervisorError: str | None
    supervisor: SupervisorHealth | SupervisorError | None
    supervisorFallback: bool
    supervisorNotice: str | None
    stateDir: str


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
    provider: "NativeProvider"
    loadedThreads: int


class NativeProvider(ContractStrEnum):
    CODEX = "codex"
    CLAUDE = "claude"


class NativeProviderAccount(ContractModel):
    """Provider diagnostics are intentionally extensible by the native provider."""

    model_config = {"extra": "allow"}
    __pydantic_extra__: dict[str, JsonValue]
    provider: NativeProvider


class NativeErrorAccount(ContractModel):
    error: str


class MigrationRecord(ContractModel):
    cursor: int
    complete: bool
    status: str
    updated: float | str | None
    error: str | None


class SearchMigration(ContractModel):
    phase: str
    cursor: int
    updated: float | str | None
    error: str | None
    lastBatchBytes: int | None


class TombstoneMigration(ContractModel):
    count: int
    floor: int
    pruning: dict[str, JsonValue]


class MigrationStatus(ContractModel):
    """Migration names and payloads evolve independently with stored schemas."""

    model_config = {"extra": "allow"}
    __pydantic_extra__: dict[str, JsonValue]
    search: SearchMigration | None = None
    payloads: dict[str, MigrationRecord] | None = None
    entityTombstones: TombstoneMigration | None = None
    stateReadError: str | None = None
    analyticsFile: dict[str, JsonValue]


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
    nativeAccounts: dict[str, NativeCodexAccount | NativeProviderAccount | NativeErrorAccount]
    studioLoadedThreads: int
    queues: QueueSizes
    runtimeLockSamples: list[RuntimeLockSample]
    executions: dict[str, JsonValue]
    sqliteContention: SqliteContention
    analyticsCapture: dict[str, JsonValue]
    migrations: MigrationStatus
    analyticsFileMigration: dict[str, JsonValue]
    searchMigrationError: str | None
    runtimeLockOperations: list[RuntimeLockOperation] | None = None
    supervisor: SupervisorDiagnostics


class DirectoryEntry(ContractModel):
    name: str
    path: str


class DirectoriesResponse(ResponseModel):
    path: str
    parent: str | None
    directories: list[DirectoryEntry]
