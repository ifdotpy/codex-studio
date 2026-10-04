"""Route contract and side-effect-order tests for the voice API."""

from __future__ import annotations

import base64
import hashlib
import sqlite3
import stat
import tempfile
import threading
import unittest
from collections.abc import Callable, Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import TYPE_CHECKING, cast

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.routing import APIRoute
from fastapi.testclient import TestClient
from pydantic import TypeAdapter
from starlette.responses import JSONResponse, Response

from studio_api.models import ErrorResponse, ResponseModel
from studio_api.voice.models import VoiceRecordKind
from studio_api.voice.router import create_router

if TYPE_CHECKING:
    from studio_api.context import ApiContext


class FakeVoice:
    def __init__(self) -> None:
        self.calls: list[tuple[str, tuple[object, ...], dict[str, object]]] = []

    def __getattr__(self, name: str) -> Callable[..., object]:
        def call(*args: object, **kwargs: object) -> object:
            self.calls.append((name, args, kwargs))
            if name == "status":
                return {"configured": True, "transport": "native", "auth": "chatgpt"}
            if name in {"start", "end"}:
                return {
                    "session_id": "session-1",
                    "state": "ready",
                    "sdp": "v=0 answer",
                    "error": None,
                    "ended": None,
                }
            if name in {"record", "approval_speech", "audio"}:
                return {
                    "seq": 1,
                    "id": "event-1",
                    "agent": "agent-1",
                    "session": "session-1",
                    "kind": "user",
                    "text": "hello",
                    "item_id": "",
                    "previous_item_id": "",
                    "payload": "{}",
                    "created": 1.25,
                }
            if name == "records":
                return {"records": [], "cursor": 0, "delivered": [], "session": None}
            if name == "speech":
                return {
                    "id": "speech-1",
                    "state": "pending",
                    "playback": "not confirmed",
                }
            if name == "submit":
                return {"id": "message-1", "status": "queued"}
            if name == "approvals":
                return {"requests": []}
            if name == "approve":
                return {"status": "answered", "replayed": True}
            raise AssertionError(f"Unexpected voice action: {name}")

        return call


class FakeRuntime:
    def __init__(self, voice: object) -> None:
        self._voice = voice

    def voice(self) -> object:
        return self._voice


class FakeContext:
    def __init__(self, voice: object | None) -> None:
        self.runtime = FakeRuntime(voice) if voice is not None else None
        self.sent: list[tuple[Request, object]] = []

    def send(self, request: Request, value: object, **_kwargs: object) -> Response:
        self.sent.append((request, value))
        status_value = _kwargs.get("status", 200)
        status = status_value if isinstance(status_value, int) else 200
        route = request.scope["route"]
        response_model = ErrorResponse if status >= 400 else route.response_model
        if response_model is not None:
            parsed = TypeAdapter(response_model).validate_python(value)
            if isinstance(parsed, ResponseModel):
                value = parsed.wire_dump()
        return JSONResponse(value, status_code=status)


def app_for(context: FakeContext) -> FastAPI:
    app = FastAPI()
    app.include_router(create_router(cast("ApiContext", context)))

    @app.exception_handler(RequestValidationError)
    def request_error(_request: Request, error: RequestValidationError) -> JSONResponse:
        result = ErrorResponse(error="Invalid request")
        return JSONResponse(result.wire_dump(), status_code=400)

    @app.exception_handler(ValueError)
    def service_error(_request: Request, error: ValueError) -> JSONResponse:
        result = ErrorResponse(error=str(error))
        return JSONResponse(result.wire_dump(), status_code=400)

    return app


class SqliteVoiceRuntime:
    def __init__(self, root: str) -> None:
        self.root = Path(root)
        self.database = self.root / "voice.sqlite3"
        self.lock = threading.RLock()

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        connection = sqlite3.connect(self.database)
        connection.row_factory = sqlite3.Row
        try:
            yield connection
            connection.commit()
        finally:
            connection.close()

    def agent(self, agent_id: str) -> dict[str, object]:
        return {"id": agent_id, "isLead": True, "deletedAt": None}


