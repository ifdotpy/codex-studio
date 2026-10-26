"""Open storage projections consumed by federation messaging in Rust."""
from typing import Annotated, Literal

from pydantic import ConfigDict, Field, StrictBool, StrictInt, StrictStr

from codex_federation_limits import MAX_MESSAGE_CODEPOINTS, MAX_ROOM_MEMBERS
from studio_api.models import ContractModel, JsonValue, MAX_SAFE_INTEGER

Counter = Annotated[StrictInt, Field(ge=0, le=MAX_SAFE_INTEGER)]
Identifier = Annotated[StrictStr, Field(max_length=128)]


class FederationStorageRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class PersistedFederationSettings(FederationStorageRecord):
    enabled: StrictBool


class PersistedFederationPeer(FederationStorageRecord):
    stateId: Identifier
    label: StrictStr
    origin: StrictStr
    publicKey: StrictStr
    status: Literal["approved", "pending", "revoked"]
    localApproved: StrictBool
    remoteApproved: StrictBool
    whoisUser: StrictStr | None = None


class PersistedFederationParticipant(ContractModel):
    id: Identifier
    role: Literal["lead", "agent"]
    name: Annotated[StrictStr, Field(max_length=80)] | None = None
    status: Annotated[StrictStr, Field(max_length=40)] | None = None


class PersistedFederationRoom(FederationStorageRecord):
    id: Identifier
    kind: Literal["federated"]
    peerId: Identifier
    peerLabel: StrictStr
    name: StrictStr
    status: Literal["pending", "approved", "revoked"]
    localMembers: list[Identifier] = Field(max_length=MAX_ROOM_MEMBERS)
    remoteMembers: list[PersistedFederationParticipant] = Field(max_length=MAX_ROOM_MEMBERS)
    localParticipants: list[PersistedFederationParticipant] = Field(default_factory=list, max_length=MAX_ROOM_MEMBERS)
    localApproved: StrictBool
    remoteApproved: StrictBool
    shareNames: StrictBool
    shareStatus: StrictBool
    sequence: Counter = 0
    remoteSequence: Counter = 0
    created: float
    updated: float


class PersistedFederationOutbox(FederationStorageRecord):
    id: Identifier
    kind: Literal["chat-message", "room-invite", "room-accept"]
    room: Identifier
    payload: dict[str, JsonValue]
    created: float
    status: Literal["queued", "delivered"]
    attempts: Counter
    nextAt: float
    error: StrictStr | None = None
    hash: StrictStr | None = None
    receipt: dict[str, JsonValue] | None = None


class PersistedFederationInbox(FederationStorageRecord):
    hash: StrictStr
    receipt: dict[str, JsonValue]
    ack: Literal["pending", "sent", "confirmed"]
    created: float
    kind: Literal["chat-message", "room-invite", "room-accept"] | None = None
    room: Identifier | None = None


FEDERATION_SCHEMA_MODELS = {
    "persisted-federation-settings.schema.json": PersistedFederationSettings,
    "persisted-federation-peer.schema.json": PersistedFederationPeer,
    "persisted-federation-room.schema.json": PersistedFederationRoom,
    "persisted-federation-outbox.schema.json": PersistedFederationOutbox,
    "persisted-federation-inbox.schema.json": PersistedFederationInbox,
}


class FederationChatPayload(FederationStorageRecord):
    sender: Identifier
    senderName: StrictStr | None = None
    text: StrictStr = Field(min_length=1, max_length=MAX_MESSAGE_CODEPOINTS)
    created: float = Field(gt=0)
    sequence: StrictInt = Field(ge=1, le=MAX_SAFE_INTEGER)


class FederationRoomPayload(ContractModel):
    room: Identifier
    participants: list[PersistedFederationParticipant] = Field(min_length=1, max_length=MAX_ROOM_MEMBERS)
    shareNames: StrictBool
    shareStatus: StrictBool


FEDERATION_SCHEMA_MODELS.update({
    "federation-chat-payload.schema.json": FederationChatPayload,
    "federation-room-payload.schema.json": FederationRoomPayload,
})
