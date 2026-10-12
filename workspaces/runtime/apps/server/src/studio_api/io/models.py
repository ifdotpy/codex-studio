"""Typed HTTP contracts for terminals, monitors, attachments, and file reads."""
from __future__ import annotations

from typing import Literal

from pydantic import ConfigDict, Field, field_validator

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel, SafeQueryInt, SafeInt, SignedSafeInt


class TerminalState(ContractStrEnum):
    RUNNING = "running"
    EXITED = "exited"
    CLOSED = "closed"


class MonitorState(ContractStrEnum):
    STARTING = "starting"
    APPROVAL = "approval"
    RUNNING = "running"
    COMPLETED = "completed"
    FAILED = "failed"
    CANCELLED = "cancelled"
    LOST = "lost"


class TerminalInputDelivery(ContractStrEnum):
    SENT = "sent"
    UNCERTAIN = "uncertain"


class TerminalRecord(ResponseModel):
    """Known fields; terminal actions return the persisted user_terminal record."""

    id: str
    agent: str
    title: str
    cwd: str
    status: TerminalState
    created: float
    updated: float | None = None
    exitCode: SignedSafeInt | None = None
    error: str | None = None
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class TerminalList(ResponseModel):
    items: list[TerminalRecord]


class TerminalOutput(ResponseModel):
    text: str
    offset: SafeInt
    truncated: bool
    status: TerminalState
    exitCode: SignedSafeInt | None = None
    error: str | None = None
    availableOffset: SafeInt | None = None
    historyStart: SafeInt | None = None
    hasMore: bool | None = None


class TerminalInputResult(ResponseModel):
    ok: bool
    delivery: TerminalInputDelivery
    error: str | None = None


class MonitorActionResult(ResponseModel):
    id: str
    status: MonitorState
    cancelRequested: bool | None = None
    error: str | None = None


class TerminalCreate(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    agent: str = Field(min_length=1, max_length=200)
    cols: SafeInt = Field(default=100, ge=2, le=1000)
    rows: SafeInt = Field(default=28, ge=1, le=500)


class TerminalInput(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    text: str = Field(max_length=65536)
    request_id: str = Field(min_length=1, max_length=200)

    @field_validator("text")
    @classmethod
    def validate_utf8_size(cls, value: str) -> str:
        if len(value.encode("utf-8")) > 65536:
            raise ValueError("Terminal input is limited to 64 KiB")
        return value


class TerminalResize(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    cols: SafeInt = Field(default=100, ge=2, le=1000)
    rows: SafeInt = Field(default=28, ge=1, le=500)


class TerminalRename(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    title: str = Field(min_length=1, max_length=120)


class TerminalClose(ContractModel):
    id: str = Field(min_length=1, max_length=200)


class MonitorId(ContractModel):
    id: str = Field(min_length=1, max_length=200)


class MonitorInput(MonitorId):
    text: str = Field(default="", max_length=32000)
    rows: SafeInt = Field(default=24, ge=1, le=1000)
    cols: SafeInt = Field(default=80, ge=1, le=1000)
    closeStdin: bool = False


class AssetUpload(ContractModel):
    id: str | None = Field(default=None, min_length=1, max_length=200)
    agent: str = Field(min_length=1, max_length=200)
    name: str = Field(min_length=1, max_length=255)
    base64: str | None = None
    data: str | None = None


class AssetRecord(ResponseModel):
    """Known fields; asset_view removes path and passes every other stored field."""

    id: str
    agent: str
    name: str
    mime: str
    image: bool
    size: SafeInt
    hash: str
    created: float
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class FileInfo(ResponseModel):
    path: str
    name: str
    mime: str
    size: SafeInt


class FileContent(ResponseModel):
    name: str
    mime: str
    base64: str


class FileQuery(ContractModel):
    agent: str | None = None
    path: str | None = None
    asset: str | None = None


class TerminalOutputQuery(ContractModel):
    id: str | None = None
    offset: SafeQueryInt = 0
    history: Literal["1"] | None = None
    limit: SafeQueryInt = Field(default=65536, ge=1)


class MonitorLogQuery(ContractModel):
    id: str | None = None
