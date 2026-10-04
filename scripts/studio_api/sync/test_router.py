"""Isolated caller-level checks for sync and draft routes."""

from __future__ import annotations

import json
import asyncio
import threading
import unittest
from typing import cast

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from fastapi.responses import JSONResponse
from fastapi.routing import APIRoute
from pydantic import TypeAdapter
from unittest.mock import patch

from studio_api.context import ApiContext
from studio_api.models import ErrorResponse
from studio_api.sync.models import SyncStreamQuery, TranscriptStreamQuery
from studio_api.responses import install_error_response_docs
from studio_api.sync.router import create_router


class StoreStub:
    def __init__(self) -> None:
        self.pull_arguments: tuple[object, ...] | None = None
        self.push_calls: list[list[dict[str, object]]] = []
        self.stream_calls = 0
        self.stream_cursors: list[int] = []
        self.stream_batches: list[dict[str, object]] = []
        self.runtime: RuntimeStub | None = None
        self.invalid_push_response = False
        self.reset_pull = False

    def identity(self) -> dict[str, object]:
        return {"workspaceId": "workspace-a", "syncProtocol": 2, "chatState": True}

    def pull(self, *args: object) -> dict[str, object]:
        self.pull_arguments = args
        if self.reset_pull:
            return {
                "workspaceId": "workspace-a", "reset": True, "floor": 4, "maxSeq": 8,
            }
        return {
            "workspaceId": "workspace-a",
            "documents": [],
            "checkpoint": {"seq": 8},
            "maxSeq": 8,
        }

    def generation(self) -> int:
        return 3

    def generation_state(self) -> dict[str, object]:
        return {"protocol": 2, "workspaceId": "workspace-a", "generations": {
            "state": 1, "transcripts": 1, "drafts": 1,
        }}

    def push_drafts(self, rows: list[dict[str, object]]) -> list[dict[str, object]]:
        self.push_calls.append(rows)
        if self.invalid_push_response:
            return [{"id": "x", "payload": "{}", "seq": True, "_deleted": False}]
        return []

    def stream_batch(self, _scope: str, cursor: int) -> dict[str, object]:
        self.stream_calls += 1
        self.stream_cursors.append(cursor)
        if self.stream_batches:
            batch = self.stream_batches.pop(0)
            if batch["kind"] == "idle" and not self.stream_batches and self.runtime is not None:
                self.runtime.closed = True
            return batch
        return {"kind": "idle", "documents": [], "cursor": 0, "maxSeq": 0, "floor": 0}


class RuntimeStub:
    closed = True

    def __init__(self) -> None:
        self.transcript_responses: list[tuple[int | None, dict[str, object] | None]] = []
        self.transcript_revisions: list[int | None] = []

    def transcript(self, _agent_id: str) -> dict[str, object]:
        return {"items": [], "truncated": False}

    def wait_transcript(
        self, _agent_id: str, revision: int | None
    ) -> tuple[int | None, dict[str, object] | None]:
        self.transcript_revisions.append(revision)
        if not self.transcript_responses:
            self.closed = True
            return None, None
        response = self.transcript_responses.pop(0)
        if not self.transcript_responses:
            self.closed = True
        return response


class TokenRatesStub:
    def workspace_snapshot(self) -> dict[str, object]:
        return {"rates": {"agent-a": {"rate": 8.5}}, "teams": {}}


class ConnectedRequest(Request):
    async def is_disconnected(self) -> bool:
        return False


class ContextStub:
    def __init__(self) -> None:
        self.store = StoreStub()
        self.canvas = type("CanvasStub", (), {"root": "/nonexistent-canvas-root"})()
        self.runtime = RuntimeStub()
        self.runtime.closed = True
        self.store.runtime = self.runtime

    def sync(self) -> StoreStub:
        return self.store

    def send(self, request: Request, value: object, status: int = 200, **_kwargs: object) -> JSONResponse:
        route = cast(object, request.scope["route"])
        model = getattr(route, "response_model", None)
        selected: object = value
        if status >= 400:
            selected = ErrorResponse.model_validate(value).model_dump(mode="json", by_alias=True, exclude_unset=True)
        elif model is not None:
            validated = TypeAdapter(model).validate_python(value)
            if isinstance(validated, list):
                selected = [item.model_dump(mode="json", by_alias=True, exclude_unset=True) for item in validated]
            else:
                selected = validated.model_dump(mode="json", by_alias=True, exclude_unset=True)
        return JSONResponse(content=selected, status_code=status)


def make_client(context: ContextStub, raise_server_exceptions: bool = True) -> TestClient:
    app = FastAPI()
    app.include_router(create_router(cast(ApiContext, context)))

    @app.exception_handler(RequestValidationError)
    async def validation_error(_request: Request, error: RequestValidationError) -> JSONResponse:
        return JSONResponse({"error": str(error)}, status_code=400)

    return TestClient(app, raise_server_exceptions=raise_server_exceptions)


