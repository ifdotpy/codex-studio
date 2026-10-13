"""Narrow persisted contracts for relocation decisions; unknown fields stay intact."""
from typing import Annotated, Literal

from studio_api.models import JsonValue

from pydantic import BaseModel, ConfigDict, Field, RootModel, StrictBool, StrictInt, StrictStr


class OpenRecord(BaseModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


Text = Annotated[StrictStr, Field(max_length=4096)]
Key = Annotated[StrictStr, Field(min_length=1, max_length=4096)]
Counter = Annotated[StrictInt, Field(ge=0, le=9007199254740991)]
Time = Annotated[float, Field(ge=0, le=253402300799)]
Phase = Literal["waiting", "exported", "importing", "prepared", "redirected", "complete", "unknown", "failed", "receiving", "native_submitted", "ready", "active"]


class Location(OpenRecord):
    serverId: Key
    path: Text
    projectId: Key
    requestedPath: Text | None = None
    gitOrigin: Text | None = None


class ProjectAlias(OpenRecord):
    serverId: Key
    projectId: Key
    name: Text


class RelocationProject(OpenRecord):
    id: Key
    path: Text
    name: Text
    homeServerId: Key | None = None
    locations: Annotated[list[Location], Field(max_length=10000)] = []
    locationsRevision: Counter = 0
    projectAliases: Annotated[list[ProjectAlias], Field(max_length=10000)] = []


class LocationRequest(OpenRecord):
    action: Literal["add_location", "remove_location"]
    project: Key
    server: Key
    path: Text | None


class LocationPayload(OpenRecord):
    project: Key
    name: Text
    path: Text | None
    request: LocationRequest
    signature: Annotated[StrictStr, Field(max_length=262144)]
    location: Location | None = None


class LocationEnvelope(OpenRecord):
    requestId: Key
    action: Literal["add_location", "remove_location"]
    payload: LocationPayload


class RelocationReceipt(OpenRecord):
    requestId: Key
    outcome: Literal["applied", "not_applied", "unknown"]
    value: dict[str, JsonValue] | None = None


class LocationOutbox(OpenRecord):
    id: Key
    server: Key
    body: LocationEnvelope
    state: Literal["queued", "complete"]
    result: RelocationReceipt | None


class NativeAgent(OpenRecord):
    id: Key
    epoch: Counter
    rootId: Key
    status: StrictStr
    inFlight: StrictBool = False
    turnId: Text | None = None
    movedFrom: dict[str, JsonValue] | None = None
    executionMove: dict[str, JsonValue] | None = None


class MoveDescriptor(OpenRecord):
    agent: NativeAgent
    acceptedResult: dict[str, JsonValue]


class ToolRequest(OpenRecord):
    id: Key
    agent: Key
    tool: Key
    epoch: Counter
    accountKey: Key | None = None
    connectionId: Key | None = None
    result: dict[str, JsonValue] | None = None
    stage: StrictStr
    outcome: Literal["unknown", "applied", "not_applied"]


class SourceMove(OpenRecord):
    id: Key
    side: Literal["source"]
    phase: Literal["waiting", "exported", "importing", "prepared", "redirected", "complete", "unknown", "failed"]
    agent: Key
    epoch: Counter
    server: Key
    result: dict[str, JsonValue]
    fingerprint: Text
    created: Time
    deadline: Time = 0
    uncertainPhase: Phase | None = None
    recoveryExpiredAt: Time | None = None
    firstTurnCache: dict[str, JsonValue] | None = None


class TargetMove(OpenRecord):
    id: Key
    side: Literal["target"]
    phase: Literal["receiving", "native_submitted", "ready", "active", "unknown", "failed"]
    move: Key
    principal: Key
    descriptor: MoveDescriptor
    created: Time
    deadline: Time = 0
    firstTurnCache: dict[str, JsonValue] | None = None


class TargetStatus(OpenRecord):
    phase: Literal["not_started", "receiving", "native_submitted", "ready", "active", "unknown", "failed"]
    error: Text | None = None
    firstTurnCache: dict[str, JsonValue] | None = None


class MoveRecord(RootModel[Annotated[SourceMove | TargetMove, Field(discriminator="side")]]):
    pass


class RelocationFormats(OpenRecord):
    project: RelocationProject | None = None
    outbox: LocationOutbox | None = None
    move: MoveRecord | None = None
    receipt: RelocationReceipt | None = None
    agent: NativeAgent | None = None
    tool: ToolRequest | None = None
    status: TargetStatus | None = None