class VoiceRouterTests(unittest.TestCase):
    def setUp(self) -> None:
        self.voice = FakeVoice()
        self.context = FakeContext(self.voice)
        self.router = create_router(cast("ApiContext", self.context))
        self.client = TestClient(app_for(self.context))

    def test_all_legacy_voice_actions_are_registered(self) -> None:
        routes = {
            route.path
            for route in self.router.routes
            if (
                isinstance(route, APIRoute)
                and route.methods is not None
                and "POST" in route.methods
            )
        }
        expected = {
            f"/api/voice/{action}"
            for action in (
                "status",
                "start",
                "end",
                "record",
                "records",
                "speech",
                "submit",
                "audio",
                "approvals",
                "approval_speech",
                "approve",
            )
        }
        self.assertTrue(expected.issubset(routes))

    def test_invalid_record_kind_is_rejected_before_service_call(self) -> None:
        response = self.client.post(
            "/api/voice/record",
            json={
                "agent": "agent-1",
                "session_id": "session-1",
                "event_id": "event-1",
                "kind": "administrator",
            },
        )
        self.assertEqual(400, response.status_code)
        self.assertEqual({"error": "Invalid request"}, response.json())
        self.assertEqual([], self.voice.calls)

    def test_unknown_record_field_does_not_trigger_side_effect(self) -> None:
        response = self.client.post(
            "/api/voice/record",
            json={
                "agent": "agent-1",
                "session_id": "session-1",
                "event_id": "event-1",
                "kind": "user",
                "unexpected": "value",
            },
        )
        # Strict extra-field rejection must precede the service call.
        self.assertEqual(400, response.status_code)
        self.assertEqual([], self.voice.calls)

    def test_record_defaults_keep_the_old_handler_arguments(self) -> None:
        response = self.client.post(
            "/api/voice/record",
            json={
                "agent": "agent-1",
                "session_id": "session-1",
                "event_id": "event-1",
                "kind": VoiceRecordKind.USER.value,
            },
        )
        self.assertEqual(200, response.status_code)
        self.assertEqual("event-1", response.json()["id"])
        name, args, kwargs = self.voice.calls[0]
        self.assertEqual("record", name)
        self.assertEqual(
            ("agent-1", "session-1", "event-1", VoiceRecordKind.USER), args
        )
        self.assertEqual({}, kwargs)

    def test_submit_preserves_durable_message_identity_on_repeated_calls(self) -> None:
        payload = {
            "agent": "agent-1",
            "message_id": "message-1",
            "record_ids": ["transcript-1"],
        }
        first = self.client.post("/api/voice/submit", json=payload)
        retry = self.client.post("/api/voice/submit", json=payload)
        self.assertEqual(200, first.status_code)
        self.assertEqual(first.json(), retry.json())
        self.assertEqual(2, len(self.voice.calls))
        self.assertEqual(self.voice.calls[0], self.voice.calls[1])
        self.assertEqual("message-1", self.voice.calls[0][1][1])

    def test_route_reaches_voice_store_and_keeps_record_retry_identity(self) -> None:
        from codex_voice import VoiceStore

        with tempfile.TemporaryDirectory() as directory:
            store = VoiceStore(SqliteVoiceRuntime(directory))  # type: ignore[no-untyped-call]
            client = TestClient(app_for(FakeContext(store)))
            body = {
                "agent": "agent-1",
                "session_id": "",
                "event_id": "record-identity-1",
                "kind": "user",
                "text": "captured words",
            }
            first = client.post("/api/voice/record", json=body)
            replay = client.post("/api/voice/record", json=body)
            self.assertEqual(200, first.status_code)
            self.assertEqual(first.json(), replay.json())

            conflict = client.post(
                "/api/voice/record", json={**body, "text": "changed words"}
            )
            self.assertEqual(400, conflict.status_code)
            self.assertEqual(
                {"error": "Voice event identity conflicts with its original content"},
                conflict.json(),
            )
            with store.runtime.db() as database:
                row_count = database.execute(
                    "SELECT COUNT(*) FROM voice_records WHERE id=?",
                    ("record-identity-1",),
                ).fetchone()[0]
            self.assertEqual(1, row_count)

    def test_audio_route_saves_private_file_and_replays_exact_chunk(self) -> None:
        from codex_voice import VoiceStore

        with tempfile.TemporaryDirectory() as directory:
            runtime = SqliteVoiceRuntime(directory)
            store = VoiceStore(runtime)  # type: ignore[no-untyped-call]
            with runtime.db() as database:
                database.execute(
                    "INSERT INTO voice_sessions(id,agent,created) VALUES(?,?,?)",
                    ("session-1", "agent-1", 1.0),
                )
            client = TestClient(app_for(FakeContext(store)))
            audio_bytes = b"isolated audio fixture"
            body = {
                "agent": "agent-1",
                "session_id": "session-1",
                "chunk_id": "chunk-1",
                "audio": base64.b64encode(audio_bytes).decode("ascii"),
                "mime": "audio/webm",
            }
            first = client.post("/api/voice/audio", json=body)
            replay = client.post("/api/voice/audio", json=body)
            self.assertEqual(200, first.status_code)
            self.assertEqual(first.json(), replay.json())
            filename = hashlib.sha256(
                b"agent-1:session-1:chunk-1"
            ).hexdigest()
            audio_path = runtime.root / "voice-audio" / filename
            self.assertEqual(audio_bytes, audio_path.read_bytes())
            self.assertEqual(0o600, stat.S_IMODE(audio_path.stat().st_mode))

            conflict_body = {
                **body,
                "audio": base64.b64encode(b"different").decode("ascii"),
            }
            conflict = client.post("/api/voice/audio", json=conflict_body)
            self.assertEqual(400, conflict.status_code)
            self.assertEqual(
                "Audio chunk identity conflicts with original content",
                conflict.json()["error"],
            )
            self.assertEqual(audio_bytes, audio_path.read_bytes())
            with runtime.db() as database:
                row_count = database.execute(
                    "SELECT COUNT(*) FROM voice_records WHERE id LIKE 'audio:%'"
                ).fetchone()[0]
            self.assertEqual(1, row_count)

    def test_unknown_action_keeps_the_legacy_400_error(self) -> None:
        response = self.client.post(
            "/api/voice/not/an/action", json={"agent": "agent-1"}
        )
        self.assertEqual(400, response.status_code)
        self.assertEqual({"error": "Unknown voice action"}, response.json())
        self.assertEqual([], self.voice.calls)

    def test_unavailable_runtime_keeps_the_legacy_404(self) -> None:
        context = FakeContext(None)
        client = TestClient(app_for(context))
        response = client.post("/api/voice/status", json={"agent": "agent-1"})
        self.assertEqual(404, response.status_code)
        self.assertEqual({"error": "Not found"}, response.json())

    def test_voice_route_response_model_is_documented_in_openapi(self) -> None:
        schema = self.client.get("/openapi.json").json()
        operation = schema["paths"]["/api/voice/records"]["post"]
        self.assertIn("requestBody", operation)
        self.assertIn("200", operation["responses"])
        self.assertIn("400", operation["responses"])
        audio = schema["paths"]["/api/voice/audio"]["post"]
        self.assertIn("413", audio["responses"])
        self.assertIn("415", audio["responses"])


if __name__ == "__main__":
    unittest.main()
