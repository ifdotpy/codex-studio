"""Version 1 device credentials and owner access API contracts."""
from __future__ import annotations

from typing import Annotated, Literal, TypeAlias
from pydantic import Field

from studio_api.models import ContractModel, JsonValue, ResponseModel


class ServerIdentity(ContractModel):
    serverId: str
    label: str
    origin: str | None
    publicKey: str
    tailscaleUser: str | None


class AccessInvitation(ContractModel):
    protocol: Literal[1]
    inviteId: str = Field(min_length=1, max_length=128)
    token: str = Field(min_length=1, max_length=128)
    serverId: str = Field(min_length=1, max_length=128)
    label: str = Field(min_length=1, max_length=80)
    origin: str = Field(max_length=512)
    publicKey: str = Field(max_length=4096)
    tailscaleUser: str = Field(min_length=1, max_length=254)
    expires: int


class InvitationResponse(ResponseModel):
    invitation: AccessInvitation
    expires: int


class AccessClient(ContractModel):
    id: str
    clientId: str
    serverId: str | None
    label: str
    kind: Literal["ui", "server"]
    origin: str | None
    publicKey: str
    tailscaleUser: str
    status: Literal["paired", "revoked", "discovered", "unreachable"]
    created: float
    lastAccess: float | None
    revoked: float | None
    reachability: Literal["reachable", "unreachable", "unknown"] | None = None
    lastSeen: float | None = None
    autoPair: bool | None = None
    alias: str | None = None


class InviteSummary(ContractModel):
    inviteId: str
    expires: int
    created: int
    status: Literal["open", "used"]


class AccessSettings(ContractModel):
    autoPair: bool
    aliases: dict[str, str] = Field(default_factory=dict)


class DiscoveryIdentity(ServerIdentity, ResponseModel):
    protocol: Literal[1]
    autoPair: bool


class AccessSnapshot(ResponseModel):
    protocol: Literal[1]
    identity: ServerIdentity
    clients: list[AccessClient]
    servers: list[AccessClient]
    invites: list[InviteSummary]
    settings: AccessSettings


class AccessAudit(ContractModel):
    sequence: int
    clientId: str
    actorId: str
    action: Literal["create_invite", "pair", "revoke", "accept_invite", "auto_pair", "unrevoke", "settings", "alias"]
    created: float


class AccessAuditResponse(ResponseModel):
    records: list[AccessAudit]


class CreateInvite(ContractModel):
    action: Literal["create_invite"]
    requestId: str = Field(min_length=1, max_length=128)
    label: str | None = Field(default=None, min_length=1, max_length=80)


class RevokeClient(ContractModel):
    action: Literal["revoke"]
    requestId: str = Field(min_length=1, max_length=128)
    clientId: str = Field(min_length=1, max_length=128)


class AcceptInvite(ContractModel):
    action: Literal["accept_invite"]
    requestId: str = Field(min_length=1, max_length=128)
    invitation: AccessInvitation


class DiscoverServers(ContractModel):
    action: Literal["discover"]
    requestId: str = Field(min_length=1, max_length=128)


class UiInvite(ContractModel):
    action: Literal["ui_invite"]
    serverId: str = Field(min_length=1, max_length=128)
    requestId: str = Field(min_length=1, max_length=128)


class SetAccessSettings(ContractModel):
    action: Literal["settings"]
    autoPair: bool
    requestId: str | None = Field(default=None, min_length=1, max_length=128)


class SetServerAlias(ContractModel):
    action: Literal["alias"]
    serverId: str = Field(min_length=1, max_length=128)
    alias: str = Field(min_length=1, max_length=3, pattern=r"^[A-Z]{1,3}$")
    requestId: str = Field(min_length=1, max_length=128)


class UnrevokeServer(ContractModel):
    action: Literal["unrevoke"]
    clientId: str = Field(min_length=1, max_length=128)
    requestId: str = Field(min_length=1, max_length=128)


class AutoPairRequest(ContractModel):
    protocol: Literal[1]
    serverId: str = Field(min_length=1, max_length=128)
    label: str = Field(min_length=1, max_length=80)
    origin: str = Field(max_length=512)
    publicKey: str = Field(max_length=4096)
    requestId: str = Field(min_length=1, max_length=128)


ManagementRequest: TypeAlias = Annotated[CreateInvite | RevokeClient | AcceptInvite | DiscoverServers | UiInvite | SetAccessSettings | UnrevokeServer | SetServerAlias, Field(discriminator="action")]


class DevicePairRequest(ContractModel):
    protocol: Literal[1]
    inviteId: str = Field(min_length=1, max_length=128)
    token: str = Field(min_length=1, max_length=128)
    clientId: str = Field(min_length=1, max_length=128)
    label: str = Field(min_length=1, max_length=80)
    kind: Literal["ui", "server"]
    publicKey: str = Field(max_length=4096)
    requestId: str = Field(min_length=1, max_length=128)
    origin: str | None = Field(default=None, max_length=512)


class DevicePairResponse(ResponseModel):
    protocol: Literal[1]
    serverId: str
    clientId: str
    label: str
    origin: str
    publicKey: str
    tailscaleUser: str
    paired: Literal[True]


class ServerOperationRequest(ContractModel):
    requestId: str = Field(min_length=1, max_length=128)
    action: str = Field(min_length=1, max_length=80)
    payload: dict[str, JsonValue] = Field(description="The cross-server service validates the action-specific payload.")


class ServerOperationResponse(ResponseModel):
    requestId: str
    outcome: str
    value: JsonValue | None = Field(default=None, description="The action-specific result from cross-server orchestration.")
    error: str | None = None
