"""FastAPI routes for voice sessions, transcript records, and approvals."""

from __future__ import annotations

from typing import TYPE_CHECKING, Protocol, TypedDict, Unpack, cast

from fastapi import APIRouter, Request
from starlette.responses import Response

from studio_api.models import ErrorResponse, JsonValue
from .models import (
    VoiceApprovalResponse,
    VoiceApprovalSpeechRequest,
    VoiceApprovalsRequest,
    VoiceApprovalsResponse,
    VoiceApproveRequest,
    VoiceAudioRequest,
    VoiceAudioResponse,
    VoiceDeliveryResponse,
    VoiceEndRequest,
    VoiceRecordKind,
    VoiceRecordRequest,
    VoiceRecordResponse,
    VoiceRecordsRequest,
    VoiceRecordsResponse,
    VoiceSessionResponse,
    VoiceSpeechRequest,
    VoiceSpeechResponse,
    VoiceStartRequest,
    VoiceStatusRequest,
    VoiceStatusResponse,
    VoiceSubmitRequest,
    UnknownVoiceActionBody,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext


class RecordOptionalFields(TypedDict, total=False):
    text: str
    item_id: str
    previous_item_id: str
    payload: dict[str, JsonValue] | None


class VoiceApi(Protocol):
    """Typed facade for the existing voice service implementation."""

    def status(self, agent: str) -> object: ...

    def start(self, agent: str, session_id: str, sdp: str) -> object: ...

    def end(self, agent: str, session_id: str) -> object: ...

    def record(
        self,
        agent: str,
        session_id: str,
        event_id: str,
        kind: VoiceRecordKind,
        **optional: Unpack[RecordOptionalFields],
    ) -> object: ...

    def records(self, agent: str, after: int = 0) -> object: ...

    def speech(self, agent: str, record_id: str, session_id: str) -> object: ...

    def submit(
        self,
        agent: str,
        message_id: str,
        record_ids: list[str],
        edited_text: str | None = None,
    ) -> object: ...

    def audio(
        self, agent: str, session_id: str, chunk_id: str, audio: str, mime: str
    ) -> object: ...

    def approvals(self, agent: str) -> object: ...

    def approval_speech(self, agent: str, request_id: str) -> object: ...

    def approve(
        self, agent: str, session_id: str, speech_id: str, transcript_id: str
    ) -> object: ...


class VoiceRuntime(Protocol):
    """Narrow the legacy runtime to its voice-service accessor."""

    def voice(self) -> VoiceApi: ...


def _voice(context: ApiContext, request: Request) -> VoiceApi | Response:
    runtime = cast(VoiceRuntime | None, context.runtime)
    if runtime is None:
        return context.send(request, {"error": "Not found"}, status=404)
    return runtime.voice()


def _send(context: ApiContext, request: Request, value: object) -> Response:
    return context.send(request, value)


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.post(
        "/api/voice/status",
        response_model=VoiceStatusResponse,
    )
    def status(request: Request, body: VoiceStatusRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(context, request, voice.status(body.agent))

    @router.post(
        "/api/voice/start",
        response_model=VoiceSessionResponse,
    )
    def start(request: Request, body: VoiceStartRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(
            context, request, voice.start(body.agent, body.session_id, body.sdp)
        )

    @router.post(
        "/api/voice/end",
        response_model=VoiceSessionResponse,
    )
    def end(request: Request, body: VoiceEndRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(context, request, voice.end(body.agent, body.session_id))

    @router.post(
        "/api/voice/record",
        response_model=VoiceRecordResponse,
    )
    def record(request: Request, body: VoiceRecordRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        supplied = body.model_dump(mode="json", exclude_unset=True)
        optional = {
            key: supplied[key]
            for key in ("text", "item_id", "previous_item_id", "payload")
            if key in supplied
        }
        return _send(
            context,
            request,
            voice.record(
                body.agent,
                body.session_id,
                body.event_id,
                body.kind,
                **cast(RecordOptionalFields, optional),
            ),
        )

    @router.post(
        "/api/voice/records",
        response_model=VoiceRecordsResponse,
    )
    def records(request: Request, body: VoiceRecordsRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        supplied = body.model_dump(mode="json", exclude_unset=True)
        if "after" in supplied:
            value = voice.records(body.agent, body.after)
        else:
            value = voice.records(body.agent)
        return _send(context, request, value)

    @router.post(
        "/api/voice/speech",
        response_model=VoiceSpeechResponse,
    )
    def speech(request: Request, body: VoiceSpeechRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(
            context,
            request,
            voice.speech(body.agent, body.record_id, body.session_id),
        )

    @router.post(
        "/api/voice/submit",
        response_model=VoiceDeliveryResponse,
    )
    def submit(request: Request, body: VoiceSubmitRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        supplied = body.model_dump(mode="json", exclude_unset=True)
        if "edited_text" in supplied:
            value = voice.submit(
                body.agent, body.message_id, body.record_ids, body.edited_text
            )
        else:
            value = voice.submit(
                body.agent, body.message_id, body.record_ids
            )
        return _send(context, request, value)

    @router.post(
        "/api/voice/audio",
        response_model=VoiceAudioResponse,
    )
    def audio(request: Request, body: VoiceAudioRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(
            context,
            request,
            voice.audio(
                body.agent, body.session_id, body.chunk_id, body.audio, body.mime
            ),
        )

    @router.post(
        "/api/voice/approvals",
        response_model=VoiceApprovalsResponse,
    )
    def approvals(request: Request, body: VoiceApprovalsRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(context, request, voice.approvals(body.agent))

    @router.post(
        "/api/voice/approval_speech",
        response_model=VoiceRecordResponse,
    )
    def approval_speech(
        request: Request, body: VoiceApprovalSpeechRequest
    ) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(
            context, request, voice.approval_speech(body.agent, body.request_id)
        )

    @router.post(
        "/api/voice/approve",
        response_model=VoiceApprovalResponse,
    )
    def approve(request: Request, body: VoiceApproveRequest) -> Response:
        voice = _voice(context, request)
        if isinstance(voice, Response):
            return voice
        return _send(
            context,
            request,
            voice.approve(
                body.agent, body.session_id, body.speech_id, body.transcript_id
            ),
        )

    @router.post(
        "/api/voice/{action:path}",
        response_model=ErrorResponse,
        status_code=400,
        include_in_schema=False,
    )
    def unknown_action(
        request: Request, action: str, body: UnknownVoiceActionBody
    ) -> Response:
        del body
        del action
        if context.runtime is None:
            return context.send(request, {"error": "Not found"}, status=404)
        return context.send(request, {"error": "Unknown voice action"}, status=400)

    return router
