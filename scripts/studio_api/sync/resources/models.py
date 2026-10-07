"""Authoritative typed contracts for protocol 3 resource notifications."""

from __future__ import annotations

from typing import Annotated, Literal

from pydantic import Field, RootModel, StrictBool, StrictFloat, StrictInt, StrictStr

from studio_api.models import ContractModel

MAX_SAFE_REVISION = 9_007_199_254_740_991
ResourceRevision = Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_REVISION)]
NonEmptyIdentifier = Annotated[StrictStr, Field(min_length=1)]
NonNegativeFiniteNumber = Annotated[StrictInt, Field(ge=0)] | Annotated[
    StrictFloat, Field(ge=0, allow_inf_nan=False)
]


class PanelResource(ContractModel):
    kind: Literal["panel"]
    agentId: NonEmptyIdentifier


class QueueResource(ContractModel):
    kind: Literal["queue"]
    agentId: NonEmptyIdentifier


class ReceiptsResource(ContractModel):
    kind: Literal["receipts"]
    agentId: NonEmptyIdentifier


class TerminalResource(ContractModel):
    kind: Literal["terminal"]
    terminalId: NonEmptyIdentifier


class TerminalsResource(ContractModel):
    kind: Literal["terminals"]


class AccountsResource(ContractModel):
    kind: Literal["accounts"]


class LimitsResource(ContractModel):
    kind: Literal["limits"]
    accountKey: NonEmptyIdentifier


class ModelsResource(ContractModel):
    kind: Literal["models"]


class TasksResource(ContractModel):
    kind: Literal["tasks"]
    agentId: NonEmptyIdentifier


class TaskResource(ContractModel):
    kind: Literal["task"]
    taskId: NonEmptyIdentifier


class WorkspaceResource(ContractModel):
    kind: Literal["workspace"]
    agentId: NonEmptyIdentifier


class VoiceResource(ContractModel):
    kind: Literal["voice"]
    agentId: NonEmptyIdentifier


class SessionCostResource(ContractModel):
    kind: Literal["session-cost"]
    agentId: NonEmptyIdentifier


class CostsResource(ContractModel):
    kind: Literal["costs"]


class DesktopResource(ContractModel):
    kind: Literal["desktop"]


class RoomResource(ContractModel):
    kind: Literal["room"]
    roomId: NonEmptyIdentifier


class StateResource(ContractModel):
    kind: Literal["state"]


class DraftsResource(ContractModel):
    kind: Literal["drafts"]


class TranscriptsResource(ContractModel):
    """Legacy collection-wide transcript subscription retained for clients."""

    kind: Literal["transcripts"]


class TranscriptResource(ContractModel):
    kind: Literal["transcript"]
    agentId: NonEmptyIdentifier


ResourceRefValue = Annotated[
    PanelResource
    | QueueResource
    | ReceiptsResource
    | TerminalResource
    | TerminalsResource
    | AccountsResource
    | LimitsResource
    | ModelsResource
    | TasksResource
    | TaskResource
    | WorkspaceResource
    | VoiceResource
    | SessionCostResource
    | CostsResource
    | DesktopResource
    | RoomResource
    | StateResource
    | DraftsResource
    | TranscriptsResource
    | TranscriptResource,
    Field(discriminator="kind"),
]


class ResourceRef(RootModel[ResourceRefValue]):
    """Closed discriminated union of all typed UI resource identities."""


class ResourceRevisionEntry(ContractModel):
    """Per-resource revision; StateResource uses the entity sequence."""

    # Omitted on the wire: entries align positionally with event.resources.
    # Optional decode support keeps older or hand-written protocol-3 frames valid.
    resource: ResourceRef | None = None
    revision: ResourceRevision
    entitySequences: list[ResourceRevision] | None = None
    entitySequenceReset: bool | None = None


class ResourceChangeEvent(ContractModel):
    """Named `resources` SSE payload. A change invalidates the listed refs."""

    protocol: Literal[3]
    workspaceId: NonEmptyIdentifier
    epoch: NonEmptyIdentifier
    revision: ResourceRevision
    reason: Literal["initial", "change", "reconnect", "overflow", "workspace"]
    resources: list[ResourceRef]
    resourceVersions: list[ResourceRevisionEntry]


class ResourceHeartbeatEvent(ContractModel):
    """Named `heartbeat` SSE payload; it never requests a resource read."""

    protocol: Literal[3]
    workspaceId: NonEmptyIdentifier
    epoch: NonEmptyIdentifier
    revision: ResourceRevision


class TokenRateValue(ContractModel):
    """One active or completed agent turn's volatile token-rate snapshot."""

    turnId: NonEmptyIdentifier
    active: StrictBool
    estimated: StrictBool
    rate: NonNegativeFiniteNumber
    outputTokens: NonNegativeFiniteNumber


class ResourceTokenRatesEvent(ContractModel):
    """Named `token-rates` SSE payload; values come from workspace_snapshot()."""

    protocol: Literal[3]
    workspaceId: NonEmptyIdentifier
    epoch: NonEmptyIdentifier
    revision: ResourceRevision
    rates: dict[NonEmptyIdentifier, TokenRateValue]
    teams: dict[NonEmptyIdentifier, dict[NonEmptyIdentifier, TokenRateValue]]


class TokenRateSnapshot(ContractModel):
    """Typed payload accepted from the token-rate producer boundary."""

    rates: dict[NonEmptyIdentifier, TokenRateValue]
    teams: dict[NonEmptyIdentifier, dict[NonEmptyIdentifier, TokenRateValue]]