class SyncRouterTests(unittest.TestCase):
    def test_stream_openapi_declares_event_stream_without_json_success(self) -> None:
        app = FastAPI()
        app.include_router(create_router(cast(ApiContext, ContextStub())))
        install_error_response_docs(app)
        paths = app.openapi()["paths"]
        for path in ("/api/sync/stream", "/api/transcript/stream"):
            responses = paths[path]["get"]["responses"]
            if path == "/api/sync/stream":
                event_schema = responses["200"]["content"]["text/event-stream"]["schema"]
                self.assertEqual(
                    event_schema["oneOf"],
                    [
                        {"$ref": "#/components/schemas/ResourceChangeEvent"},
                        {"$ref": "#/components/schemas/ResourceHeartbeatEvent"},
                    ],
                )
                components = app.openapi()["components"]["schemas"]
                self.assertIn("ResourceRef", components)
                self.assertIn("ResourceChangeEvent", components)
                self.assertIn("ResourceHeartbeatEvent", components)
                self.assertIn("resources", event_schema["x-sse-events"])
            else:
                self.assertEqual(
                    responses["200"]["content"],
                    {"text/event-stream": {"schema": {"type": "string"}}},
                )
            self.assertIn("400", responses)
            self.assertIn("application/json", responses["400"]["content"])

    def read_stream(self, context: ContextStub, path: str) -> str:
        router = create_router(cast(ApiContext, context))
        route = cast(
            APIRoute,
            next(route for route in router.routes if getattr(route, "path", None) == path.split("?", 1)[0]),
        )
        scope: dict[str, object] = {
            "type": "http", "asgi": {"version": "3.0"}, "http_version": "1.1",
            "method": "GET", "scheme": "http", "path": path.split("?", 1)[0],
            "raw_path": path.split("?", 1)[0].encode(),
            "query_string": path.split("?", 1)[1].encode() if "?" in path else b"",
            "headers": [], "client": ("test", 1000), "server": ("test", 80),
        }
        request = ConnectedRequest(scope)

        async def read() -> bytes:
            if path.startswith("/api/sync/stream"):
                protocol = "2" if "protocol=2" in path else "1"
                response = await route.endpoint(
                    request, SyncStreamQuery(protocol=protocol, scope="drafts", after=4)
                )
            else:
                response = await route.endpoint(request, TranscriptStreamQuery(id="agent-a"))
            result = bytearray()
            async for chunk in response.body_iterator:
                result.extend(chunk)
            return bytes(result)

        return asyncio.run(read()).decode()

    def test_shared_stream_reads_token_rates_object_and_emits_snapshot(self) -> None:
        context = ContextStub()
        context.runtime.closed = False

        async def stop_after_first_iteration(_delay: float) -> None:
            context.runtime.closed = True

        with (
            patch("codex_token_rate.token_rates", return_value=TokenRatesStub()),
            patch("studio_api.sync.router.asyncio.sleep", new=stop_after_first_iteration),
        ):
            body = self.read_stream(context, "/api/sync/stream?protocol=2&scope=drafts&after=0")
        self.assertIn("event: token-rates", body)
        self.assertIn('"rates": {"agent-a": {"rate": 8.5}}', body)
        self.assertIn('"workspaceId": "workspace-a"', body)

    def test_pull_uses_legacy_first_nonempty_query_value(self) -> None:
        context = ContextStub()
        response = make_client(context).get("/api/sync/pull?scope=&scope=state&after=&after=9&limit=20")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["workspaceId"], "workspace-a")
        self.assertEqual(context.store.pull_arguments, ("state", 9, 20, False, 0, False, None))

    def test_pull_keeps_query_parameter_openapi_schema(self) -> None:
        app = FastAPI()
        app.include_router(create_router(cast(ApiContext, ContextStub())))
        parameters = app.openapi()["paths"]["/api/sync/pull"]["get"]["parameters"]
        self.assertEqual(
            [(parameter["name"], parameter["schema"]["anyOf"][0]["type"]) for parameter in parameters],
            [
                ("scope", "string"),
                ("after", "integer"),
                ("limit", "integer"),
                ("fresh", "string"),
                ("initialHigh", "integer"),
                ("reset", "string"),
                ("priorityId", "string"),
            ],
        )
        self.assertTrue(all(parameter["in"] == "query" and not parameter["required"] for parameter in parameters))

    def test_pull_keeps_invalid_integer_query_status(self) -> None:
        response = make_client(ContextStub()).get("/api/sync/pull?after=invalid")
        self.assertEqual(response.status_code, 400)

    def test_pull_keeps_status_for_invalid_first_repeated_integer(self) -> None:
        response = make_client(ContextStub(), raise_server_exceptions=False).get(
            "/api/sync/pull?after=invalid&after=9"
        )
        self.assertEqual(response.status_code, 500)

    def test_entity_pull_reset_has_its_own_complete_response_variant(self) -> None:
        context = ContextStub()
        context.store.reset_pull = True
        response = make_client(context).get("/api/sync/pull?scope=state:entities:v1&reset=1")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), {
            "workspaceId": "workspace-a", "reset": True, "floor": 4,
            "maxSeq": 8, "generation": 3,
        })

    def test_invalid_draft_batch_is_rejected_before_sync_service_or_write(self) -> None:
        context = ContextStub()
        client = make_client(context)
        valid_payload = json.dumps({
            "id": "device:lead", "device": "device", "session": "lead", "text": "draft",
        })
        invalid_batches = (
            {"rows": [{"newDocumentState": {"id": "device:lead", "payload": "{"}}]},
            {"rows": [{
                "newDocumentState": {"id": "device:lead", "payload": valid_payload},
                "assumedMasterState": {"id": "device:lead", "payload": "{"},
            }]},
            {"rows": [{
                "newDocumentState": {"id": "wrong", "payload": valid_payload},
            }]},
        )
        for body in invalid_batches:
            with self.subTest(body=body):
                response = client.post(
                    "/api/sync/drafts",
                    headers={"X-Canvas-Workspace": "workspace-a"}, json=body,
                )
                self.assertEqual(response.status_code, 400)
        self.assertEqual(context.store.push_calls, [])

    def test_valid_draft_payload_reaches_store_once_after_workspace_check(self) -> None:
        context = ContextStub()
        payload = json.dumps({"id": "device:lead", "device": "device", "session": "lead", "text": "draft"})
        response = make_client(context).post(
            "/api/sync/drafts",
            headers={"X-Canvas-Workspace": "workspace-a"},
            json={"rows": [{"newDocumentState": {
                "id": "device:lead", "payload": payload, "_deleted": False,
            }}]},
        )
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), [])
        self.assertEqual(len(context.store.push_calls), 1)

    def test_wrong_workspace_blocks_draft_mutation(self) -> None:
        context = ContextStub()
        response = make_client(context).post(
            "/api/sync/drafts", headers={"X-Canvas-Workspace": "another-workspace"}, json={"rows": []}
        )
        self.assertEqual(response.status_code, 409)
        self.assertEqual(response.json()["error"], "The server workspace changed. Reload before sending.")
        self.assertEqual(context.store.push_calls, [])

    def test_real_api_context_sender_validates_after_write_without_retry_hint(self) -> None:
        context = ApiContext.for_schema()
        store = StoreStub()
        store.invalid_push_response = True
        app = FastAPI()
        app.include_router(create_router(context))
        payload = json.dumps({"device": "device", "session": "lead", "text": "draft"})
        request = {
            "rows": [{"newDocumentState": {
                "id": "device:lead", "payload": payload, "_deleted": False,
            }}],
        }
        with patch.object(context, "sync", return_value=store):
            with TestClient(app, raise_server_exceptions=False) as client:
                response = client.post(
                    "/api/sync/drafts", json=request,
                    headers={"X-Canvas-Workspace": "workspace-a"},
                )
        self.assertEqual(len(store.push_calls), 1)
        self.assertEqual(response.status_code, 500)
        self.assertNotIn("outcome", response.json())
        self.assertEqual(response.json()["error"], "The server could not validate its response")

    def test_real_api_context_sender_accepts_typed_sync_success_responses(self) -> None:
        context = ApiContext.for_schema()
        store = StoreStub()
        app = FastAPI()
        app.include_router(create_router(context))
        with patch.object(context, "sync", return_value=store):
            with TestClient(app, raise_server_exceptions=False) as client:
                identity = client.get("/api/sync/identity")
                pull = client.get("/api/sync/pull?scope=state&after=0")
        self.assertEqual(identity.status_code, 200)
        self.assertEqual(identity.json(), {
            "workspaceId": "workspace-a", "syncProtocol": 2, "chatState": True,
        })
        self.assertEqual(pull.status_code, 200)
        self.assertEqual(pull.json()["generation"], 3)

    def test_protocol_one_stream_has_wire_headers_and_invalid_cursor_is_preflight(self) -> None:
        context = ContextStub()
        client = make_client(context)
        response = client.get("/api/sync/stream?protocol=1&scope=drafts&after=0")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.headers["x-codex-sync-protocol"], "1")
        bad = client.get("/api/sync/stream?protocol=1&scope=drafts&after=-1")
        self.assertEqual(bad.status_code, 400)
        self.assertEqual(context.store.stream_calls, 1)

    def test_protocol_one_stream_emits_changes_and_advances_reconnect_cursor(self) -> None:
        context = ContextStub()
        context.runtime.closed = False
        context.store.stream_batches = [
            {"kind": "changes", "documents": [], "cursor": 5, "maxSeq": 5, "floor": 0},
            {"kind": "idle", "documents": [], "cursor": 5, "maxSeq": 5, "floor": 0},
        ]
        body = self.read_stream(context, "/api/sync/stream?protocol=1&scope=drafts&after=4")
        self.assertIn("id: 5\nevent: changes", body)
        self.assertEqual(context.store.stream_calls, 2)
        self.assertEqual(context.store.stream_cursors, [4, 5])

    def test_protocol_one_change_identity_read_does_not_block_event_loop(self) -> None:
        context = ContextStub()
        context.runtime.closed = False
        context.store.stream_batches = [
            {"kind": "changes", "documents": [{"id": "doc", "payload": "{}"}],
             "cursor": 5, "maxSeq": 5, "floor": 0},
        ]
        identity_entered = threading.Event()
        identity_release = threading.Event()
        event_loop_progressed = threading.Event()

        def blocked_identity() -> dict[str, object]:
            identity_entered.set()
            if not identity_release.wait(2):
                raise TimeoutError("Identity fixture was not released")
            return {"workspaceId": "workspace-a", "syncProtocol": 2, "chatState": True}

        router = create_router(cast(ApiContext, context))
        route = cast(
            APIRoute,
            next(route for route in router.routes
                 if getattr(route, "path", None) == "/api/sync/stream"),
        )
        scope: dict[str, object] = {
            "type": "http", "asgi": {"version": "3.0"}, "http_version": "1.1",
            "method": "GET", "scheme": "http", "path": "/api/sync/stream",
            "raw_path": b"/api/sync/stream", "query_string": b"protocol=1&scope=drafts&after=4",
            "headers": [], "client": ("test", 1000), "server": ("test", 80),
        }
        request = ConnectedRequest(scope)

        async def read_one() -> bytes:
            response = await route.endpoint(
                request, SyncStreamQuery(protocol="1", scope="drafts", after=4)
            )

            async def prove_loop_progress() -> None:
                try:
                    self.assertTrue(await asyncio.to_thread(identity_entered.wait, 1))
                    asyncio.get_running_loop().call_soon_threadsafe(event_loop_progressed.set)
                    self.assertTrue(await asyncio.to_thread(event_loop_progressed.wait, 0.5))
                finally:
                    identity_release.set()

            progress = asyncio.create_task(prove_loop_progress())
            try:
                chunk = cast(bytes, await asyncio.wait_for(response.body_iterator.__anext__(), 1))
                await asyncio.wait_for(progress, 1)
                return chunk
            finally:
                identity_release.set()
                await response.body_iterator.aclose()

        with patch.object(context.store, "identity", side_effect=blocked_identity):
            chunk = asyncio.run(read_one())
        self.assertEqual(
            chunk.decode(),
            'id: 5\nevent: changes\ndata: '
            '{"protocolVersion":1,"workspaceId":"workspace-a","scope":"drafts",'
            '"documents":[{"id":"doc","payload":"{}"}],"cursor":5}\n\n',
        )
        self.assertTrue(event_loop_progressed.is_set())

    def test_protocol_one_stream_emits_reset_control_event(self) -> None:
        context = ContextStub()
        context.runtime.closed = False
        context.store.stream_batches = [
            {"kind": "reset", "reason": "floor-advanced", "floor": 7, "maxSeq": 9}
        ]
        body = self.read_stream(context, "/api/sync/stream?protocol=1&scope=drafts&after=0")
        self.assertIn("event: reset", body)
        self.assertIn('"reason": "floor-advanced"', body)

    def test_transcript_stream_preserves_replace_delta_and_order_updates(self) -> None:
        context = ContextStub()
        context.runtime.closed = False
        context.runtime.transcript_responses = [
            (1, {"items": [{"id": "b", "text": "hello", "kind": "message"},
                           {"id": "a", "text": "second", "kind": "message"}], "truncated": False}),
            (2, {"items": [{"id": "a", "text": "second", "kind": "message"},
                           {"id": "b", "text": "hello world", "kind": "message"}], "truncated": False}),
        ]
        body = self.read_stream(context, "/api/transcript/stream?id=agent-a")
        events = [json.loads(line.removeprefix("data: ")) for line in body.splitlines() if line.startswith("data: ")]
        self.assertEqual(len(events), 2)
        self.assertTrue(events[0]["replace"])
        self.assertEqual(events[0]["order"], ["b", "a"])
        self.assertEqual(events[1]["order"], ["a", "b"])
        self.assertEqual(events[1]["items"], [{"id": "b", "append": " world"}])
        self.assertEqual(context.runtime.transcript_revisions, [-1, 1])


if __name__ == "__main__":
    unittest.main()
