"""Typed wire contracts for transcript history, search, and checkpoints."""

from __future__ import annotations

from typing import Literal

from pydantic import ConfigDict, Field, StrictBool, StrictStr, model_validator

from studio_api.models import ContractModel, JsonValue, ResponseModel


class TranscriptItem(ContractModel):
    """A saved transcript record; extra typed fields carry provider metadata."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    role: StrictStr | None = None
    title: StrictStr | None = None
    text: StrictStr | None = None
    at: int | float | StrictStr | None = None
    turnId: StrictStr | None = None
    turnStatus: StrictStr | None = None
    agent: StrictStr | None = None
    sourceId: StrictStr | None = None
    clientMessageId: StrictStr | None = None
    truncated: StrictBool | None = None


class TranscriptAgent(ContractModel):
    id: StrictStr
    status: StrictStr | None = None
    activity: StrictStr | None = None
    inFlight: StrictBool | None = None
    contextUsage: JsonValue = None
    compactions: int | None = None
    compactionsObservedOnly: int | None = None


class TranscriptPageResponse(ResponseModel):
    items: list[TranscriptItem]
    truncated: StrictBool
    nextCursor: StrictStr | None = None
    nextAfterCursor: StrictStr | None = None
    historyVersion: StrictStr | None = None
    unavailable: StrictStr | None = None
    agent: TranscriptAgent | None = None
    tail: StrictStr | None = None


class TranscriptSearchResult(ContractModel):
    id: StrictStr
    sourceId: StrictStr
    clientMessageId: StrictStr | None = None
    role: StrictStr
    turnId: StrictStr | None = None
    text: StrictStr
    agent: StrictStr
    excerpt: StrictStr


class TranscriptSearchResponse(ResponseModel):
    results: list[TranscriptSearchResult]
    truncated: StrictBool


class TranscriptItemResponse(ResponseModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    agent: StrictStr
    sourceId: StrictStr
    role: StrictStr | None = None
    title: StrictStr | None = None
    text: StrictStr | None = None
    at: int | float | StrictStr | None = None
    turnId: StrictStr | None = None
    turnStatus: StrictStr | None = None
    clientMessageId: StrictStr | None = None
    truncated: StrictBool | None = None


class SearchResult(ContractModel):
    id: StrictStr
    agent: StrictStr
    type: Literal["message", "work", "complaint", "plan", "room"] | None = None
    kind: StrictStr | None = None
    reference: StrictStr | None = None
    room: StrictStr | None = None
    excerpt: StrictStr | None = None
    text: StrictStr | None = None


class SearchResponse(ResponseModel):
    results: list[SearchResult]
    query: StrictStr
    limit: int


class SearchItemResponse(ResponseModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    kind: Literal["message", "room", "work", "plan", "complaint"]
    agent: StrictStr
    text: StrictStr
    role: StrictStr | None = None
    title: StrictStr | None = None
    at: int | float | StrictStr | None = None
    turnId: StrictStr | None = None
    turnStatus: StrictStr | None = None
    sourceId: StrictStr | None = None
    clientMessageId: StrictStr | None = None
    truncated: StrictBool | None = None


class Checkpoint(ContractModel):
    """Checkpoint summary fields; payload/lineage extensions remain JSON typed."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    agent: StrictStr | None = None
    rootId: StrictStr | None = None
    label: StrictStr
    tree: StrictStr | None = None
    commit: StrictStr | None = None
    ref: StrictStr | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    created: int | float
    cwd: StrictStr | None = None


class CheckpointsResponse(ResponseModel):
    checkpoints: list[Checkpoint]


class CheckpointPreviewResponse(ResponseModel):
    checkpoint: Checkpoint
    expectedTree: StrictStr
    diff: StrictStr
    patch: StrictStr
    truncated: StrictBool
    canRestore: StrictBool


class CheckpointCaptureResponse(ResponseModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    agent: StrictStr | None = None
    rootId: StrictStr | None = None
    label: StrictStr
    tree: StrictStr | None = None
    commit: StrictStr | None = None
    ref: StrictStr | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    created: int | float
    cwd: StrictStr | None = None


class BranchAgent(ContractModel):
    """Common branch identity fields plus typed runtime-specific metadata."""

    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr
    name: StrictStr | None = None
    status: StrictStr | None = None
    cwd: StrictStr | None = None
    model: StrictStr | None = None
    accountKey: StrictStr | None = None
    threadId: StrictStr | None = None


class CheckpointRestoreResponse(ResponseModel):
    status: Literal["restored"]
    checkpoint: StrictStr


class BranchDraft(ContractModel):
    text: StrictStr | None = None
    prefixText: StrictStr | None = None
    assets: list[JsonValue] | None = None


class BranchResponse(ResponseModel):
    model_config = ConfigDict(extra="allow", strict=True)
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)

    id: StrictStr | None = None
    agent: BranchAgent | None = None
    draft: BranchDraft | None = None


class TranscriptQuery(ContractModel):
    id: StrictStr | None = None
    before: StrictStr | None = None
    around: StrictStr | None = None
    after: StrictStr | None = None
    limit: StrictStr | None = None


class TranscriptItemQuery(ContractModel):
    id: StrictStr | None = None
    message_id: StrictStr | None = None


class TranscriptSearchQuery(ContractModel):
    id: StrictStr | None = None
    q: StrictStr | None = None
    limit: StrictStr | None = None


class SearchQuery(ContractModel):
    q: StrictStr | None = None
    limit: StrictStr | None = None


class CheckpointsQuery(ContractModel):
    agent: StrictStr | None = None


class BranchRequest(ContractModel):
    agent: StrictStr
    message_id: StrictStr
    id: StrictStr
    before: StrictBool | None = None

    @model_validator(mode="before")
    @classmethod
    def before_must_be_boolean_when_supplied(cls, value: object) -> object:
        if isinstance(value, dict) and "before" in value and type(value["before"]) is not bool:
            raise ValueError("before must be a boolean")
        return value


class CheckpointCaptureRequest(ContractModel):
    agent: StrictStr
    label: StrictStr = "Checkpoint"


class CheckpointPreviewRequest(ContractModel):
    agent: StrictStr
    checkpoint: StrictStr


class CheckpointRestoreRequest(ContractModel):
    agent: StrictStr
    checkpoint: StrictStr | None = None
    checkpoint_id: StrictStr | None = None
    expectedTree: StrictStr
