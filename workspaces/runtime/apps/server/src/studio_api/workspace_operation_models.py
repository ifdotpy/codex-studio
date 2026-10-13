"""Narrow, open storage projections for workspace-operation decisions."""
from __future__ import annotations

from typing import Annotated, Literal

from pydantic import ConfigDict, Field, StrictBool, StrictFloat, StrictInt, StrictStr

from studio_api.models import ContractModel, JsonValue, MAX_SAFE_INTEGER


class WorkspaceOpenRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)
    id: StrictStr


class WorkspaceCheckpointAgentRecord(WorkspaceOpenRecord):
    epoch: Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
    cwd: StrictStr
    accountKey: StrictStr = "default"
    threadId: StrictStr | None = None
    lastCompletedTurn: StrictStr | None = None
    turnId: StrictStr | None = None
    inFlight: StrictBool | None = None
    autoWake: StrictBool | None = None
    worktreeReady: StrictBool | None = None
    imageWorkspace: StrictBool | None = None
    workspaceOperation: StrictStr | None = None
    deletedAt: StrictFloat | StrictInt | None = None
    agentArchive: dict[str, JsonValue] | None = None
    status: StrictStr
    startAttempt: dict[str, JsonValue] | None = None


class WorkspaceCheckpointHistoryRecord(WorkspaceOpenRecord):
    agent: StrictStr
    created: StrictFloat | StrictInt = 0
    historyParent: StrictStr | None = None
    items: list[StrictStr] = Field(default_factory=list)
    historyDelta: list[StrictStr] = Field(default_factory=list)


class PersistedWorkspaceOperationRecord(WorkspaceOpenRecord):
    kind: Literal["capture", "checkpoint", "restore", "branch", "provision"]
    agent: StrictStr
    phase: Literal["capture_pending", "capture_running", "provider_pending", "provider_ready",
                   "local_mutation", "recovery_required", "completed", "failed"]
    source: dict[str, JsonValue] | None = None
    signature: StrictStr | None = None
    provider: dict[str, JsonValue] | None = None
    result: JsonValue = None
    restartHold: StrictBool | None = None
