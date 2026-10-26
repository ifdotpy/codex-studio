"""Typed HTTP contracts for terminals, monitors, attachments, and file reads."""
from __future__ import annotations

from typing import Literal

from pydantic import ConfigDict, Field, field_validator, model_validator

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel, SafeQueryInt, SafeInt, SignedSafeInt, UInt64


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


class ProcessCapabilities(ResponseModel):
    readOutput: bool
    sendInput: bool
    resize: bool
    stop: bool


class ManagedProcessRecord(ResponseModel):
    id: str
    key: str
    generation: str
    agent: str
    accountKey: str
    epoch: SafeInt
    source: Literal["monitor", "codex_task", "claude_task"]
    status: str
    cancelRequested: bool = False
    created: float
    name: str
    command: str | None = None
    cwd: str | None = None
    output: str | None = None
    log: str | None = None
    error: str | None = None
    stdinError: str | None = None
    stdinClosed: bool = False
    stdinCloseRequested: bool = False
    bytes: SafeInt = 0
    outputTruncated: bool = False
    capabilities: ProcessCapabilities


class ProcessList(ResponseModel):
    processes: list[ManagedProcessRecord]
    responseTarget: "ProcessResponseTarget | None" = None


class ProcessListQuery(ContractModel):
    agent: str = Field(min_length=1, max_length=200)


class ProcessIdentity(ContractModel):
    key: str = Field(min_length=1, max_length=2048)
    generation: str = Field(min_length=1, max_length=256)
    agent: str = Field(min_length=1, max_length=200)
    accountKey: str = Field(min_length=1, max_length=200)
    epoch: SafeInt


class ProcessResponseTarget(ContractModel):
    agent: str = Field(min_length=1, max_length=200)
    accountKey: str = Field(min_length=1, max_length=200)
    epoch: SafeInt
    connectionId: str = Field(min_length=1, max_length=200)
    threadId: str = Field(min_length=1, max_length=200)
    turnId: str = Field(min_length=1, max_length=200)


class ProcessControlRequest(ContractModel):
    action: Literal["stop_selected", "stop_owned", "stop_response", "background_command"]
    requestId: str = Field(min_length=1, max_length=200)
    agent: str = Field(min_length=1, max_length=200)
    accountKey: str = Field(min_length=1, max_length=200)
    epoch: SafeInt
    process: ProcessIdentity | None = None
    processes: list[ProcessIdentity] = Field(default_factory=list, max_length=500)
    includeDescendants: bool = False
    responseTarget: ProcessResponseTarget | None = None
    itemId: str | None = Field(default=None, min_length=1, max_length=200,
                               description="Running command item that moves to the background")

    @model_validator(mode="after")
    def validate_action_scope(self) -> "ProcessControlRequest":
        if self.action == "stop_selected" and (self.process is None or self.processes):
            raise ValueError("Select exactly one process to stop")
        if self.action == "stop_owned" and self.process is not None:
            raise ValueError("Owned process stop accepts a process snapshot")
        if self.action == "stop_owned" and not self.processes:
            raise ValueError("No owned process identities were supplied")
        if self.action in {"stop_response", "background_command"}:
            name = "Response stop" if self.action == "stop_response" else "Background move"
            if self.process is not None or self.processes:
                raise ValueError(f"{name} does not accept process identities")
            if self.responseTarget is None:
                raise ValueError(f"{name} requires the current response identity")
            if (self.responseTarget.agent != self.agent
                    or self.responseTarget.accountKey != self.accountKey
                    or self.responseTarget.epoch != self.epoch):
                raise ValueError("Response identity does not match the process owner")
        elif self.responseTarget is not None:
            raise ValueError("Response identity applies only to response stop and background move")
        if (self.action == "background_command") != (self.itemId is not None):
            raise ValueError("Background move requires exactly one command item")
        if self.action not in {"stop_owned", "stop_selected"} and self.includeDescendants:
            raise ValueError("Descendant scope applies only to process stop")
        return self


class ProcessActionStatus(ContractStrEnum):
    STOPPED = "stopped"
    REQUESTED = "requested"
    ALREADY_FINISHED = "already_finished"
    UNCERTAIN = "uncertain"
    FAILED = "failed"
    STALE = "stale"


class ProcessActionResult(ResponseModel):
    requestId: str
    status: Literal["completed", "partial", "uncertain"]
    processes: list[dict[str, JsonValue]] = Field(default_factory=list)
    response: dict[str, JsonValue] | None = None
    error: str | None = None


class ProcessReceiptQuery(ContractModel):
    requestId: str = Field(min_length=1, max_length=200)
    agent: str = Field(min_length=1, max_length=200)
    accountKey: str = Field(min_length=1, max_length=200)
    epoch: SafeQueryInt


class ProcessReceipt(ResponseModel):
    found: bool
    result: ProcessActionResult | None = None


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
    size: UInt64  # stat().st_size is a signed 64-bit file size


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
    id: str
