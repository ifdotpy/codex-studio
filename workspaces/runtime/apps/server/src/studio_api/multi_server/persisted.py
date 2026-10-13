"""Narrow, open storage contracts for owner-paired access and delivery."""
from typing import Literal

from pydantic import ConfigDict, Field

from studio_api.models import ContractModel, JsonValue


class AccessRecord(ContractModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class PersistedAccessClient(AccessRecord):
    id: str = Field(min_length=1, max_length=128)
    clientId: str = Field(min_length=1, max_length=128)
    kind: Literal["ui", "server"]
    label: str = Field(min_length=1, max_length=80)
    created: float = Field(ge=0)
    publicKey: str = Field(max_length=4096)
    status: Literal["paired", "revoked", "discovered", "unreachable"]
    tailscaleUser: str | None = None
    origin: str | None = None
    pairingRequestId: str | None = None
    pairingHash: str | None = None
    pairingReceipt: dict[str, JsonValue] | None = None
    autoPairingRequestId: str | None = None
    autoPairingHash: str | None = None
    autoPairingReceipt: dict[str, JsonValue] | None = None


class PersistedAccessInvite(AccessRecord):
    inviteId: str = Field(min_length=1, max_length=128)
    status: Literal["open", "used"]
    expires: float = Field(ge=0)
    tokenHash: str = Field(pattern=r"^[0-9a-f]{64}$")
    tailscaleUser: str = Field(min_length=1, max_length=254)


class PersistedServerDelivery(AccessRecord):
    id: str = Field(min_length=1, max_length=200)
    state: Literal["queued", "running", "waiting", "complete"]
    result: str | None = None
    signature: str | None = None
    server: str | None = None
    body: str | None = None
    fingerprint: str | None = None
    created_at: float = Field(ge=0, default=0)
    completed_at: float = Field(ge=0, default=0)
    attempts: int = Field(ge=0, default=0)


class PersistedAccessReceipt(AccessRecord):
    created: float | None = Field(ge=0, default=None)
    completed: float | None = Field(ge=0, default=None)
    status: int | None = Field(ge=100, le=599, default=None)
    body: str | None = Field(max_length=1048576, default=None)
    headers: list[list[str]] | None = None
