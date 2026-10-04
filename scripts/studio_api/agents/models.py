"""Typed HTTP contracts for agent lifecycle and control operations."""

from __future__ import annotations

from uuid import UUID
from typing import Literal

from pydantic import Field, field_validator, model_validator

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel
from studio_api.sync.models import (
    AgentEntityDto,
    CapacityRetryDto,
    CapacityRetrySettingsDto,
    CapacityRetryStatus,
    UsageResumeCause,
    UsageResumeDto,
    UsageResumeStatus,
)


class AgentResponse(AgentEntityDto, ResponseModel):
    """Agent entity response that also accepts the HTTP sync envelope."""


class AgentRole(ContractStrEnum):
    ORCHESTRATOR = "orchestrator"
    IMPLEMENTER = "implementer"
    REVIEWER = "reviewer"


class AgentMode(ContractStrEnum):
    MULTI = "multi"
    SINGLE = "single"


class NativeAction(ContractStrEnum):
    COMPACT = "compact"
    REVIEW = "review"


class RetryAction(ContractStrEnum):
    RETRY = "retry"
    CANCEL = "cancel"


class TransferAction(ContractStrEnum):
    RETRY = "retry"
    CANCEL = "cancel"
    FINISH_HISTORY = "finish_history"


class SafetyAction(ContractStrEnum):
    WAIT = "wait"
    RETRY = "retry"
    CANCEL = "cancel"


class NativeCommandAction(ContractStrEnum):
    INPUT = "input"
    CANCEL = "cancel"


class TransferScope(ContractStrEnum):
    TEAM = "team"
    SUBAGENTS = "subagents"


class RecoveryStatus(ContractStrEnum):
    ADOPTED = "adopted"
    INPUT_RESTORED = "input_restored"
    RECONCILED = "reconciled"
    TOOL_RESPONSE_DELIVERED = "tool_response_delivered"
    SUPERSEDED = "superseded"
    UNCONFIRMED = "unconfirmed"


class RecoveryOutcome(ContractStrEnum):
    COMPLETED = "completed"
    FAILED = "failed"
    INTERRUPTED = "interrupted"


class NativeCommandEventStatus(ContractStrEnum):
    PENDING = "pending"
    RESERVED = "reserved"
    DISPATCHING = "dispatching"
    DELIVERED = "delivered"
    UNCERTAIN = "uncertain"
    FAILED = "failed"
    CANCELLED = "cancelled"
    EXPIRED = "expired"
    STORED_ONLY = "stored_only"


class TransferStatus(ContractStrEnum):
    PENDING = "pending"
    COMPLETED = "completed"
    CANCELLED = "cancelled"


class TransferMemberPhase(ContractStrEnum):
    WAITING = "waiting"
    CHECKING = "checking"
    READING = "reading"
    SUBMITTING = "submitting"
    SUBMITTED = "submitted"
    UNKNOWN = "unknown"
    READY = "ready"
    BLOCKED = "blocked"
    LAZY = "lazy"
    LAZY_SUBMITTED = "lazy_submitted"
    INTERRUPTING = "interrupting"
    COMPLETED = "completed"
    LEFT = "left"


class TransferInterruptOutcome(ContractStrEnum):
    ACKNOWLEDGED = "acknowledged"
    UNKNOWN = "unknown"


class NativeActionReceiptStatus(ContractStrEnum):
    ACCEPTED = "accepted"


class NativeActionOutcomeStatus(ContractStrEnum):
    PENDING = "pending"
    ACKNOWLEDGED = "acknowledged"
    UNKNOWN = "unknown"
    FAILED = "failed"


class SafetyActionStage(ContractStrEnum):
    WAITING = "waiting"
    TURNS = "turns"
    ITEMS = "items"
    INTERRUPT = "interrupt"
    VERIFY_TURNS = "verify_turns"
    VERIFY_ITEMS = "verify_items"
    FORK = "fork"
    START = "start"
    RUNNING = "running"
    FAILED = "failed"
    CANCELLED = "cancelled"
    UNKNOWN = "unknown"


