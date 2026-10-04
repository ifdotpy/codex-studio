"""Explicit wire contracts for transcript history, search, and checkpoints.

Transcript records mirror the fields emitted by ``Runtime.item`` and enriched
by ``Runtime.transcript`` / ``codex_transcript_history``. Native provider data
is represented in the transcript text (or as typed input records), not passed
through as unvalidated top-level fields.
"""

from __future__ import annotations

from typing import Literal

from pydantic import Field, StrictBool, StrictInt, StrictStr, model_validator

from studio_api.sync.models import AgentActivity
from studio_api.models import ContractModel, JsonValue, ResponseModel
from studio_api.sync.models import (
    AgentEntityDto,
    ComplaintResponseDto,
    WorkDecisionDto,
    WorkResultDto,
)

DeliveryMode = Literal["queue", "steer", "after_tool", "after_turn"]
DeliveryStatus = Literal[
    "pending", "reserved", "dispatching", "uncertain", "failed", "cancelled",
    "delivered", "stored_only", "queued", "accepted", "sent",
]


class TranscriptAsset(ContractModel):
    """Path-free attachment projection returned by the workspace service."""

    id: StrictStr
    agent: StrictStr
    name: StrictStr
    mime: StrictStr
    image: StrictBool
    size: int
    hash: StrictStr
    created: float


class TranscriptInput(ContractModel):
    """One native/runtime input batch entry shown inside a transcript item."""

    id: StrictStr | None = None
    at: float | None = None
    kind: StrictStr
    text: StrictStr
    truncated: StrictBool
    assets: list[TranscriptAsset] = Field(default_factory=list)
    clientMessageId: StrictStr | None = None
    deliveryStatus: DeliveryStatus | None = None
    requestedDelivery: DeliveryMode | None = None
    deliveryError: StrictStr | None = None
    materialized: StrictBool | None = None
    pending: StrictBool | None = None


class TranscriptRecord(ContractModel):
    """One persisted or synthesized transcript row with named UI fields."""

    id: StrictStr
    role: StrictStr | None = None
    kind: StrictStr | None = None
    title: StrictStr | None = None
    text: StrictStr
    at: float | StrictStr | None = None
    truncated: StrictBool | None = None
    turnId: StrictStr | None = None
    turnStatus: StrictStr | None = None
    phase: StrictStr | None = None
    streaming: StrictBool | None = None
    toolStatus: StrictStr | None = None
    inputs: list[TranscriptInput] | None = None
    assets: list[TranscriptAsset] | None = None
    sourceId: StrictStr | None = None
    clientMessageId: StrictStr | None = None
    agent: StrictStr | None = None
    materialized: StrictBool | None = None
    pending: StrictBool | None = None
    deliveryStatus: DeliveryStatus | None = None
    requestedDelivery: DeliveryMode | None = None
    deliveryError: StrictStr | None = None
    turnError: StrictStr | None = None
    turnErrorResolved: StrictBool | None = None
    reasoningMs: float | None = None
    reasoningSince: float | None = None
    reasoningObservedAt: float | None = None
    observedWait: StrictBool | None = None


class TranscriptMessageRecord(TranscriptRecord):
    """Transcript rows always carry their runtime role and display text."""

    role: StrictStr
    # A small number of older persisted rows have no title. Default values are
    # omitted by the shared sender, so the wire keeps that historical absence.
    title: StrictStr = ""
    pending: StrictBool = False


class TranscriptContextUsage(ContractModel):
    """Runtime token usage shape published by tokenUsage notifications."""

    tokens: StrictInt | None = None
    window: StrictInt | None = None
    at: float


class TranscriptAgent(ContractModel):
    id: StrictStr
    status: StrictStr | None = None
    activity: AgentActivity | None = None
    inFlight: StrictBool | None = None
    contextUsage: TranscriptContextUsage | None = None
    compactions: int | None = None
    compactionsObservedOnly: StrictBool | None = None


class TranscriptPageResponse(ResponseModel):
    items: list[TranscriptMessageRecord]
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


