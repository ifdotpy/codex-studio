"""Typed federation API request and response contracts."""

from __future__ import annotations

from typing import Annotated, Literal, TypeAlias

from pydantic import Field

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel

PROTOCOL_VERSION = 1
MAX_BATCH = 50


class FederationAction(ContractStrEnum):
    STATUS = "status"
    SET_ENABLED = "set_enabled"
    CREATE_INVITE = "create_invite"
    ACCEPT_PEER = "accept_peer"
    APPROVE_PEER = "approve_peer"
    REVOKE_PEER = "revoke_peer"
    CREATE_ROOM = "create_room"
    APPROVE_ROOM = "approve_room"
    ROOM_OPTIONS = "room_options"


class PeerStatus(ContractStrEnum):
    PENDING = "pending"
    APPROVED = "approved"
    REVOKED = "revoked"


class InviteStatus(ContractStrEnum):
    OPEN = "open"
    USED = "used"


class RoomStatus(ContractStrEnum):
    PENDING = "pending"
    APPROVED = "approved"
    REVOKED = "revoked"


class WhoisStatus(ContractStrEnum):
    PENDING = "pending"
    VERIFIED = "verified"
    MISSING = "missing"
    UNAVAILABLE = "unavailable"


class TailscaleStatus(ContractStrEnum):
    VERIFIED = "verified"
    UNAVAILABLE = "unavailable"


class MessageKind(ContractStrEnum):
    CHAT_MESSAGE = "chat-message"
    ROOM_INVITE = "room-invite"
    ROOM_ACCEPT = "room-accept"


class ParticipantRole(ContractStrEnum):
    LEAD = "lead"
    AGENT = "agent"


class FederationParticipant(ContractModel):
    id: str = Field(max_length=128)
    role: ParticipantRole
    name: str | None = Field(default=None, max_length=80)
    status: str | None = Field(
        default=None,
        max_length=40,
        description="Extensible agent runtime status copied into approved room membership.",
    )


class FederationPeer(ContractModel):
    stateId: str
    label: str
    origin: str
    publicKey: str
    status: PeerStatus
    localApproved: bool
    remoteApproved: bool
    whoisStatus: WhoisStatus | None
    whoisUser: str | None
    created: float | None
    updated: float | None
    lastError: str | None


class FederationInvite(ContractModel):
    id: str
    created: float
    expires: int
    status: InviteStatus
    expectedUser: str | None = None


class FederationRoom(ContractModel):
    id: str
    peerId: str
    peerLabel: str
    name: str
    status: RoomStatus
    localMembers: list[str]
    remoteMembers: list[FederationParticipant]
    shareNames: bool
    shareStatus: bool
    created: float


class FederationIdentity(ContractModel):
    stateId: str
    label: str
    fingerprint: str


class FederationSnapshot(ResponseModel):
    version: int
    enabled: bool
    identity: FederationIdentity | None
    peers: list[FederationPeer]
    invites: list[FederationInvite]
    rooms: list[FederationRoom]
    queued: int


class FederationInvitation(ContractModel):
    protocol: Literal[1]
    inviteId: str
    token: str
    stateId: str
    label: str
    leadName: str | None = None
    origin: str
    publicKey: str
    tailscaleUser: str | None = None
    tailscaleStatus: TailscaleStatus | None = None
    expires: int


class CreateInviteResponse(ResponseModel):
    invitation: FederationInvitation
    expires: int
    warning: str


class StatusActionRequest(ContractModel):
    action: Literal[FederationAction.STATUS]


class SetEnabledRequest(ContractModel):
    action: Literal[FederationAction.SET_ENABLED]
    enabled: bool


class CreateInviteRequest(ContractModel):
    action: Literal[FederationAction.CREATE_INVITE]
    label: str | None = Field(default=None, min_length=1)
    expected_user: str | None = Field(default=None, max_length=254)


