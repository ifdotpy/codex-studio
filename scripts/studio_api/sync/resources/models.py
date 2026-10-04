"""Authoritative typed contracts for protocol 3 resource notifications."""

from __future__ import annotations

from typing import Annotated, Literal

from pydantic import Field, RootModel, StrictInt, StrictStr

from studio_api.models import ContractModel

MAX_SAFE_REVISION = 9_007_199_254_740_991
ResourceRevision = Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_REVISION)]


class PanelResource(ContractModel):
    kind: Literal["panel"]
    agentId: StrictStr


class QueueResource(ContractModel):
    kind: Literal["queue"]
    agentId: StrictStr


class ReceiptsResource(ContractModel):
    kind: Literal["receipts"]
    agentId: StrictStr


class TerminalResource(ContractModel):
    kind: Literal["terminal"]
    terminalId: StrictStr


class TerminalsResource(ContractModel):
    kind: Literal["terminals"]


class AccountsResource(ContractModel):
    kind: Literal["accounts"]


class LimitsResource(ContractModel):
    kind: Literal["limits"]
    accountKey: StrictStr


class ModelsResource(ContractModel):
    kind: Literal["models"]


class TasksResource(ContractModel):
    kind: Literal["tasks"]
    agentId: StrictStr


class TaskResource(ContractModel):
    kind: Literal["task"]
    taskId: StrictStr


class WorkspaceResource(ContractModel):
    kind: Literal["workspace"]
    agentId: StrictStr


class VoiceResource(ContractModel):
    kind: Literal["voice"]
    agentId: StrictStr


class SessionCostResource(ContractModel):
    kind: Literal["session-cost"]
    agentId: StrictStr


class CostsResource(ContractModel):
    kind: Literal["costs"]


class DesktopResource(ContractModel):
    kind: Literal["desktop"]


class WorktreeDiskResource(ContractModel):
    kind: Literal["worktree-disk"]
    agentId: StrictStr


class RoomResource(ContractModel):
    kind: Literal["room"]
    roomId: StrictStr


class StateResource(ContractModel):
    kind: Literal["state"]


class DraftsResource(ContractModel):
    kind: Literal["drafts"]


class TranscriptsResource(ContractModel):
    kind: Literal["transcripts"]


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
    | WorktreeDiskResource
    | RoomResource
    | StateResource
    | DraftsResource
    | TranscriptsResource,
    Field(discriminator="kind"),
]


class ResourceRef(RootModel[ResourceRefValue]):
    """Closed discriminated union of all typed UI resource identities."""


class ResourceChangeEvent(ContractModel):
    """Named `resources` SSE payload. A change invalidates the listed refs."""

    protocol: Literal[3]
    workspaceId: StrictStr
    epoch: StrictStr
    revision: ResourceRevision
    reason: Literal["initial", "change", "reconnect", "overflow", "workspace"]
    resources: list[ResourceRef]


class ResourceHeartbeatEvent(ContractModel):
    """Named `heartbeat` SSE payload; it never requests a resource read."""

    protocol: Literal[3]
    workspaceId: StrictStr
    epoch: StrictStr
    revision: ResourceRevision

