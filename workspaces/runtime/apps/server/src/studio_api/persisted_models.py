"""Contracts for JSON files and workspace usage data persisted by the server."""
from __future__ import annotations

from typing import Annotated, Any, Literal, cast

from pydantic import AfterValidator, ConfigDict, Field, StrictBool, StrictFloat, StrictInt, StrictStr
from pydantic_core import PydanticCustomError

from studio_api.accounts.models import UsageLimitsResponse
from studio_api.models import MAX_SAFE_INTEGER, ContractModel, JsonValue, SafeInt
from studio_api.workspace_operation_models import (
    PersistedWorkspaceOperationRecord, WorkspaceCheckpointAgentRecord, WorkspaceCheckpointHistoryRecord,
)
from studio_api.relocation_models import RelocationFormats
from studio_api.persisted_federation import FEDERATION_SCHEMA_MODELS
from studio_api.persisted_account_transfer import PersistedAccountTransferRecord, PersistedTransferAgentRecord


def _unicode_scalar(value: str) -> str:
    if any(0xD800 <= ord(character) <= 0xDFFF for character in value):
        raise PydanticCustomError("unicode_scalar", "string must contain only Unicode scalar values")
    return value


UnicodeScalar = Annotated[StrictStr, AfterValidator(_unicode_scalar)]


class ClaudeCustomModelRecord(ContractModel):
    id: StrictStr
    label: UnicodeScalar
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ClaudeOptionsRecord(ContractModel):
    binaryPath: StrictStr | None = None
    configDir: StrictStr | None = None
    launchArgs: StrictStr | None = None
    autoCompactWindow: SafeInt | None = None
    customModels: list[ClaudeCustomModelRecord] | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class PersistedTranscriptAsset(ContractModel):
    """Full attachment row; unlike the HTTP projection this retains its path."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    agent: StrictStr
    name: StrictStr
    path: StrictStr
    mime: StrictStr
    image: StrictBool
    size: SafeInt
    hash: StrictStr
    created: float


class PersistedAgentArchive(ContractModel):
    """Archive identity consumed by lifecycle restoration; other audit data stays open."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    at: float
    epoch: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
    cleanupPending: StrictBool | None = None


class PersistedAgentRecord(ContractModel):
    """Persisted agent identity plus the extensible state written by the runtime."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    rootId: StrictStr
    epoch: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
    status: StrictStr
    cwd: StrictStr | None = None
    name: StrictStr | None = None
    sandbox: dict[StrictStr, JsonValue] | None = None
    approvalPolicy: StrictStr | None = None
    parentId: StrictStr | None = None
    isLead: StrictBool | None = None
    deletedAt: StrictBool | float | None = None
    turnId: StrictStr | None = None
    threadId: StrictStr | None = None
    accountKey: StrictStr | None = None
    model: StrictStr | None = None

    # Lifecycle fields are optional so older Python-written agents remain readable.
    agentArchive: PersistedAgentArchive | None = None
    autoWake: StrictBool | None = None
    cleanedImageWorkspace: dict[StrictStr, JsonValue] | None = None
    cleanedWorktree: dict[StrictStr, JsonValue] | None = None
    deletedAt: StrictBool | float | None = None
    executionMode: StrictStr | None = None
    imageWorkspace: StrictBool | None = None
    imageWorkspaceReady: StrictBool | None = None
    inFlight: StrictBool | None = None
    isLead: StrictBool | None = None
    parentId: StrictStr | None = None
    parkedEvent: StrictStr | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    worktreeCleanup: dict[StrictStr, JsonValue] | None = None
    worktreeReady: StrictBool | None = None


class _OpenPersistedRecord(ContractModel):
    """A storage projection that validates only fields used by Rust readers."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr


class PersistedSupervisor(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    generation: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] | None = None


