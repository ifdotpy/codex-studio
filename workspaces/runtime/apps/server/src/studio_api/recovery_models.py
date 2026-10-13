"""Narrow, open persisted projections consumed by Rust recovery decisions."""
from __future__ import annotations

from typing import Annotated
from pydantic import ConfigDict, Field, StrictBool, StrictInt, StrictStr
from studio_api.models import ContractModel, JsonValue


class RecoverySupervisorIdentity(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    stateDir: StrictStr | None = None
    handle: StrictStr | None = None
    generation: Annotated[StrictInt, Field(ge=0, le=9007199254740991)] | None = None


class RecoveryFields(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    id: StrictStr | None = None
    epoch: Annotated[StrictInt, Field(ge=0, le=9007199254740991)] | None = None
    accountKey: StrictStr | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    autoWake: StrictBool | None = None
    submitted: StrictBool | None = None
    activeAtReservation: StrictBool | None = None
    observedTurnId: StrictStr | None = None
    events: Annotated[list[StrictStr], Field(max_length=32)] | None = None
    stage: StrictStr | None = None
    phase: StrictStr | None = None
    executionOutcome: StrictStr | None = None
    outcome: StrictStr | None = None
    automatic: StrictBool | None = None
    source: StrictStr | RecoveryFields | None = None
    startAttempt: RecoveryFields | None = None
    agent: StrictStr | None = None
    newThreadId: StrictStr | None = None
    connectionId: StrictStr | None = None
    supervisorIdentity: RecoverySupervisorIdentity | None = None
    created: Annotated[float, Field(ge=0, allow_inf_nan=False)] | None = None
    error: JsonValue = None


class RecoveryAgentRecord(RecoveryFields):
    # Agents count notifications; only nested attempt records contain event IDs.
    events: Annotated[StrictInt, Field(ge=0)] = 0  # type: ignore[assignment]
    id: Annotated[StrictStr, Field(min_length=1)]
    rootId: StrictStr
    epoch: Annotated[StrictInt, Field(ge=0, le=9007199254740991)]
    status: StrictStr
    provider: StrictStr | None = None
    inFlight: StrictBool | None = None
    activeTools: list[JsonValue] | None = None
    accountHistory: list[dict[str, JsonValue]] | None = None
    nativeFailureHold: StrictBool | None = None
    turnEpoch: Annotated[StrictInt, Field(ge=0, le=9007199254740991)] | None = None
    startAttempt: RecoveryFields | None = None
    disconnectRecovery: RecoveryFields | None = None
    restartRecovery: RecoveryFields | None = None
    connectionRecovery: RecoveryFields | None = None
    contextRepair: RecoveryFields | None = None
    contextRepairWait: RecoveryFields | None = None
    nativeThreadBlock: RecoveryFields | None = None
    nativeTurnError: RecoveryFields | None = None
    startOutcomeHold: RecoveryFields | None = None


class RecoveryTaskRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    id: Annotated[StrictStr, Field(min_length=1)]
    agent: Annotated[StrictStr, Field(min_length=1)]
    itemId: Annotated[StrictStr, Field(min_length=1)]
    turnId: Annotated[StrictStr, Field(min_length=1)]
    type: StrictStr
    kind: StrictStr
    processId: StrictStr | Annotated[StrictInt, Field(ge=-9007199254740991, le=9007199254740991)] | None = None
    status: StrictStr
    created: Annotated[float, Field(ge=0, allow_inf_nan=False)]


class RecoveryToolReceipt(RecoveryFields):
    id: Annotated[StrictStr, Field(min_length=1)]
    agent: StrictStr
    callId: StrictStr
    finished: Annotated[float, Field(ge=0, allow_inf_nan=False)] | None = None
    result: dict[str, JsonValue] | None = None
