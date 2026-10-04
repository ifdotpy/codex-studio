"""Typed HTTP contracts for terminals, monitors, attachments, and file reads."""
from __future__ import annotations

from pydantic import Field, field_validator

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel


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
    id: str
    agent: str
    title: str
    cwd: str
    status: TerminalState
    created: float
    updated: float | None = None
    exitCode: int | None = None
    error: str | None = None


class TerminalList(ResponseModel):
    items: list[TerminalRecord]


class TerminalOutput(ResponseModel):
    text: str
    offset: int
    truncated: bool
    status: TerminalState
    exitCode: int | None = None
    error: str | None = None
    availableOffset: int | None = None
    historyStart: int | None = None
    hasMore: bool | None = None


class TerminalInputResult(ResponseModel):
    ok: bool
    delivery: TerminalInputDelivery
    error: str | None = None


class MonitorActionResult(ResponseModel):
    id: str
    status: MonitorState | None = None
    cancelRequested: bool | None = None
    error: str | None = None


class TerminalCreate(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    agent: str = Field(min_length=1, max_length=200)
    cols: int = Field(default=100, ge=2, le=1000)
    rows: int = Field(default=28, ge=1, le=500)


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
    cols: int = Field(default=100, ge=2, le=1000)
    rows: int = Field(default=28, ge=1, le=500)


class TerminalRename(ContractModel):
    id: str = Field(min_length=1, max_length=200)
    title: str = Field(min_length=1, max_length=120)


class TerminalClose(ContractModel):
    id: str = Field(min_length=1, max_length=200)


class MonitorId(ContractModel):
    id: str = Field(min_length=1, max_length=200)


class MonitorInput(MonitorId):
    text: str = Field(default="", max_length=32000)
    rows: int = Field(default=24, ge=1, le=1000)
    cols: int = Field(default=80, ge=1, le=1000)
    closeStdin: bool = False


class AssetUpload(ContractModel):
    id: str | None = Field(default=None, min_length=1, max_length=200)
    agent: str = Field(min_length=1, max_length=200)
    name: str = Field(min_length=1, max_length=255)
    base64: str | None = None
    data: str | None = None


class AssetRecord(ResponseModel):
    id: str
    agent: str
    name: str
    mime: str
    image: bool
    size: int
    hash: str
    created: float


class FileInfo(ResponseModel):
    path: str
    name: str
    mime: str
    size: int


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
    offset: str = "0"
    history: list[str] = Field(default_factory=list)
    limit: str = "65536"


class MonitorLogQuery(ContractModel):
    id: str | None = None