class PersistedToolRequestRecord(_OpenPersistedRecord):
    agent: StrictStr
    stage: Literal["queued", "running", "cancelled", "interrupted", "failed", "unknown", "completed"]
    outcome: Literal["pending", "applied", "not_applied", "unknown"]
    tool: StrictStr | None = None
    signature: StrictStr | None = None
    cancelRequested: StrictBool = False
    readOnly: StrictBool = False
    result: PersistedToolResult | None = None
    agentIds: list[StrictStr] = Field(default_factory=list)
    created: float | None = None
    updated: float | None = None
    finished: float | None = None
    epoch: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] | None = None
    accountKey: StrictStr | None = None
    threadId: StrictStr | None = None
    callId: StrictStr | None = None
    request_id: StrictStr | None = None
    operationResult: dict[StrictStr, JsonValue] | None = None
    supervisor: PersistedSupervisor | None = None
    timing: dict[StrictStr, JsonValue] = Field(default_factory=dict)


class PersistedToolAlias(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    agent: StrictStr
    alias: StrictStr
    request: StrictStr


class PersistedToolContent(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    type: StrictStr | None = None
    text: StrictStr | None = None


class PersistedToolResult(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    success: StrictBool | None = None
    contentItems: list[PersistedToolContent] | None = None
    serverOutputExpiresAt: float | None = None


class PersistedOperationResult(ContractModel):
    """Opaque operation answer; only its presence is consumed by receipt reads."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class PersistedExecutionRecord(_OpenPersistedRecord):
    runId: StrictStr | None = None
    latestAttemptId: StrictStr | None = None
    kind: StrictStr | None = None
    status: StrictStr | None = None
    outcome: StrictStr | None = None
    resultReference: StrictStr | None = None


class PersistedPeerTeam(_OpenPersistedRecord):
    name: StrictStr
    members: list[StrictStr]


class PersistedRoomRecord(_OpenPersistedRecord):
    kind: StrictStr
    members: list[StrictStr] = Field(default_factory=list)
    rootId: StrictStr | None = None
    customName: StrictStr | None = None
    updated: float | None = None
    projectPath: StrictStr | None = None
    userHidden: StrictBool | None = None
    radio: dict[StrictStr, JsonValue] | None = None


class PersistedProjectLocationOutbox(_OpenPersistedRecord):
    """Only the durable outbox identity is consumed by the location dependency gate."""


class PersistedProjectRecord(_OpenPersistedRecord):
    path: StrictStr | None = None
    name: StrictStr | None = None
    peerTeams: list[PersistedPeerTeam] = Field(default_factory=list)


class PersistedFederationMember(_OpenPersistedRecord):
    name: StrictStr | None = None


class PersistedFederationRoomRecord(_OpenPersistedRecord):
    status: StrictStr
    localApproved: StrictBool = False
    remoteApproved: StrictBool = False
    localMembers: list[StrictStr] = Field(default_factory=list)
    remoteMembers: list[PersistedFederationMember] = Field(default_factory=list)
    localParticipants: list[dict[StrictStr, JsonValue]] = Field(default_factory=list)
    peerId: StrictStr
    peerLabel: StrictStr
    name: StrictStr


class PersistedMessageEvent(_OpenPersistedRecord):
    status: Literal["pending", "reserved", "dispatching", "uncertain", "failed", "cancelled",
                    "delivered", "stored_only", "queued", "accepted", "sent"]
    error: StrictStr | None = None


class PersistedChatMessage(_OpenPersistedRecord):
    seq: Annotated[StrictInt, Field(ge=1, le=MAX_SAFE_INTEGER)]
    room: StrictStr
    sender: StrictStr
    text: StrictStr
    created: float
    deliveries: dict[StrictStr, StrictStr]


class PersistedReadState(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    read: StrictBool | None = None


class PersistedSummaryQuestion(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    question: StrictStr | None = None


class PersistedSummaryParams(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    questions: list[PersistedSummaryQuestion] | None = None


class PersistedSummaryValue(_OpenPersistedRecord):
    """Fields consumed from the durable sidebar projection, not an HTTP DTO."""

    name: StrictStr | None = None
    path: StrictStr | None = None
    source: StrictStr | None = None
    status: StrictStr | None = None
    hostProjectPath: StrictStr | None = None
    cwd: StrictStr | None = None
    isLead: StrictBool | None = None
    inFlight: StrictBool | None = None
    archived: StrictBool | None = None
    pinned: StrictBool | None = None
    userHidden: StrictBool | None = None
    needsUserResponse: StrictBool | None = None
    deferred: StrictBool | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    lastCompletedTurn: StrictStr | None = None
    lastCompletedTurnStatus: StrictStr | None = None
    readState: PersistedReadState | None = None
    members: list[StrictStr] | None = None
    peerTeams: list[PersistedPeerTeam] | None = None
    locations: list[dict[StrictStr, JsonValue]] | None = None
    projectAliases: list[dict[StrictStr, JsonValue]] | None = None
    serverId: StrictStr | None = None
    projectId: StrictStr | None = None
    projectServerId: StrictStr | None = None
    homeServerId: StrictStr | None = None
    projectPath: StrictStr | None = None
    sharedRoomId: StrictStr | None = None
    provider: StrictStr | None = None
    agent: StrictStr | None = None
    leadId: StrictStr | None = None
    tail: StrictStr | None = None
    title: StrictStr | None = None
    created: float | None = None
    updated: float | None = None
    version: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] | None = None
    params: PersistedSummaryParams | None = None


class PersistedSyncEntity(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    value: PersistedSummaryValue


class PersistedAccessRecord(_OpenPersistedRecord):
    kind: StrictStr | None = None
    status: StrictStr | None = None
    revoked: float | None = None


class PersistedTaskRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    status: StrictStr | None = None


class PersistedPlanRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    agent: StrictStr | None = None
    id: StrictStr
    rootId: StrictStr
    text: StrictStr
    version: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
    updated: SafeInt | StrictFloat | None
    steps: list[JsonValue] = Field(default_factory=list)
    native: dict[StrictStr, JsonValue] | None = None


class PersistedAnnotationRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    path: StrictStr | None = None
    line: SafeInt | None = None
    text: StrictStr | None = None
    created: StrictFloat | StrictInt | None = None


class PersistedRequestRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    connectionId: StrictStr | None = None
    status: StrictStr | None = None

    method: StrictStr | None = None


class PersistedComplaintRecord(_OpenPersistedRecord):
    leadId: StrictStr | None = None
    author: StrictStr | None = None
    text: StrictStr | None = None
    status: StrictStr | None = None


class PersistedCheckpointRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    created: StrictFloat | StrictInt | None = None


class MonitorOperationRecord(ContractModel):
    """Exact native command ownership; extra transport evidence remains readable."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    agent: StrictStr = Field(min_length=1, max_length=200)
    epoch: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
    accountKey: StrictStr = Field(min_length=1, max_length=200)
    connectionId: StrictStr = Field(min_length=1, max_length=200)


class PersistedMonitorResult(ContractModel):
    """First definitive command result, flushed before the monitor/event transaction."""

    version: Literal[1]
    key: StrictStr = Field(min_length=1, max_length=200)
    code: StrictInt | None
    error: StrictStr | None = Field(default=None, max_length=12000)
    operation: MonitorOperationRecord | None
    finished: float = Field(gt=0)


class PersistedMonitorRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    ruleId: StrictStr | None = None
    status: Literal["approval", "starting", "running", "completed", "failed", "cancelled", "lost"] | None = None
    lastExitCode: StrictInt | None = None
    epoch: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] | None = None
    command: StrictStr = Field(default="", max_length=12000)
    cwd: StrictStr = ""
    timeout_ms: Annotated[StrictInt, Field(ge=1000, le=86400000)] = 3600000
    stallTimeoutSeconds: Annotated[StrictInt, Field(ge=0, le=31536000)] = 1800
    wakeOn: Literal["exit", "failure"] = "exit"
    livenessCommand: StrictStr = Field(default="", max_length=12000)
    successExitCodes: list[Annotated[StrictInt, Field(ge=0, le=255)]] = Field(default_factory=lambda: [0], min_length=1, max_length=16)
    created: float = Field(default=0, ge=0)
    finished: float | None = Field(default=None, ge=0)
    activityAt: float = Field(default=0, ge=0)
    activityGeneration: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0
    stallWakeGeneration: Annotated[StrictInt, Field(ge=-1, le=MAX_SAFE_INTEGER)] = -1
    stallProbeGeneration: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] | None = None
    stallProbeStarted: float | None = Field(default=None, ge=0)
    cancelRequested: StrictBool = False
    configurationPending: StrictBool = False
    interactive: StrictBool = False
    stdinClosed: StrictBool = False
    stdinCloseRequested: StrictStr | None = None
    operation: MonitorOperationRecord | None = None
    inputAttempt: dict[str, JsonValue] | None = None
    log: StrictStr = ""
    tail: StrictStr = Field(default="", max_length=12000)
    bytes: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0
    exitCode: StrictInt | None = None
    error: StrictStr | None = Field(default=None, max_length=12000)


class PersistedTerminalRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    status: StrictStr | None = None
    exitCode: StrictInt | None = None


class PersistedUsageResumeRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    status: StrictStr | None = None
    cause: StrictStr | None = None
    failedAt: StrictFloat | StrictInt | None = None


class PersistedWorkItemRecord(_OpenPersistedRecord):
    rootId: StrictStr | None = None
    status: StrictStr | None = None
    title: StrictStr | None = None

    owner: StrictStr | None = None
    releases: list[JsonValue] = Field(default_factory=list)
    version: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0


class PersistedRuleRecord(_OpenPersistedRecord):
    """Open rule records; only fields consumed by the rule engine are constrained."""

    kind: Literal["interval", "once", "file", "event", "low_workers"] = "interval"
    agent: StrictStr = ""
    rootId: StrictStr = ""
    epoch: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0
    name: Annotated[StrictStr, Field(max_length=120)] = ""
    status: Literal["active", "paused", "completed"] = "active"
    intervalSeconds: Annotated[StrictInt, Field(ge=10, le=31536000)] = 60
    nextAt: StrictFloat | StrictInt = 0
    created: StrictFloat | StrictInt = 0
    at: StrictFloat | StrictInt = 0
    path: StrictStr | None = None
    event: StrictStr = ""
    command: Annotated[StrictStr, Field(max_length=12000)] = ""
    text: StrictStr = ""
    inFlight: StrictBool = False
    checks: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0
    wakes: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0
    fingerprint: tuple[
        Annotated[StrictInt, Field(ge=-(2**63), le=2**63 - 1)],
        Annotated[StrictInt, Field(ge=0, le=2**64 - 1)],
        Annotated[StrictInt, Field(ge=0, le=2**64 - 1)],
    ] | None = None
    fileActivityAt: StrictFloat | StrictInt = 0
    fileGeneration: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0
    stallWakeGeneration: Annotated[StrictInt, Field(ge=-1, le=MAX_SAFE_INTEGER)] = -1
    stallTimeoutSeconds: Annotated[StrictInt, Field(ge=0, le=31536000)] = 1800
    livenessCommand: Annotated[StrictStr, Field(max_length=12000)] = ""
    stallProbe: StrictBool = False
    stallEventKey: StrictStr = ""
    stallText: StrictStr = ""
    minimumWorkers: Annotated[StrictInt, Field(ge=1, le=255)] = 1
    durationMinutes: Annotated[StrictInt, Field(ge=1, le=525600)] = 1
    activeWorkers: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)] = 0
    restartCheck: dict[StrictStr, JsonValue] | None = None
    alerted: StrictBool = False
    lowSince: StrictFloat | StrictInt | None = None
    lastEvent: StrictStr = ""
    eventText: Annotated[StrictStr, Field(max_length=12000)] = ""
    lastOutput: Annotated[StrictStr, Field(max_length=12000)] = ""
    lastAt: StrictFloat | StrictInt = 0
    lastFinished: StrictFloat | StrictInt = 0
    lastStallFinished: StrictFloat | StrictInt = 0
    lastExitCode: StrictInt | None = None
    lastStallExitCode: StrictInt | None = None
    lastStallError: StrictStr | None = None
    error: StrictStr | None = None

