"""Strict wire-model tests for the voice API."""

from __future__ import annotations

import unittest

from pydantic import TypeAdapter, ValidationError

from studio_api.models import JsonValue
from studio_api.voice.models import (
    VoiceApprovalResponse,
    VoiceRecordKind,
    VoiceRecordRequest,
    VoiceSpeechResponse,
    UnknownVoiceActionBody,
)


class VoiceModelTests(unittest.TestCase):
    def test_record_kind_accepts_closed_wire_values_only(self) -> None:
        adapter: TypeAdapter[VoiceRecordKind] = TypeAdapter(VoiceRecordKind)
        self.assertIs(VoiceRecordKind.USER, adapter.validate_python("user"))
        for invalid in (b"user", 1, True, "superuser"):
            with self.subTest(invalid=invalid):
                with self.assertRaises(ValidationError):
                    adapter.validate_python(invalid)

    def test_record_request_rejects_coercions_and_unrecognized_fields(self) -> None:
        valid: dict[str, object] = {
            "agent": "lead-1",
            "session_id": "session-1",
            "event_id": "event-1",
            "kind": "user",
        }
        self.assertEqual(
            VoiceRecordKind.USER,
            VoiceRecordRequest.model_validate(valid).kind,
        )
        for modified in (
            {**valid, "agent": 5},
            {**valid, "kind": "made_up"},
            {**valid, "extra": "ignored by the old handler"},
        ):
            with self.subTest(modified=modified):
                with self.assertRaises(ValidationError):
                    VoiceRecordRequest.model_validate(modified)

    def test_playback_record_uses_closed_status_values(self) -> None:
        body: dict[str, object] = {
            "agent": "lead-1",
            "session_id": "session-1",
            "event_id": "event-1",
            "kind": "playback",
            "text": "played",
            "payload": {"record_id": "speech-1"},
        }
        self.assertEqual(
            VoiceRecordKind.PLAYBACK,
            VoiceRecordRequest.model_validate(body).kind,
        )
        with self.assertRaises(ValidationError):
            VoiceRecordRequest.model_validate({**body, "text": "heard"})

    def test_utterance_order_is_a_bounded_json_integer(self) -> None:
        body: dict[str, object] = {
            "agent": "lead-1",
            "session_id": "session-1",
            "event_id": "event-1",
            "kind": "user",
        }
        for value in (None, True, "3", 1_000_000_001):
            with self.subTest(value=value):
                with self.assertRaises(ValidationError):
                    VoiceRecordRequest.model_validate(
                        {**body, "payload": {"utterance_order": value}}
                    )
        valid = VoiceRecordRequest.model_validate(
            {**body, "payload": {"utterance_order": 1_000_000_000}}
        )
        self.assertIsNotNone(valid.payload)

    def test_unknown_action_body_preserves_legacy_json_object_acceptance(self) -> None:
        body = UnknownVoiceActionBody.model_validate(
            {"agent": "lead-1", "extra": {"opaque": [1, "two", None]}}
        )
        self.assertEqual("lead-1", body.__pydantic_extra__["agent"])

    def test_extensible_voice_payload_accepts_json_values(self) -> None:
        payload: dict[str, JsonValue] = {
            "native": True,
            "utterance_order": 4,
            "metadata": ["provider-specific", None],
        }
        request = VoiceRecordRequest.model_validate(
            {
                "agent": "lead-1",
                "session_id": "session-1",
                "event_id": "event-1",
                "kind": "user",
                "payload": payload,
            }
        )
        self.assertEqual(payload, request.payload)

    def test_response_dump_preserves_optional_omitted_and_null_fields(self) -> None:
        omitted = VoiceApprovalResponse.model_validate({"status": "answered"})
        explicit_null = VoiceApprovalResponse.model_validate(
            {"status": "answered", "replayed": None}
        )
        self.assertEqual({"status": "answered"}, omitted.wire_dump())
        self.assertEqual(
            {"status": "answered", "replayed": None}, explicit_null.wire_dump()
        )

    def test_speech_response_accepts_both_legacy_shapes(self) -> None:
        adapter: TypeAdapter[VoiceSpeechResponse] = TypeAdapter(VoiceSpeechResponse)
        queued = adapter.validate_python(
            {
                "id": "voice-session:record",
                "state": "pending",
                "playback": "not confirmed",
            }
        )
        receipt = adapter.validate_python(
            {
                "id": "voice-session:record",
                "session": "voice-session",
                "record_id": "record",
                "state": "submitted",
                "error": None,
            }
        )
        self.assertEqual("pending", queued.state)
        self.assertEqual("submitted", receipt.state)


if __name__ == "__main__":
    unittest.main()