class PreparationSettings(ResponseModel):
    provider: str | None = None
    model: str | None = None
    effort: str | None = None
    nativeEffort: str | None = None
    fastMode: bool | None = None
    yoloMode: bool | None = None
    daybreakEnabled: bool | None = None
    cyberAccessProgram: str | None = None
    profileInstructions: str | None = None
    role: AgentRole | None = None
    accountKey: str | None = None
    workerDefaults: PreparationSettings | None = None
    pendingSettings: PreparationSettings | None = None
    pendingSettingsAccountKey: str | None = None
    claudeOptions: JsonValue | None = None
    # Versioned portable-history descriptor is a separate extensible archive contract.
    portableHistory: JsonValue | None = None


class TransferMemberResponse(ResponseModel):
    phase: TransferMemberPhase
    sourceAccountKey: str | None = None
    sourceThreadId: str | None = None
    targetThreadId: str | None = None
    provider: str | None = None
    name: str | None = None
    reason: str | None = None
    error: str | None = None
    waiting: str | None = None
    lazy: bool | None = None
    interruptReason: str | None = None
    interruptOutcome: TransferInterruptOutcome | None = None
    continueAfterTransfer: bool | None = None


class TransferMemberNotice(ResponseModel):
    id: str
    name: str | None = None
    provider: str | None = None
    reason: str | None = None


class TransferRequestReceipt(ResponseModel):
    leadId: str
    targetAccountKey: str
    scope: TransferScope | None = None


class CreateLeadRequest(ContractModel):
    id: str | None = Field(default=None, min_length=1, max_length=200)
    previous: str | None = Field(default=None, max_length=200)
    reuse_empty: bool | None = None
    cwd: str | None = None
    project_folder: str | None = None
    account_key: str | None = Field(default=None, min_length=1, max_length=200)
    model: str | None = Field(default=None, min_length=1, max_length=300)
    yolo_mode: bool | None = None

    @field_validator("model")
    @classmethod
    def model_not_blank(cls, value: str | None) -> str | None:
        if value is not None and not value.strip():
            raise ValueError("Selected model must not be empty")
        return value


class WorkerDefaults(ContractModel):
    model: str | None = Field(min_length=1, max_length=300)
    effort: str | None = Field(max_length=100)
    fast_mode: bool
    daybreak_enabled: bool | None = None
    account_key: str | None = Field(default=None, max_length=200)

    @field_validator("effort")
    @classmethod
    def effort_not_blank(cls, value: str | None) -> str | None:
        if value is not None and not value.strip():
            raise ValueError("Reasoning effort must be a non-empty string or null")
        return value


class ReviewDefaults(ContractModel):
    model: str | None = Field(max_length=300)
    effort: str | None = Field(max_length=100)

    @field_validator("model", "effort")
    @classmethod
    def selected_value_not_blank(cls, value: str | None) -> str | None:
        if value is not None and not value.strip():
            raise ValueError("Review model and effort must be non-empty strings or null")
        return value


class CreateAgentRequest(ContractModel):
    id: str | None = Field(default=None, min_length=1, max_length=200)
    parent: str | None = Field(default=None, min_length=1, max_length=200)
    cwd: str | None = None
    prompt: str
    name: str | None = None
    role: AgentRole | None = None
    model: str | None = Field(default=None, min_length=1, max_length=300)
    effort: str | None = Field(default=None, max_length=100)
    fast_mode: bool | None = None
    daybreak_enabled: bool | None = None
    yolo_mode: bool | None = None
    account_key: str | None = Field(default=None, max_length=200)
    worker_defaults: WorkerDefaults | None = None
    profile_id: str | None = Field(default=None, max_length=200)
    concurrency: int | None = Field(default=None, ge=0, le=512)
    maxAgents: int | None = Field(default=None, ge=1, le=1024)
    tokenBudget: int | None = Field(default=None, gt=0)

    @field_validator("id")
    @classmethod
    def id_is_uuid(cls, value: str | None) -> str | None:
        if value is not None:
            UUID(value)
        return value

    @field_validator("prompt")
    @classmethod
    def prompt_has_runtime_length(cls, value: str) -> str:
        if not 1 <= len(value.strip()) <= 32000:
            raise ValueError("Task must have 1 to 32000 characters")
        return value

    @field_validator("name")
    @classmethod
    def name_has_runtime_length(cls, value: str | None) -> str | None:
        if value is not None and not 1 <= len(value.strip()) <= 100:
            raise ValueError("Agent name must have 1 to 100 characters")
        return value

    @field_validator("model", "effort")
    @classmethod
    def execution_name_not_blank(cls, value: str | None) -> str | None:
        if value is not None and not value.strip():
            raise ValueError("Selected model and reasoning effort must not be empty")
        return value


class ConversationRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    cwd: str | None = None
    yolo_mode: bool | None = None
    expected_account_key: str | None = Field(default=None, min_length=1, max_length=200)
    model: str | None = Field(default=None, min_length=1, max_length=300)
    effort: str | None = Field(default=None, max_length=100)
    fast_mode: bool | None = None
    daybreak_enabled: bool | None = None
    next_turn: bool | None = None
    request_id: str | None = Field(default=None, min_length=1, max_length=200)
    worker_defaults: WorkerDefaults | None = None
    review_defaults: ReviewDefaults | None = None
    agent_mode: AgentMode | None = None
    subagent_concurrency: int | None = Field(default=None, ge=0, le=512)
    expected_mode_revision: int | None = Field(default=None, ge=0)

    @field_validator("model", "effort")
    @classmethod
    def execution_name_not_blank(cls, value: str | None) -> str | None:
        if value is not None and not value.strip():
            raise ValueError("Selected model and reasoning effort must not be empty")
        return value


class ConfigureRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    concurrency: int | None = Field(default=None, ge=0, le=512)
    expected_mode_revision: int | None = Field(default=None, ge=0)
    request_id: str | None = Field(default=None, min_length=1, max_length=200)
    maxAgents: int | None = Field(default=None, ge=1, le=1024)
    tokenBudget: int | None = Field(default=None, gt=0)


class AccountSelectionRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    account_key: str = Field(min_length=1, max_length=200)
    cwd: str | None = None


class TransferStartRequest(ContractModel):
    id: str | None = Field(default=None, min_length=1, max_length=200)
    account_key: str | None = Field(default=None, min_length=1, max_length=200)
    request_id: str = Field(min_length=1, max_length=200)
    scope: TransferScope = TransferScope.TEAM


class TransferActionRequest(ContractModel):
    request_id: str = Field(min_length=1, max_length=200)
    action: TransferAction
    # Legacy action callers may include these, but the route ignores them.
    id: str | None = Field(default=None, min_length=1, max_length=200)
    account_key: str | None = Field(default=None, min_length=1, max_length=200)
    scope: TransferScope | None = None


AccountTransferRequest = TransferStartRequest | TransferActionRequest


class ActionContext(ContractModel):
    accountKey: str | None = None
    threadId: str | None = None
    epoch: int | None = None


class SafetyActionRequest(ContractModel):
    safety: SafetyAction
    turnId: str = Field(min_length=1, max_length=200)


class NativeActionRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    action: NativeAction | SafetyActionRequest
    request_id: str | None = Field(default=None, min_length=1, max_length=200)
    context: ActionContext | None = None


class NativeCommandRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    action: NativeCommandAction
    text: str | None = Field(default=None, max_length=16000)


class ImportRequest(ContractModel):
    id: str | None = Field(default=None, max_length=200)
    threadId: str = Field(min_length=1, max_length=300)
    account_key: str = Field(default="default", min_length=1, max_length=200)
    prompt: str | None = Field(default=None, max_length=32000)
    cwd: str | None = None
    model: str | None = Field(default=None, min_length=1, max_length=300)

    @field_validator("threadId")
    @classmethod
    def selected_thread_not_blank(cls, value: str) -> str:
        if not value.strip():
            raise ValueError("Select a Codex thread")
        return value

    @field_validator("model")
    @classmethod
    def model_not_blank(cls, value: str | None) -> str | None:
        if value is not None and not value.strip():
            raise ValueError("Selected model must not be empty")
        return value


class ImportListQuery(ContractModel):
    cursor: str | None = None
    account_key: str = "default"


class CapabilitiesQuery(ContractModel):
    agent: str | None = None


class SkillsQuery(ContractModel):
    agent: str | None = None


class StopRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    descendants: bool = True


class IdRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)


class CapacityRetryRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    retry_id: str = Field(min_length=1, max_length=200)
    action: RetryAction


class UsageResumeRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    resume_id: str = Field(min_length=1, max_length=200)
    enabled: bool


class UsageResumeResponse(UsageResumeDto, ResponseModel):
    """Usage resume endpoint DTO using the canonical sync receipt fields."""

    id: str
    status: UsageResumeStatus
    accountKey: str
    threadId: str
    epoch: int
    turnId: str
    cause: UsageResumeCause
    failedAt: float