class TranscriptItemResponse(TranscriptMessageRecord, ResponseModel):
    """Full transcript item projection, including its canonical sync envelope."""


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


class SearchItemResponse(TranscriptRecord, ResponseModel):
    """Named producer fields for message, room, work, plan, and complaint items.

    ``native`` is the provider protocol's extensible plan update payload; all
    other returned fields have explicit application-owned types.
    """

    kind: Literal["message", "room", "work", "plan", "complaint"]
    agent: StrictStr
    text: StrictStr
    room: StrictStr | None = None
    sender: StrictStr | None = None
    seq: int | None = None
    deliveries: StrictStr | None = None
    name: StrictStr | None = None
    status: StrictStr | None = None
    created: float | None = None
    updated: float | None = None
    rootId: StrictStr | None = None
    leadId: StrictStr | None = None
    author: StrictStr | None = None
    title: StrictStr | None = None
    sourceType: StrictStr | None = None
    responses: list[ComplaintResponseDto] | None = None
    needsResponse: StrictBool | None = None
    readAt: float | None = None
    leadName: StrictStr | None = None
    authorName: StrictStr | None = None
    leadStopped: StrictBool | None = None
    leadDeleted: StrictBool | None = None
    recipient: Literal["user", "lead"] | None = None
    description: StrictStr | None = None
    owner: StrictStr | None = None
    dependencies: list[StrictStr] | None = None
    version: int | None = None
    results: list[WorkResultDto] | None = None
    decisions: list[WorkDecisionDto] | None = None
    createdBy: StrictStr | None = None
    blockedBy: list[StrictStr] | None = None
    displayStatus: StrictStr | None = None
    native: JsonValue = Field(
        default=None,
        description="Provider-native plan update fields retained for plan display.",
    )
    steps: list[JsonValue] | None = None
    userHidden: StrictBool | None = None
    members: list[StrictStr] | None = None
    projectPath: StrictStr | None = None
    radio: JsonValue = Field(
        default=None,
        description="Extensible room radio configuration from the room producer.",
    )


class CheckpointSummary(ContractModel):
    """Public checkpoint record produced by ``capture_checkpoint`` summary."""

    id: StrictStr
    agent: StrictStr | None = None
    rootId: StrictStr | None = None
    label: StrictStr
    tree: StrictStr | None = None
    commit: StrictStr | None = None
    ref: StrictStr | None = None
    threadId: StrictStr | None = None
    turnId: StrictStr | None = None
    created: float
    cwd: StrictStr | None = None
    historyParent: StrictStr | None = None
    historyBoundary: int | None = None


class CheckpointsResponse(ResponseModel):
    checkpoints: list[CheckpointSummary]


class CheckpointPreviewResponse(ResponseModel):
    checkpoint: CheckpointSummary
    expectedTree: StrictStr
    diff: StrictStr
    patch: StrictStr
    truncated: StrictBool
    canRestore: StrictBool


class CheckpointCaptureResponse(CheckpointSummary, ResponseModel):
    """Flat checkpoint summary returned by the legacy capture endpoint."""


class CheckpointRestoreResponse(ResponseModel):
    status: Literal["restored"]
    checkpoint: StrictStr


class BranchDraft(ContractModel):
    text: StrictStr | None = None
    prefixText: StrictStr | None = None
    assets: list[TranscriptAsset] | None = None


class BranchResponse(AgentEntityDto, ResponseModel):
    """Canonical public agent projection plus the branch draft payload."""

    draft: BranchDraft | None = None


class TranscriptQuery(ContractModel):
    id: StrictStr | None = None
    before: StrictStr | None = None
    around: StrictStr | None = None
    after: StrictStr | None = None
    limit: int | None = Field(default=None, strict=False)


class TranscriptItemQuery(ContractModel):
    id: StrictStr | None = None
    message_id: StrictStr | None = None


class TranscriptSearchQuery(ContractModel):
    id: StrictStr | None = None
    q: StrictStr | None = None
    limit: int | None = Field(default=None, strict=False)


class SearchQuery(ContractModel):
    q: StrictStr | None = None
    limit: int | None = Field(default=None, strict=False)


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