class PersistedTranscriptInput(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr | None = None
    kind: StrictStr | None = None
    text: StrictStr | None = None
    truncated: StrictBool | None = None


class PersistedTranscriptRecord(_OpenPersistedRecord):
    """Legacy parent text is hydrated from fulltext before response validation."""

    role: StrictStr | None = None
    text: StrictStr | None = None
    inputs: list[PersistedTranscriptInput] | None = None


class RuntimeEventMetadataRecord(ContractModel):
    """Optional event metadata row, keyed by runtime_events.id, not JSON id."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    transcriptItemId: StrictStr | None = None
    assets: list[JsonValue] = Field(default_factory=list)
    timing: dict[StrictStr, JsonValue] | None = None


class PersistedGraphAgentRecord(_OpenPersistedRecord):
    pass


class PersistedProfileRecord(ContractModel):
    """Stored profile contract; unknown future fields remain readable."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    name: StrictStr
    role: Literal["reviewer", "implementer"]
    model: StrictStr
    effort: Literal["low", "medium", "high", "xhigh", "max", "ultra"] | None
    instructions: StrictStr


class AccountRecord(ContractModel):
    id: StrictStr
    home: StrictStr
    label: UnicodeScalar
    source: StrictStr
    provider: Literal["codex", "claude"] | None = None
    accountId: StrictStr | None = None
    email: StrictStr | None = None
    plan: StrictStr | None = None
    status: Literal["ready", "signedOut", "pending", "error", "changed", "duplicate"]
    error: StrictStr | None = None
    disconnected: StrictBool | None = None
    deleted: StrictBool | None = None
    duplicateOf: StrictStr | None = None
    claudeOptions: ClaudeOptionsRecord | None = None
    credentialIdentity: StrictStr | None = Field(default=None, alias="_credentialIdentity")
    authErrorKind: StrictStr | None = Field(default=None, alias="_authErrorKind")


class CodexLoginReceiptRecord(ContractModel):
    requestId: StrictStr
    accountKey: StrictStr
    status: Literal["starting", "pending", "ready", "duplicate", "cancelled", "error", "uncertain"]
    createdAt: StrictFloat | None = None
    loginId: StrictStr | None = None
    verificationUrl: StrictStr | None = None
    userCode: StrictStr | None = None
    error: StrictStr | None = None
    email: StrictStr | None = None
    resolvedAccountKey: StrictStr | None = None
    reauthAccountKey: StrictStr | None = None
    expiresAt: StrictFloat | StrictInt | None = None
    expectedAccountId: StrictStr | None = None
    emailHint: StrictStr | None = None
    requestedLabel: UnicodeScalar | None = None
    nativeCompleted: StrictBool | None = None


class DeleteReceiptRecord(ContractModel):
    accountKey: StrictStr
    deletedAt: StrictFloat


class NameReceiptRecord(ContractModel):
    accountKey: StrictStr
    label: UnicodeScalar


class AccountRegistryRecord(ContractModel):
    version: Literal[1]
    defaultAccountKey: StrictStr
    accounts: dict[StrictStr, AccountRecord]
    logins: dict[StrictStr, CodexLoginReceiptRecord] = Field(default_factory=dict)
    deleteReceipts: dict[StrictStr, DeleteReceiptRecord] = Field(default_factory=dict)
    nameReceipts: dict[StrictStr, NameReceiptRecord] = Field(default_factory=dict)
    pendingAccountsChange: StrictBool = False


class UsageLimitsWorkspaceEnvelope(ContractModel):
    """Workspace storage projection; usage snapshot values share the API model."""

    rateLimits: UsageLimitsResponse | None = None
    rateLimitsByAccount: dict[StrictStr, UsageLimitsResponse] = Field(default_factory=dict)
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

STATE_SCHEMA_MODELS = {
    **FEDERATION_SCHEMA_MODELS,
    "account-registry.schema.json": AccountRegistryRecord,
    "persisted-access-record.schema.json": PersistedAccessRecord,
    "persisted-account-transfer-record.schema.json": PersistedAccountTransferRecord,
    "persisted-agent-record.schema.json": PersistedAgentRecord,
    "persisted-annotation-record.schema.json": PersistedAnnotationRecord,
    "persisted-chat-message.schema.json": PersistedChatMessage,
    "persisted-relocation.schema.json": RelocationFormats,
    "persisted-checkpoint-record.schema.json": PersistedCheckpointRecord,
    "persisted-complaint-record.schema.json": PersistedComplaintRecord,
    "persisted-execution-record.schema.json": PersistedExecutionRecord,
    "persisted-federation-room-record.schema.json": PersistedFederationRoomRecord,
    "persisted-graph-agent-record.schema.json": PersistedGraphAgentRecord,
    "persisted-message-event.schema.json": PersistedMessageEvent,
    "persisted-monitor-record.schema.json": PersistedMonitorRecord,
    "persisted-monitor-result.schema.json": PersistedMonitorResult,
    "persisted-operation-result.schema.json": PersistedOperationResult,
    "persisted-plan-record.schema.json": PersistedPlanRecord,
    "persisted-profile-record.schema.json": PersistedProfileRecord,
    "persisted-project-location-outbox.schema.json": PersistedProjectLocationOutbox,
    "persisted-project-record.schema.json": PersistedProjectRecord,
    "persisted-request-record.schema.json": PersistedRequestRecord,
    "persisted-room-record.schema.json": PersistedRoomRecord,
    "persisted-rule-record.schema.json": PersistedRuleRecord,
    "persisted-sync-entity.schema.json": PersistedSyncEntity,
    "persisted-task-record.schema.json": PersistedTaskRecord,
    "persisted-terminal-record.schema.json": PersistedTerminalRecord,
    "persisted-tool-alias.schema.json": PersistedToolAlias,
    "persisted-tool-request-record.schema.json": PersistedToolRequestRecord,
    "persisted-tool-result.schema.json": PersistedToolResult,
    "persisted-transcript-record.schema.json": PersistedTranscriptRecord,
    "persisted-usage-resume-record.schema.json": PersistedUsageResumeRecord,
    "persisted-work-item-record.schema.json": PersistedWorkItemRecord,
    "persisted-transfer-agent-record.schema.json": PersistedTransferAgentRecord,
    "runtime-event-metadata.schema.json": RuntimeEventMetadataRecord,
    "runtime-transcript-asset.schema.json": PersistedTranscriptAsset,
    "persisted-workspace-operation-record.schema.json": PersistedWorkspaceOperationRecord,
    "workspace-checkpoint-agent-record.schema.json": WorkspaceCheckpointAgentRecord,
    "workspace-checkpoint-history-record.schema.json": WorkspaceCheckpointHistoryRecord,
    "workspace-usage-limits.schema.json": UsageLimitsWorkspaceEnvelope,
}


def state_schema_document(model: Any) -> dict[str, object]:
    schema = model.model_json_schema(mode="validation")

    def omit_defaults(value: object) -> None:
        if isinstance(value, dict):
            value.pop("default", None)
            for child in value.values():
                omit_defaults(child)
        elif isinstance(value, list):
            for child in value:
                omit_defaults(child)

    omit_defaults(schema)
    return cast(dict[str, object], schema)