class RenameRequest(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    name: str | None = None
    request_id: str | None = Field(default=None, min_length=1, max_length=200)

    @field_validator("name")
    @classmethod
    def name_not_blank(cls, value: str | None) -> str | None:
        if value is not None and not 1 <= len(value.strip()) <= 80:
            raise ValueError("A name must have 1 to 80 characters")
        return value


    @model_validator(mode="after")
    def smart_rename_identity(self) -> RenameRequest:
        if self.name is None and not self.request_id:
            raise ValueError("Supply a request ID to generate a name")
        return self


class SimpleIdResponse(ResponseModel):
    id: str


class DeletedResponse(ResponseModel):
    deleted: list[str]


class RetryResponse(CapacityRetryDto, ResponseModel):
    """Capacity retry endpoint DTO using the canonical sync receipt fields."""

    id: str
    status: CapacityRetryStatus


class RecoveryResponse(ResponseModel):
    status: RecoveryStatus
    outcome: RecoveryOutcome | None = None
    turnId: str | None = None
    attemptId: str | None = None
    queuedInputPreserved: bool | None = None
    requests: list[str] | None = None
    error: str | None = None
    checked: bool | None = None


class ContextRepairResponse(ResponseModel):
    id: str
    repair: JsonValue | None = None


class NativeActionReceipt(ResponseModel):
    requestId: str
    agentId: str
    action: NativeAction
    accountKey: str
    threadId: str
    epoch: int
    attemptId: str
    acceptedAt: float
    status: NativeActionReceiptStatus


class NativeActionOutcome(ResponseModel):
    status: NativeActionOutcomeStatus
    error: str | None = None


class NativeActionResponse(ResponseModel):
    id: str | None = None
    receipt: NativeActionReceipt | None = None
    outcome: NativeActionOutcome | None = None
    replayed: bool | None = None
    result: JsonValue | None = None
    stage: SafetyActionStage | None = None
    model: str | None = None
    turnId: str | None = None
    created: float | None = None
    updated: float | None = None
    epoch: int | None = None
    accountKey: str | None = None
    error: str | None = None
    newThreadId: str | None = None
    acceptedTurnId: str | None = None
    requestId: str | None = None
    rpcMethod: str | None = None


class TransferResponse(ResponseModel):
    id: str
    leadId: str
    targetAccountKey: str
    status: TransferStatus
    created: float
    updated: float | None = None
    scope: TransferScope
    targetProvider: str | None = None
    finishHistory: bool | None = None
    members: dict[str, TransferMemberResponse]
    requests: dict[str, TransferRequestReceipt] | None = None
    total: int | None = None
    completed: int | None = None
    moved: int | None = None
    nativeHistoryPending: int | None = None
    movingNow: int | None = None
    canFinishHistory: bool | None = None
    interrupted: list[TransferMemberNotice] | None = None
    leftOnSource: list[TransferMemberNotice] | None = None
    blocked: list[TransferMemberNotice] | None = None
    waitingCount: int | None = None
    waiting: str | None = None
    needsAttention: bool | None = None
    canRetry: bool | None = None


class RenameResponse(ResponseModel):
    id: str
    name: str | None = None
    request_id: str | None = None
    status: Literal["pending", "applied", "failed"] | None = None
    error: str | None = None


class ImportListResponse(ResponseModel):
    data: list[JsonValue] = Field(default_factory=list)
    nextCursor: str | None = None


class CapabilitiesResponse(ResponseModel):
    agent: str
    at: float
    # Provider tool and MCP catalogs are provider-defined JSON payloads.
    managed: list[JsonValue]
    observed: list[str]
    skills: list[JsonValue]
    servers: list[JsonValue]
    errors: list[str]
    model: str
    effort: str | None = None
    role: AgentRole
    nativeInventory: str
    observedNative: list[str]
    mcp: list[JsonValue]
    skillsCursor: str | None = None
    serversCursor: str | None = None


class SkillsResponse(ResponseModel):
    skills: list["SkillResponse"]
    errors: list[str]


class SkillResponse(ResponseModel):
    name: str
    path: str
    description: str


class NativeCommandResponse(ResponseModel):
    id: str | None = None
    status: NativeCommandEventStatus | None = None
    terminated: bool | None = None
    processId: str | None = None
    error: str | None = None
    waitingFor: JsonValue | None = None
    receipt: JsonValue | None = None