class AcceptPeerRequest(ContractModel):
    action: Literal[FederationAction.ACCEPT_PEER]
    invitation: FederationInvitation


class ApprovePeerRequest(ContractModel):
    action: Literal[FederationAction.APPROVE_PEER]
    state_id: str
    accept_missing_whois: bool = False


class RevokePeerRequest(ContractModel):
    action: Literal[FederationAction.REVOKE_PEER]
    state_id: str


class CreateRoomRequest(ContractModel):
    action: Literal[FederationAction.CREATE_ROOM]
    peer_id: str
    local_members: list[str] = Field(min_length=1, max_length=50)
    share_names: bool = False
    share_status: bool = False


class ApproveRoomRequest(ContractModel):
    action: Literal[FederationAction.APPROVE_ROOM]
    room_id: str
    local_members: list[str] = Field(min_length=1, max_length=50)
    share_names: bool = False
    share_status: bool = False


class RoomDisclosure(ContractModel):
    shareNames: bool | None = None
    shareStatus: bool | None = None


class RoomOptionsRequest(ContractModel):
    action: Literal[FederationAction.ROOM_OPTIONS]
    room_id: str
    disclosure: RoomDisclosure


ManagementRequest: TypeAlias = Annotated[
    StatusActionRequest
    | SetEnabledRequest
    | CreateInviteRequest
    | AcceptPeerRequest
    | ApprovePeerRequest
    | RevokePeerRequest
    | CreateRoomRequest
    | ApproveRoomRequest
    | RoomOptionsRequest,
    Field(discriminator="action"),
]


class PairRequest(ContractModel):
    protocol: Literal[1]
    inviteId: str
    token: str
    stateId: str
    label: str
    leadName: str | None = None
    origin: str
    publicKey: str
    tailscaleUser: str | None = None
    tailscaleStatus: str | None = None


class StatusRequest(ContractModel):
    protocol: Literal[1]


class SignedEnvelope(ContractModel):
    protocol: Literal[1]
    senderServer: str
    id: str = Field(max_length=128)
    kind: MessageKind
    room: str = Field(max_length=128)
    payload: JsonValue = Field(
        description="Kind-specific extensible payload covered by the envelope signature; the federation service validates it before storing.",
    )
    created: int | float
    timestamp: int
    nonce: str = Field(min_length=16, max_length=128)
    signature: str


class MessageRequest(ContractModel):
    protocol: Literal[1]
    envelope: SignedEnvelope


class FederationAck(ContractModel):
    id: str
    hash: str


class PullRequest(ContractModel):
    protocol: Literal[1]
    acks: list[FederationAck] = Field(default_factory=list, max_length=MAX_BATCH)


class SignedPairResult(ContractModel):
    protocol: Literal[1]
    accepted: Literal[True]
    remoteApproved: bool
    whoisStatus: WhoisStatus
    stateId: str


class SignedStatusResult(ContractModel):
    protocol: Literal[1]
    approved: bool
    revoked: bool


class MessageReceipt(ContractModel):
    protocol: Literal[1]
    messageId: str
    accepted: Literal[True]
    room: str
    sequence: int | None


class FederationAckResult(ContractModel):
    id: str
    hash: str


class PullResult(ContractModel):
    protocol: Literal[1]
    messages: list[SignedEnvelope]
    acks: list[FederationAckResult]
    acknowledged: list[FederationAckResult]


class SignedPairResponse(ResponseModel):
    stateId: str
    timestamp: int
    nonce: str
    result: SignedPairResult
    signature: str


class SignedStatusResponse(ResponseModel):
    stateId: str
    timestamp: int
    nonce: str
    result: SignedStatusResult
    signature: str


class SignedMessageResponse(ResponseModel):
    stateId: str
    timestamp: int
    nonce: str
    result: MessageReceipt
    signature: str


class SignedPullResponse(ResponseModel):
    stateId: str
    timestamp: int
    nonce: str
    result: PullResult
    signature: str
