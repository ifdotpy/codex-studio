"""Narrow stored Claude receipts; unknown fields survive the Rust cutover."""
from __future__ import annotations

from typing import Annotated, Literal
from pydantic import ConfigDict, Field, StrictBool, StrictInt, StrictStr
from studio_api.models import ContractModel, JsonValue, MAX_SAFE_INTEGER

Identity = Annotated[StrictStr, Field(min_length=1, max_length=1024)]
Epoch = Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
Digest = Annotated[StrictStr, Field(pattern=r"^[a-f0-9]{64}$")]


class OpenRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ClaudeEventSnapshot(OpenRecord):
    id: Identity
    kind: StrictStr
    created: float
    assets: list[JsonValue] = Field(max_length=256)
    textHash: Digest


class ClaudeFrozenInput(OpenRecord):
    input: list[JsonValue] = Field(max_length=1024)
    source: dict[str, JsonValue]
    configuration: dict[str, JsonValue]
    events: list[ClaudeEventSnapshot] = Field(min_length=1, max_length=32)
    transcriptItemId: Identity
    clientUserMessageId: Identity
    textHash: Digest


class PersistedClaudeAttemptRecord(OpenRecord):
    id: Identity
    epoch: Epoch | None = None
    accountKey: Identity | None = None
    threadId: Identity | None = None
    connectionId: Identity | None = None
    nativeOperationId: Identity | None = None
    events: list[Identity] | None = Field(default=None, max_length=32)
    submitted: StrictBool | None = None
    activeAtReservation: StrictBool | None = None
    created: float | None = None
    claudeInputRequest: ClaudeFrozenInput | None = None
    claudeInputRejection: dict[str, JsonValue] | None = None


class ClaudeLoginReceipt(OpenRecord):
    requestId: Identity
    accountKey: Identity
    status: Literal["starting", "pending", "ready", "error", "cancelled"]
    email: StrictStr | None = None
    plan: StrictStr | None = None
    chatsRefreshed: StrictBool | None = None
    codeSubmitted: StrictBool | None = None
    error: StrictStr | None = None


class ClaudeChildIdentity(OpenRecord):
    pid: Annotated[StrictInt, Field(ge=1, le=4294967295)]
    start: Annotated[StrictStr, Field(min_length=1, max_length=128)]


class ClaudeLoginMetadata(OpenRecord):
    child: ClaudeChildIdentity | None = None
    inheritedLease: StrictBool | None = None
    outcomeUnknown: StrictBool | None = None
    operatorCleared: StrictBool | None = None
    flow: Literal["add"] | None = None
    requestHash: Digest | None = None
    emailHint: StrictStr | None = None
    label: StrictStr | None = None
    configDir: StrictStr | None = None
    binaryPath: StrictStr | None = None
    codeSubmitted: StrictBool | None = None
    cleanupDone: StrictBool | None = None


class PersistedClaudeLoginRecord(OpenRecord):
    receipt: ClaudeLoginReceipt
    metadata: ClaudeLoginMetadata


class ClaudeControlTransport(OpenRecord):
    account: Identity
    connection: Identity
    generation: Annotated[StrictInt, Field(ge=1, le=MAX_SAFE_INTEGER)]


class PersistedClaudeControlRecord(OpenRecord):
    id: Identity
    requestId: Identity
    kind: Literal["claude_settings", "claude_rollback"]
    agent: Identity
    turnId: Identity | None = None
    signature: Digest
    source: dict[str, JsonValue]
    phase: Literal["provider_pending", "provider_ready", "recovery_required", "completed", "failed"]
    submitted: StrictBool | None = None
    controlEpoch: Epoch | None = None
    controlTransport: ClaudeControlTransport | None = None
    provider: dict[str, JsonValue] | None = None
    result: JsonValue = None
    removedTurns: list[Identity] | None = Field(default=None, max_length=100000)
    retainedTurn: Identity | None = None
    created: float
    updated: float | None = None
    error: StrictStr | None = None
