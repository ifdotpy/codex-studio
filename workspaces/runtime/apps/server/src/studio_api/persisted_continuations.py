"""Narrow storage projections consumed by continuation decisions in Rust."""
from __future__ import annotations

from typing import Annotated, Literal
from pydantic import ConfigDict, Field, StrictBool, StrictInt, StrictStr
from studio_api.models import ContractModel, JsonValue
from studio_api.accounts.models import UsageLimitsResponse

Text = Annotated[StrictStr, Field(max_length=1048576)]
Identity = Annotated[StrictStr, Field(min_length=1, max_length=4096)]
Count = Annotated[StrictInt, Field(ge=0, le=9007199254740991)]
Stamp = Annotated[float, Field(ge=0, le=9007199254740991)]


class OpenRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class ContinuationAttempt(OpenRecord):
    id: Identity
    epoch: Count
    submitted: StrictBool
    events: list[Identity] = Field(default_factory=list, max_length=4096)
    action: StrictStr | None = None
    turnId: Identity | None = None
    observedTurnId: Identity | None = None
    capacityRetryId: Identity | None = None
    activeAtReservation: StrictBool = False


class UsageContinuation(OpenRecord):
    id: Identity
    accountKey: Identity
    threadId: Identity
    epoch: Count
    turnId: Identity
    status: Literal["scheduled", "started", "cancelled", "unknown"]
    cause: Literal["usage_limit", "rate_limit", "auth"] = "usage_limit"
    failedAt: Stamp
    dueAt: Stamp | None = None
    resetAt: Stamp | None = None
    plannedAt: Stamp | None = None
    proofAt: Stamp | None = None
    authAttempt: Count = 0
    authRefreshMarker: Text | None = None
    taskClaims: list[Identity] = Field(default_factory=list, max_length=4096)
    reason: Text | None = None


class CapacityContinuation(OpenRecord):
    id: Identity
    accountKey: Identity
    threadId: Identity
    epoch: Count
    turnId: Identity
    status: Literal["scheduled", "starting", "finished", "cancelled", "unknown", "exhausted", "failed"]
    cause: Literal["serverOverloaded", "internalServerError", "httpConnectionFailed", "responseStreamConnectionFailed", "responseStreamDisconnected", "responseTooManyFailedAttempts"]
    cwd: Text
    settings: dict[str, JsonValue]
    attempt: Count
    maxAttempts: Count
    waits: Count = 0
    dueAt: Stamp | None = None
    claimedAt: Stamp | None = None
    acceptedTurnId: Identity | None = None
    taskClaims: list[Identity] = Field(default_factory=list, max_length=4096)
    reason: Text | None = None


class ContinuationAgent(OpenRecord):
    id: Identity
    rootId: Identity
    epoch: Count
    status: Identity
    accountKey: Identity = "default"
    threadId: Identity | None = None
    turnId: Identity | None = None
    lastCompletedTurn: Identity | None = None
    parentId: Identity | None = None
    autoWake: StrictBool = False
    inFlight: StrictBool = False
    usageResumeEnabled: StrictBool = True
    nativeFailureHold: StrictBool = False
    deletedAt: Stamp | None = None
    cwd: Text = ""
    startAttempt: ContinuationAttempt | None = None
    usageResume: UsageContinuation | None = None
    capacityRetry: CapacityContinuation | None = None
    capacityRetryCount: Count = 0
    excludedAccountKeys: list[Identity] = Field(default_factory=list, max_length=4096)
    provider: Literal["codex", "claude"] | None = None
    authResumeAttempt: Count = 0
    turnEpoch: Count | None = None
    isLead: StrictBool = False
    lastCompletedTurnStatus: StrictStr | None = None


class AuthWaitReceipt(OpenRecord):
    id: Identity
    agent: Identity
    accountKey: Identity
    status: Literal["auth_probe_wait", "completed", "cancelled"]
    cause: Literal["claude_auth_probe"]
    error: Literal["Cannot read Claude Code sign-in status"]
    createdAt: Annotated[float, Field(gt=0, le=9007199254740991)]
    dueAt: Stamp | None
    source: dict[str, JsonValue]
    attempt: ContinuationAttempt
    inputs: list[dict[str, JsonValue]] = Field(max_length=64)
    accountPin: dict[str, JsonValue]
    taskClaims: list[Identity] = Field(max_length=4096)


class ContinuationEvent(OpenRecord):
    id: Identity
    agent: Identity
    epoch: Count
    kind: Identity
    text: Text
    status: Literal["pending", "reserved", "dispatching", "uncertain", "delivered", "stored_only", "cancelled"]
    created: Stamp
    turn_id: Identity | None = None
    error: Text | None = None


class ContinuationMetadata(OpenRecord):
    preserveProgress: StrictBool = False
    earlierProgressUpdates: Count = 0
    assets: list[Identity] = Field(default_factory=list, max_length=64)


class ComplaintResponse(OpenRecord):
    author: Identity


class WakeComplaint(OpenRecord):
    id: Identity
    leadId: Identity
    recipient: StrictStr
    status: StrictStr
    sourceType: StrictStr | None = None
    responses: list[ComplaintResponse] = Field(default_factory=list, max_length=4096)


class WorkResult(OpenRecord):
    id: Identity


class WorkDecision(OpenRecord):
    resultId: Identity


class WakeWork(OpenRecord):
    id: Identity
    rootId: Identity
    results: list[WorkResult] = Field(default_factory=list, max_length=4096)
    decisions: list[WorkDecision] = Field(default_factory=list, max_length=4096)


class ContinuationLimitsObservation(UsageLimitsResponse):
    stale: StrictBool = False


class ContinuationSnapshotRecord(ContractModel):
    limits: ContinuationLimitsObservation | None = None
    agent: ContinuationAgent
    ancestors: list[ContinuationAgent] = Field(default_factory=list, max_length=64)
    claims: list[Identity] = Field(default_factory=list, max_length=4096)
    record: UsageContinuation | CapacityContinuation | AuthWaitReceipt | None = None
    events: list[ContinuationEvent] = Field(default_factory=list, max_length=4096)
    metadata: dict[str, ContinuationMetadata | None] = Field(default_factory=dict)
    complaints: dict[str, WakeComplaint] = Field(default_factory=dict)
    work: dict[str, WakeWork] = Field(default_factory=dict)
    assets: dict[str, dict[str, JsonValue]] = Field(default_factory=dict)
