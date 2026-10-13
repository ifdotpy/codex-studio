"""Contracts for JSON files and workspace usage data persisted by the server."""
from __future__ import annotations

from typing import Annotated, Any, Literal, cast

from pydantic import AfterValidator, ConfigDict, Field, StrictBool, StrictFloat, StrictInt, StrictStr
from pydantic_core import PydanticCustomError

from studio_api.accounts.models import UsageLimitsResponse
from studio_api.models import MAX_SAFE_INTEGER, ContractModel, JsonValue, SafeInt

from studio_api.multi_server.persisted import (
    PersistedAccessClient,
    PersistedAccessInvite,
    PersistedAccessReceipt,
    PersistedServerDelivery,
)


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


class _OpenPersistedRecord(ContractModel):
    """A storage projection that validates only fields used by Rust readers."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr


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


class PersistedComplaintRecord(_OpenPersistedRecord):
    leadId: StrictStr | None = None
    author: StrictStr | None = None
    text: StrictStr | None = None
    status: StrictStr | None = None


class PersistedCheckpointRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    created: StrictFloat | StrictInt | None = None


class PersistedMonitorRecord(_OpenPersistedRecord):
    agent: StrictStr | None = None
    ruleId: StrictStr | None = None
    status: StrictStr | None = None
    lastExitCode: StrictInt | None = None


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


class PersistedRuleRecord(_OpenPersistedRecord):
    kind: StrictStr | None = None
    agent: StrictStr | None = None
    restartCheck: dict[StrictStr, JsonValue] | None = None
    alerted: StrictBool | None = None
    lowSince: StrictFloat | StrictInt | None = None


class PersistedTranscriptInput(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    kind: StrictStr | None = None
    text: StrictStr | None = None
    truncated: StrictBool | None = None


class PersistedTranscriptRecord(_OpenPersistedRecord):
    """Legacy parent text is hydrated from fulltext before response validation."""

    text: StrictStr | None = None
    inputs: list[PersistedTranscriptInput] | None = None


class RuntimeEventMetadataRecord(ContractModel):
    """Optional event metadata row, keyed by runtime_events.id, not JSON id."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

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
    "account-registry.schema.json": AccountRegistryRecord,
    "persisted-access-client.schema.json": PersistedAccessClient,
    "persisted-access-invite.schema.json": PersistedAccessInvite,
    "persisted-access-receipt.schema.json": PersistedAccessReceipt,
    "persisted-server-delivery.schema.json": PersistedServerDelivery,
    "persisted-agent-record.schema.json": PersistedAgentRecord,
    "persisted-task-record.schema.json": PersistedTaskRecord,
    "persisted-plan-record.schema.json": PersistedPlanRecord,
    "persisted-annotation-record.schema.json": PersistedAnnotationRecord,
    "persisted-request-record.schema.json": PersistedRequestRecord,
    "persisted-complaint-record.schema.json": PersistedComplaintRecord,
    "persisted-checkpoint-record.schema.json": PersistedCheckpointRecord,
    "persisted-monitor-record.schema.json": PersistedMonitorRecord,
    "persisted-terminal-record.schema.json": PersistedTerminalRecord,
    "persisted-usage-resume-record.schema.json": PersistedUsageResumeRecord,
    "persisted-work-item-record.schema.json": PersistedWorkItemRecord,
    "persisted-rule-record.schema.json": PersistedRuleRecord,
    "persisted-transcript-record.schema.json": PersistedTranscriptRecord,
    "persisted-graph-agent-record.schema.json": PersistedGraphAgentRecord,
    "persisted-profile-record.schema.json": PersistedProfileRecord,
    "runtime-transcript-asset.schema.json": PersistedTranscriptAsset,
    "runtime-event-metadata.schema.json": RuntimeEventMetadataRecord,
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
