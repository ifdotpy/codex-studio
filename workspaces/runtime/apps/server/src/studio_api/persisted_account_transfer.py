"""Narrow storage contracts for transfer decisions; preserve unconsumed fields."""
from typing import Annotated, Literal

from pydantic import ConfigDict, Field, StrictBool, StrictInt, StrictStr
from studio_api.models import ContractModel, JsonValue, MAX_SAFE_INTEGER

Text = Annotated[StrictStr, Field(max_length=65536)]
Identity = Annotated[StrictStr, Field(min_length=1, max_length=200)]
Epoch = Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
Phase = Literal['waiting', 'lazy', 'reading', 'interrupting', 'submitted',
                'lazy_submitted', 'ready', 'blocked', 'unknown', 'completed', 'left']


class OpenTransferRecord(ContractModel):
    model_config = ConfigDict(extra='allow', strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class TransferRequestIdentity(OpenTransferRecord):
    leadId: Identity
    targetAccountKey: Identity
    scope: Literal['team', 'subagents'] = 'team'


class TransferTransportTicket(OpenTransferRecord):
    operation: Identity
    agent: Identity
    epoch: Epoch
    account: Identity
    connection: Identity
    request: Identity


class LazyTransferRecord(OpenTransferRecord):
    id: Identity
    sourceAccountKey: Identity
    sourceThreadId: Identity | None = None
    sourceState: dict[str, JsonValue] = Field(default_factory=dict)


class TransferMemberRecord(OpenTransferRecord):
    phase: Phase
    sourceAccountKey: Identity
    sourceThreadId: Identity | None = None
    lazy: StrictBool = False
    name: Text | None = None
    provider: Text | None = None
    error: Text | None = None
    waiting: Text | None = None
    reason: Text | None = None
    archiveInvalidated: StrictBool = False
    interruptSubmittedAt: float | None = None
    interruptOutcome: Literal['acknowledged', 'unknown', 'rejected'] | None = None
    interruptReason: Text | None = None
    continueAfterTransfer: StrictBool = False
    submittedAt: float | None = None
    result: dict[str, JsonValue] | None = None
    source: dict[str, JsonValue] | None = None
    settings: dict[str, JsonValue] | None = None
    targetSettings: dict[str, JsonValue] | None = None
    nativeMethod: Literal['thread/start', 'thread/fork'] | None = None
    nativeParams: dict[str, JsonValue] | None = None
    transportTicket: TransferTransportTicket | None = None
    targetConnection: Identity | None = None


class PersistedAccountTransferRecord(OpenTransferRecord):
    id: Identity
    leadId: Identity
    targetAccountKey: Identity
    status: Literal['pending', 'completed', 'cancelled']
    scope: Literal['team', 'subagents'] = 'team'
    created: float
    updated: float | None = None
    targetProvider: Text | None = None
    finishHistory: StrictBool = False
    members: Annotated[dict[Identity, TransferMemberRecord], Field(max_length=2050)]
    requests: Annotated[dict[Identity, TransferRequestIdentity], Field(max_length=4096)] = Field(default_factory=dict)


class PersistedTransferAgentRecord(OpenTransferRecord):
    id: Identity
    rootId: Identity
    epoch: Epoch
    status: Text
    accountKey: Identity = 'default'
    provider: Literal['codex', 'claude'] = 'codex'
    projectFolderRevision: Epoch = 0
    workerDefaults: dict[str, JsonValue] | None = None
    usageResume: dict[str, JsonValue] | None = None
    threadId: Identity | None = None
    cwd: Text | None = None
    isLead: StrictBool = False
    inFlight: StrictBool = False
    autoWake: StrictBool = False
    deletedAt: float | None = None
    accountTransferId: Identity | None = None
    accountTransfer: dict[str, JsonValue] | None = None
    lazyAccountTransfer: LazyTransferRecord | None = None
    emptyTransferRecovery: dict[str, JsonValue] | None = None
