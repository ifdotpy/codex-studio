"""Strict request and response contracts for the voice API."""

from __future__ import annotations

from typing import Annotated, Literal, Union

from pydantic import (
    ConfigDict,
    Field,
    StrictBool,
    StrictFloat,
    StrictInt,
    StrictStr,
    model_validator,
)

from studio_api.models import ContractModel, ContractStrEnum, JsonValue, ResponseModel


class VoiceRecordKind(ContractStrEnum):
    USER = "user"
    COURIER = "courier"
    ASSISTANT = "assistant"
    EVENT = "event"
    PLAYBACK = "playback"
    ORCHESTRATOR = "orchestrator"


class PlaybackStatus(ContractStrEnum):
    QUEUED = "queued"
    PLAYING = "playing"
    PLAYED = "played"
    INTERRUPTED = "interrupted"
    UNKNOWN = "unknown"


class VoiceSessionState(ContractStrEnum):
    CONNECTING = "connecting"
    READY = "ready"
    STOPPING = "stopping"
    ENDED = "ended"
    FAILED = "failed"
    LOST = "lost"
    UNKNOWN = "unknown"


class VoicePlaybackConfirmation(ContractStrEnum):
    NOT_CONFIRMED = "not confirmed"


class VoiceSpeechState(ContractStrEnum):
    PENDING = "pending"
    SUBMITTED = "submitted"
    FAILED = "failed"
    UNKNOWN = "unknown"


class VoiceDeliveryState(ContractStrEnum):
    PENDING = "pending"
    RESERVED = "reserved"
    DISPATCHING = "dispatching"
    UNCERTAIN = "uncertain"
    FAILED = "failed"
    CANCELLED = "cancelled"
    QUEUED = "queued"
    DELIVERED = "delivered"
    STORED_ONLY = "stored_only"


class UnknownVoiceActionBody(ContractModel):
    """Extensible JSON object consumed only by the legacy unknown-action route."""

    model_config = ConfigDict(extra="allow")
    __pydantic_extra__: dict[str, JsonValue] = Field(init=False)


class VoiceStatusRequest(ContractModel):
    agent: StrictStr


class VoiceStartRequest(ContractModel):
    agent: StrictStr
    session_id: StrictStr
    sdp: StrictStr


class VoiceEndRequest(ContractModel):
    agent: StrictStr
    session_id: StrictStr


class VoiceRecordRequest(ContractModel):
    agent: StrictStr
    session_id: StrictStr
    event_id: StrictStr
    kind: VoiceRecordKind
    text: StrictStr = ""
    item_id: StrictStr = ""
    previous_item_id: StrictStr = ""
    payload: dict[StrictStr, JsonValue] | None = None

    @model_validator(mode="after")
    def validate_record_payload(self) -> VoiceRecordRequest:
        payload = self.payload or {}
        if self.kind is VoiceRecordKind.PLAYBACK:
            if self.text not in {status.value for status in PlaybackStatus}:
                raise ValueError("Invalid playback status")
            if not isinstance(payload.get("record_id"), str):
                raise ValueError("Invalid playback status")
        if "utterance_order" in payload:
            utterance_order = payload["utterance_order"]
            if (
                type(utterance_order) is not int
                or not 0 <= utterance_order <= 1_000_000_000
            ):
                raise ValueError("Invalid utterance order")
        return self


class VoiceRecordsRequest(ContractModel):
    agent: StrictStr
    after: StrictInt = 0


class VoiceSpeechRequest(ContractModel):
    agent: StrictStr
    record_id: StrictStr
    session_id: StrictStr


class VoiceSubmitRequest(ContractModel):
    agent: StrictStr
    message_id: StrictStr
    record_ids: list[StrictStr]
    edited_text: StrictStr | None = None


class VoiceAudioRequest(ContractModel):
    agent: StrictStr
    session_id: StrictStr
    chunk_id: StrictStr
    audio: StrictStr
    mime: StrictStr


class VoiceApprovalsRequest(ContractModel):
    agent: StrictStr


class VoiceApprovalSpeechRequest(ContractModel):
    agent: StrictStr
    request_id: StrictStr


class VoiceApproveRequest(ContractModel):
    agent: StrictStr
    session_id: StrictStr
    speech_id: StrictStr
    transcript_id: StrictStr


class VoiceTransport(ContractStrEnum):
    NATIVE = "native"


class VoiceAuth(ContractStrEnum):
    CHATGPT = "chatgpt"


class VoiceStatusResponse(ResponseModel):
    configured: StrictBool
    transport: VoiceTransport
    auth: VoiceAuth


class VoiceSessionResponse(ResponseModel):
    session_id: StrictStr
    state: VoiceSessionState | None
    sdp: StrictStr | None
    error: StrictStr | None
    ended: StrictFloat | StrictInt | None


class VoiceRecordResponse(ResponseModel):
    seq: StrictInt
    id: StrictStr
    agent: StrictStr
    session: StrictStr
    kind: VoiceRecordKind
    text: StrictStr
    item_id: StrictStr
    previous_item_id: StrictStr
    payload: StrictStr
    created: StrictFloat | StrictInt


class VoiceRecordsResponse(ResponseModel):
    records: list[VoiceRecordResponse]
    cursor: StrictInt
    delivered: list[StrictStr]
    session: VoiceSessionResponse | None


class VoiceSpeechQueuedResponse(ResponseModel):
    id: StrictStr
    state: VoiceSpeechState
    playback: VoicePlaybackConfirmation


class VoiceSpeechReceiptResponse(ResponseModel):
    id: StrictStr
    session: StrictStr
    record_id: StrictStr
    state: VoiceSpeechState
    error: StrictStr | None


VoiceSpeechResponse = Annotated[
    Union[VoiceSpeechQueuedResponse, VoiceSpeechReceiptResponse],
    Field(union_mode="left_to_right"),
]


class VoiceDeliveryResponse(ResponseModel):
    id: StrictStr
    status: VoiceDeliveryState
    error: StrictStr | None = None
    waitingFor: list[JsonValue] | None = None


class VoiceApprovalsResponse(ResponseModel):
    # Runtime request records remain extensible provider-owned protocol payloads.
    requests: list[JsonValue]


class VoiceApprovalResponse(ResponseModel):
    status: Literal["answered"]
    replayed: StrictBool | None = None


VoiceAudioResponse = VoiceRecordResponse
