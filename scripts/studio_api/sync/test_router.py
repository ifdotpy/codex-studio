"""Isolated caller-level checks for sync and draft routes."""

from __future__ import annotations

import json
import asyncio
from pathlib import Path
from contextlib import contextmanager
import sqlite3
import threading
import tempfile
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
from studio_api.sync.models import SyncStreamQuery
from studio_api.responses import install_error_response_docs
from studio_api.schema import API_SCHEMA_HASH_PARAM, API_SCHEMA_MISMATCH_FIELD
from studio_api.sync.router import create_router
from studio_api.sync.resources.hub import ResourceHub
from studio_api.sync.resources.models import (
    ResourceRef,
    TokenRateSnapshot,
    TokenRateValue,
    TranscriptResource,
)
from codex_runtime import Runtime
from codex_sync import SyncStore
from codex_sync_entities import encoded, max_seq, put


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
        self.drafts_revision = 0
        self.draft_revision_increment = 1

    def identity(self) -> dict[str, object]:
        return {"workspaceId": "workspace-a", "syncProtocol": 2}

    def pull(self, *args: object) -> dict[str, object]:
        self.pull_arguments = args
        if args and args[0] in {"state", "state:chat"}:
            raise ValueError("Invalid sync scope")
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

    def draft_sequence(self) -> int:
        return self.drafts_revision

    def push_drafts(self, rows: list[dict[str, object]]) -> list[dict[str, object]]:
        self.push_calls.append(rows)
        self.drafts_revision += self.draft_revision_increment
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
        self.transcript_responses: list[dict[str, object]] = []
        self.transcript_reads = 0
        self.hub: ResourceHub | None = None
        self.transcript_close_after = 2
        self.transcript_error: Exception | None = None
        self.active_agents = {"agent-a"}

    @contextmanager
    def db(self):
        connection = sqlite3.connect(":memory:")
        try:
            yield connection
        finally:
            connection.close()

    def checked_actor(self, _db: sqlite3.Connection, agent_id: str) -> object:
        if agent_id not in self.active_agents:
            raise ValueError("Unknown managed agent")
        return {"id": agent_id}

    def transcript(self, _agent_id: str) -> dict[str, object]:
        self.transcript_reads += 1
        if self.transcript_error is not None:
            raise self.transcript_error
        response = self.transcript_responses.pop(0) if self.transcript_responses else {
            "items": [], "truncated": False,
        }
        if self.transcript_reads < self.transcript_close_after and self.hub is not None:
            self.hub.publish(ResourceRef(TranscriptResource(kind="transcript", agentId=_agent_id)))
        if self.transcript_reads >= self.transcript_close_after:
            self.closed = True
        return response


class TokenRatesStub:
    def workspace_snapshot(self) -> dict[str, object]:
        return {"rates": {"agent-a": {"rate": 8.5}}, "teams": {}}


class ProgressWatchdogStub:
    def __init__(self) -> None:
        self.agents: list[str] = []

    def subscribe(self, agent_id: str, _on_change: object):
        self.agents.append(agent_id)

        def detach() -> None:
            self.agents.remove(agent_id)

        return detach


class ConnectedRequest(Request):
    async def is_disconnected(self) -> bool:
        return False


class ContextStub:
    def __init__(self) -> None:
        self.api_schema_hash = "server-schema"
        self.store = StoreStub()
        self.canvas = type("CanvasStub", (), {"root": "/nonexistent-canvas-root"})()
        self.runtime = RuntimeStub()
        self.runtime.closed = True
        self.store.runtime = self.runtime
        self.watchdog = ProgressWatchdogStub()
        self.hub = ResourceHub(
            "workspace-a",
            progress_watchdog=self.watchdog,
            token_rates=TokenRateSnapshot(
                rates={"agent-a": TokenRateValue(
                    turnId="turn-a", active=True, estimated=False, rate=8.5, outputTokens=17,
                )},
                teams={},
            ),
        )
        self.runtime.hub = self.hub

    def sync(self) -> StoreStub:
        return self.store

    async def get_api_schema_hash(self) -> str:
        return self.api_schema_hash

    def peek_api_schema_hash(self) -> str:
        return self.api_schema_hash

    def start_api_schema_hash(self) -> None:
        return None

    def resource_hub(self) -> ResourceHub:
        return self.hub

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

    @app.exception_handler(ValueError)
    async def service_error(_request: Request, error: ValueError) -> JSONResponse:
        return JSONResponse({"error": str(error)}, status_code=400)

    return TestClient(app, raise_server_exceptions=raise_server_exceptions)


class SyncRouterTests(unittest.TestCase):
    def test_cancelled_event_stream_send_closes_progress_subscription(self) -> None:
        context = ContextStub()

        class ShutdownEventStub:
            def __init__(self) -> None:
                self.event = asyncio.Event()

            def async_event(self) -> asyncio.Event:
                return self.event

        async def verify() -> None:
            event = ShutdownEventStub()
            path = "/api/sync/stream?protocol=3&resources=" + json.dumps(
                [{"kind": "panel", "agentId": "agent-a"}], separators=(",", ":")
            )
            scope: dict[str, object] = {
                "type": "http",
                "asgi": {"version": "3.0", "spec_version": "2.3"},
                "http_version": "1.1",
                "method": "GET",
                "scheme": "http",
                "path": "/api/sync/stream",
                "raw_path": b"/api/sync/stream",
                "query_string": path.split("?", 1)[1].encode(),
                "headers": [],
                "client": ("test", 1000),
                "server": ("test", 80),
                "state": {"studio_shutdown_event": event},
            }
            request = ConnectedRequest(scope)
            router = create_router(cast(ApiContext, context))
            route = cast(
                APIRoute,
                next(
                    route
                    for route in router.routes
                    if getattr(route, "path", None) == "/api/sync/stream"
                ),
            )
            response = await route.endpoint(
                request,
                SyncStreamQuery(
                    protocol="3",
                    resources=request.query_params.get("resources"),
                ),
            )
            receive_started = False
            sent_types: list[str] = []

            async def receive() -> dict[str, object]:
                nonlocal receive_started
                if not receive_started:
                    receive_started = True
                    return {"type": "http.request", "body": b"", "more_body": False}
                await asyncio.Future()

            async def send(message: dict[str, object]) -> None:
                sent_types.append(str(message["type"]))
                if message["type"] == "http.response.body":
                    raise asyncio.CancelledError

            await response(scope, receive, send)
            self.assertIn("http.response.body", sent_types)
            self.assertEqual(context.watchdog.agents, [])
            self.assertEqual(context.hub._subscriptions, set())

        asyncio.run(verify())

    def test_entity_pull_retires_legacy_agent_alias_and_preserves_canonical_chat(self) -> None:
        context = ContextStub()
        legacy = json.dumps({
            "collection": "agent", "id": "chat-a",
            "value": {"id": "chat-a", "kind": "chat", "name": "Historical chat", "tail": "reply"},
        })
        limits = {"accountKey": "default", "at": 1.0, "processedAt": 2.0}
        workspace = json.dumps({
            "collection": "workspace", "id": "current",
            "value": {"rateLimits": limits, "rateLimitsByAccount": {"default": limits}},
        })
        canonical = json.dumps({"collection": "chat", "id": "chat-a", "value": {
            "id": "chat-a", "kind": "chat", "name": "Canonical chat", "members": ["lead-a"],
        }})
        documents = [
            {"id": "entity:chat:chat-a", "payload": canonical, "seq": 6, "_deleted": False},
            {"id": "entity:agent:chat-a", "payload": legacy, "seq": 7, "_deleted": False},
            {"id": "entity:workspace:current", "payload": workspace, "seq": 8, "_deleted": False},
        ]
        projection = {"workspaceId": "workspace-a", "documents": documents,
                      "checkpoint": {"seq": 8}, "maxSeq": 8, "initialHigh": 8}
        with patch.object(context.store, "pull", return_value=projection):
            response = make_client(context).get(
                "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1"
            )
        self.assertEqual(response.status_code, 200)
        body = response.json()
        self.assertEqual(body["checkpoint"], {"seq": 8})
        self.assertEqual(body["documents"][0], documents[0])
        self.assertEqual(body["documents"][0]["payload"], canonical)
        row = body["documents"][1]
        self.assertEqual(row["id"], "entity:agent:chat-a")
        self.assertEqual(row["seq"], 7)
        self.assertTrue(row["_deleted"])
        self.assertEqual(row["payload"], legacy)
        self.assertEqual(body["documents"][2]["payload"], workspace)

    def test_entity_pull_exposes_only_entity_fields_for_agent_rule_and_workspace(self) -> None:
        context = ContextStub()
        temporary = tempfile.TemporaryDirectory(prefix="sync-router-")
        self.addCleanup(temporary.cleanup)
        database = Path(temporary.name) / "sync.sqlite"

        @contextmanager
        def connect():
            db = sqlite3.connect(database, timeout=10)
            try:
                yield db
            finally:
                db.close()

        store = SyncStore(connect, context.runtime.transcript)
        store.pull("state:entities:v1", fresh=True)
        limits = {"accountKey": "default", "at": 1.0, "processedAt": 2.0, "checkedAt": 3.0,
                  "data": {"rateLimits": {"primary": {"usedPercent": 42}}}}
        agent = {
            "id": "agent-a", "kind": "agent", "name": "Worker", "accountHistory": [{"secret": "history"}],
            "deliveredMode": {"secret": "delivery"}, "nativeRelease": {"phase": "released", "targetEpoch": 4,
                                                                              "targetRootId": "private-root"},
            "startAttempt": {"claudeInputRequest": {"text": "private-input"}},
        }
        rule = {
            "id": "rule-a", "agent": "agent-a", "name": "Housekeeping",
            "enabled": True, "description": "Scheduled maintenance",
            "kind": "interval", "intervalSeconds": 30, "nextAt": 100.0,
            "at": 90.0, "path": "/private/path", "event": "file-change",
            "command": "private-command", "stallTimeoutSeconds": 1800,
            "livenessCommand": "private-liveness", "text": "private-text",
            "status": "completed", "checks": 4, "wakes": 2,
            "minimumWorkers": 3, "durationMinutes": 5,
            "error": "private-error", "lastExitCode": 0,
            "lastOutput": "private-output", "restartHoldNotified": {"epoch": 1},
            "lastFinished": 2.0, "activeWorkers": 1, "lastStallExitCode": 0,
            "stallProbe": False, "eventText": "private-event",
        }
        work = {
            "id": "work-a", "archive": {"status": "kept"},
            "archiveIntent": {"status": "pending", "token": "private-archive"},
            "releases": [{"agent": "agent-a", "token": "private-release"}],
        }
        with connect() as db:
            put(db, "agent", agent["id"], agent)
            put(db, "rule", rule["id"], rule)
            put(db, "work", work["id"], work)
            put(db, "workspace", "current", {
                "id": "current", "connected": True, "rateLimits": limits,
                "rateLimitsByAccount": {"default": limits},
            })
            db.commit()
        context.store = store
        response = make_client(context).get(
            "/api/sync/pull?scope=state:entities:v1&after=0&fresh=1&reset=1"
        )
        self.assertEqual(response.status_code, 200)
        entities = {
            row["id"]: json.loads(row["payload"])["value"]
            for row in response.json()["documents"]
        }
        self.assertEqual(entities["entity:agent:agent-a"], {
            "id": "agent-a", "kind": "agent", "name": "Worker",
            "nativeRelease": {"phase": "released"}, "startAttempt": {},
        })
        self.assertEqual(entities["entity:rule:rule-a"], {
            "id": "rule-a", "agent": "agent-a", "name": "Housekeeping",
            "enabled": True, "description": "Scheduled maintenance",
        })
        self.assertEqual(entities["entity:work:work-a"]["archive"], {"status": "kept"})
        self.assertNotIn("archiveIntent", entities["entity:work:work-a"])
        self.assertNotIn("releases", entities["entity:work:work-a"])
        self.assertEqual(entities["entity:workspace:current"]["rateLimits"], limits)
        self.assertEqual(entities["entity:workspace:current"]["rateLimitsByAccount"]["default"], limits)
        wire = json.dumps(response.json())
        for private in ("history", "delivery", "targetEpoch", "private-root", "private-input",
                        "private/path", "private-command", "private-liveness", "private-text",
                        "private-error", "private-output", "private-event"):
            self.assertNotIn(private, wire)
        self.assertNotIn("private-archive", wire)
        self.assertNotIn("private-release", wire)

    def test_entity_pull_skips_malformed_stored_rows_and_advances_checkpoint(self) -> None:
        context = ContextStub()
        temporary = tempfile.TemporaryDirectory(prefix="sync-router-")
        self.addCleanup(temporary.cleanup)
        database = Path(temporary.name) / "sync.sqlite"

        @contextmanager
        def connect():
            db = sqlite3.connect(database, timeout=10)
            try:
                yield db
            finally:
                db.close()

        store = SyncStore(connect, context.runtime.transcript)
        initial = store.pull("state:entities:v1", fresh=True)
        payloads = (
            ("agent", "before", {"id": "before", "kind": "agent", "name": "Before"}),
            ("agent", "after", {"id": "after", "kind": "agent", "name": "After"}),
        )
        with connect() as db:
            seq = max_seq(db)
            for collection, key, value in payloads:
                payload, digest, _ = encoded(collection, key, value)
                seq += 1
                db.execute("""INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                              VALUES (?,?,?,?,?,0)""",
                           (collection, key, seq, digest, payload))
            for key, raw in (("bad-json", "not-json"), ("wrong-root", '["agent"]'),
                             ("missing-id", '{"collection":"agent","value":{}}')):
                seq += 1
                db.execute("""INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                              VALUES ('agent',?,?,?, ?,0)""",
                           (key, seq, "malformed", raw))
            after, digest, _ = encoded("agent", "after-malformed", {
                "id": "after-malformed", "kind": "agent", "name": "After malformed",
            })
            seq += 1
            db.execute("""INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                          VALUES ('agent','after-malformed',?,?,?,0)""",
                       (seq, digest, after))
            db.commit()
        context.store = store
        with self.assertLogs("codex_sync_entities", level="WARNING") as captured:
            response = make_client(context).get(
                f"/api/sync/pull?scope=state:entities:v1&after={initial['checkpoint']['seq']}"
            )
        self.assertEqual(response.status_code, 200)
        body = response.json()
        ids = [document["id"] for document in body["documents"]]
        self.assertIn("entity:agent:before", ids)
        for malformed in ("bad-json", "wrong-root", "missing-id"):
            self.assertNotIn(f"entity:agent:{malformed}", ids)
        self.assertIn("entity:agent:after-malformed", ids)
        self.assertEqual(body["checkpoint"]["seq"], seq)
        self.assertEqual(len(captured.records), 3)

    def test_entity_pull_strips_extras_but_delivers_bad_dto_and_private_fields(self) -> None:
        context = ContextStub()
        temporary = tempfile.TemporaryDirectory(prefix="sync-router-")
        self.addCleanup(temporary.cleanup)
        database = Path(temporary.name) / "sync.sqlite"

        @contextmanager
        def connect():
            db = sqlite3.connect(database, timeout=10)
            try:
                yield db
            finally:
                db.close()

        store = SyncStore(connect, context.runtime.transcript)
        initial = store.pull("state:entities:v1", fresh=True)
        entries = [
            ("agent", "extra", {"id": "extra", "kind": "agent", "name": "Worker",
                                  "accountHistory": [{"secret": "agent-private"}]}, False),
            ("agent", "wrong-type", {"id": "wrong-type", "kind": "agent", "name": 42,
                                       "accountHistory": [{"secret": "wrong-private"}]}, False),
            ("work", "work-extra", {"id": "work-extra", "archive": {"status": "kept"},
                                     "archiveIntent": {"token": "work-private"}}, False),
            ("agent", "tombstone-extra", {"id": "tombstone-extra", "kind": "agent",
                                           "accountHistory": [{"secret": "tombstone-private"}]}, True),
        ]
        with connect() as db:
            seq = max_seq(db)
            for collection, key, value, deleted in entries:
                payload, digest, _ = encoded(collection, key, value)
                seq += 1
                db.execute("""INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                              VALUES (?,?,?,?,?,?)""",
                           (collection, key, seq, digest, payload, int(deleted)))
            db.commit()
        context.store = store
        with self.assertLogs("studio_api.sync.router", level="WARNING") as captured:
            response = make_client(context).get(
                f"/api/sync/pull?scope=state:entities:v1&after={initial['checkpoint']['seq']}"
            )
        self.assertEqual(response.status_code, 200)
        documents = {row["id"]: row for row in response.json()["documents"]}
        self.assertEqual(set(documents), {f"entity:{collection}:{key}" for collection, key, _, _ in entries})
        self.assertNotIn("accountHistory", json.loads(documents["entity:agent:extra"]["payload"])["value"])
        wrong_type = json.loads(documents["entity:agent:wrong-type"]["payload"])["value"]
        self.assertEqual(wrong_type["name"], 42)
        self.assertNotIn("accountHistory", wrong_type)
        work = json.loads(documents["entity:work:work-extra"]["payload"])["value"]
        self.assertEqual(work["archive"], {"status": "kept"})
        self.assertNotIn("archiveIntent", work)
        tombstone = documents["entity:agent:tombstone-extra"]
        self.assertTrue(tombstone["_deleted"])
        self.assertNotIn("accountHistory", json.loads(tombstone["payload"])["value"])
        self.assertEqual(response.json()["checkpoint"]["seq"], seq)
        log_text = "\n".join(captured.output)
        self.assertIn("accountHistory", log_text)
        self.assertIn("archiveIntent", log_text)
        for secret in ("agent-private", "wrong-private", "work-private", "tombstone-private"):
            self.assertNotIn(secret, log_text)
        self.assertNotIn("42", log_text)
        self.assertNotIn("agent-private", response.text)
        self.assertNotIn("wrong-private", response.text)
        self.assertNotIn("work-private", response.text)
        self.assertNotIn("tombstone-private", response.text)

    def test_entity_pull_fails_open_and_logs_invalid_public_data(self) -> None:
        context = ContextStub()
        payload = json.dumps({"collection": "agent", "id": "a", "value": {
            "id": "a", "kind": "agent", "name": "A", "unrecognized": "secret",
        }})
        later = json.dumps({"collection": "agent", "id": "b", "value": {
            "id": "b", "kind": "agent", "name": "B",
        }})
        projection = {"workspaceId": "workspace-a", "documents": [
            {"id": "entity:agent:a", "seq": 1, "_deleted": False, "payload": payload},
            {"id": "entity:agent:b", "seq": 2, "_deleted": False, "payload": later},
        ], "checkpoint": {"seq": 2}, "maxSeq": 2}
        with patch.object(context.store, "pull", return_value=projection), \
                self.assertLogs("studio_api.sync.router", level="WARNING") as logs:
            response = make_client(context).get("/api/sync/pull?scope=state:entities:v1")
        self.assertEqual(response.status_code, 200)
        rows = response.json()["documents"]
        self.assertEqual(len(rows), 2)
        self.assertNotIn("unrecognized", json.loads(rows[0]["payload"])["value"])
        self.assertEqual(rows[1]["payload"], later)
        self.assertFalse(rows[0]["_deleted"])
        self.assertIn("entity:agent:a", logs.output[0])
        self.assertIn("unrecognized", logs.output[0])
        self.assertNotIn("secret", logs.output[0])

    def test_stream_openapi_declares_protocol_three_event_stream(self) -> None:
        app = FastAPI()
        app.include_router(create_router(cast(ApiContext, ContextStub())))
        install_error_response_docs(app)
        paths = app.openapi()["paths"]
        responses = paths["/api/sync/stream"]["get"]["responses"]
        event_schema = responses["200"]["content"]["text/event-stream"]["schema"]
        self.assertEqual(
            event_schema["oneOf"],
            [
                {"$ref": "#/components/schemas/ResourceChangeEvent"},
                {"$ref": "#/components/schemas/ResourceHeartbeatEvent"},
                {"$ref": "#/components/schemas/ResourceTokenRatesEvent"},
            ],
        )
        components = app.openapi()["components"]["schemas"]
        self.assertIn("ResourceRef", components)
        self.assertIn("ResourceChangeEvent", components)
        self.assertIn("ResourceHeartbeatEvent", components)
        self.assertIn("ResourceTokenRatesEvent", components)
        self.assertIn("resources", event_schema["x-sse-events"])
        self.assertNotIn("/api/sync/generations", paths)
        self.assertNotIn("/api/transcript/stream", paths)

    def read_stream(
        self, context: ContextStub, path: str, headers: list[tuple[bytes, bytes]] | None = None
    ) -> str:
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
            "headers": headers or [], "client": ("test", 1000), "server": ("test", 80),
        }
        request = ConnectedRequest(scope)

        async def read() -> bytes:
            resources = request.query_params.get("resources")
            response = await route.endpoint(
                request,
                SyncStreamQuery(
                    protocol=request.query_params.get("protocol"),
                    resources=resources,
                ),
            )
            result = bytearray()
            async for chunk in response.body_iterator:
                result.extend(chunk)
            return bytes(result)

        return asyncio.run(read()).decode()

    def test_protocol_endpoint_advertises_only_version_three(self) -> None:
        response = make_client(ContextStub()).get("/api/sync/protocol")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["protocolVersion"], 3)
        self.assertEqual(response.json()["supportedVersions"], [3])

    def test_removed_routes_and_unsupported_stream_selectors_are_rejected(self) -> None:
        client = make_client(ContextStub())
        for path in ("/api/sync/generations", "/api/transcript/stream"):
            with self.subTest(path=path):
                self.assertEqual(client.get(path).status_code, 404)
        for selector in ("?protocol=1", "?protocol=2", ""):
            with self.subTest(selector=selector):
                response = client.get("/api/sync/stream" + selector)
                self.assertEqual(response.status_code, 426)
                self.assertEqual(response.json()["supportedVersions"], [3])

    def test_protocol_three_emits_typed_initial_resources_and_token_rates(self) -> None:
        context = ContextStub()
        resources = json.dumps([{"kind": "transcript", "agentId": "agent-a"}], separators=(",", ":"))
        body = self.read_stream(context, "/api/sync/stream?protocol=3&resources=" + resources)
        events = [line[7:] for line in body.splitlines() if line.startswith("event: ")]
        self.assertEqual(events[:2], ["resources", "token-rates"])
        self.assertNotIn("api-schema", events)
        self.assertIn("event: resources", body)
        self.assertIn('"reason":"initial"', body)
        self.assertIn('"kind":"transcript","agentId":"agent-a"', body)
        self.assertIn("event: token-rates", body)
        self.assertIn('"turnId":"turn-a"', body)
        self.assertIn('"epoch":"', body)

    def test_mismatching_schema_stream_sends_only_handshake_without_subscribing(self) -> None:
        context = ContextStub()
        with patch.object(context.hub, "subscribe", wraps=context.hub.subscribe) as subscribe:
            responses = [
                make_client(context).get(
                    f"/api/sync/stream?protocol=3&{API_SCHEMA_HASH_PARAM}={value}&resources=invalid"
                )
                for value in ("foreign", "%C3%A9", "")
            ]
        for response in responses:
            self.assertEqual(response.status_code, 200)
            self.assertTrue(response.headers["content-type"].startswith("text/event-stream"))
            body = response.text
            self.assertEqual(body.count("event:"), 1)
            self.assertIn("event: api-schema", body)
            payload = json.loads(body.split("data: ", 1)[1].split("\n", 1)[0])
            self.assertEqual(payload["hash"], context.api_schema_hash)
            self.assertIs(payload[API_SCHEMA_MISMATCH_FIELD], True)
        subscribe.assert_not_called()

    def test_matching_schema_stream_starts_with_handshake(self) -> None:
        context = ContextStub()
        with patch.object(context.hub, "subscribe", wraps=context.hub.subscribe) as subscribe:
            equal = self.read_stream(
                context,
                f"/api/sync/stream?protocol=3&{API_SCHEMA_HASH_PARAM}=server-schema&resources=%5B%7B%22kind%22%3A%22state%22%7D%5D",
            )
        events = [line[7:] for line in equal.splitlines() if line.startswith("event: ")]
        self.assertEqual(events[:3], ["api-schema", "resources", "token-rates"])
        self.assertEqual(subscribe.call_count, 1)

    def test_protocol_three_reconnect_baselines_every_subscribed_resource(self) -> None:
        context = ContextStub()
        resources = json.dumps([{"kind": "panel", "agentId": "agent-a"}], separators=(",", ":"))
        body = self.read_stream(
            context,
            "/api/sync/stream?protocol=3&resources=" + resources,
            headers=[(b"last-event-id", b"7")],
        )
        self.assertIn('"reason":"reconnect"', body)
        self.assertIn('"kind":"panel","agentId":"agent-a"', body)

    def test_protocol_three_entity_invalidation_keeps_tombstone_floor_reset_pull(self) -> None:
        context = ContextStub()
        resources = json.dumps([{"kind": "state"}], separators=(",", ":"))
        body = self.read_stream(context, "/api/sync/stream?protocol=3&resources=" + resources)
        self.assertIn('"kind":"state"', body)
        context.store.reset_pull = True
        response = make_client(context).get(
            "/api/sync/pull?scope=state:entities:v1&after=2&reset=1"
        )
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["floor"], 4)
        self.assertTrue(response.json()["reset"])

    def test_missing_panel_agent_does_not_block_or_watch_other_resources(self) -> None:
        context = ContextStub()
        resources = json.dumps([
            {"kind": "panel", "agentId": "deleted-agent"},
            {"kind": "state"},
        ], separators=(",", ":"))
        body = self.read_stream(context, "/api/sync/stream?protocol=3&resources=" + resources)
        self.assertIn('"kind":"panel","agentId":"deleted-agent"', body)
        self.assertIn('"kind":"state"', body)
        self.assertEqual(context.watchdog.agents, [])

    def test_pull_uses_first_nonempty_entity_scope_query_value(self) -> None:
        context = ContextStub()
        response = make_client(context).get("/api/sync/pull?scope=&scope=state:entities:v1&after=&after=9&limit=20")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["workspaceId"], "workspace-a")
        self.assertEqual(context.store.pull_arguments, ("state:entities:v1", 9, 20, False, 0, False, None))

    def test_pull_without_scope_defaults_to_entity_scope(self) -> None:
        context = ContextStub()
        response = make_client(context).get("/api/sync/pull")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(context.store.pull_arguments, ("state:entities:v1", 0, 100, False, 0, False, None))

    def test_legacy_pull_scopes_return_bad_request(self) -> None:
        client = make_client(ContextStub())
        for scope in ("state", "state:chat"):
            with self.subTest(scope=scope):
                response = client.get("/api/sync/pull?scope=" + scope)
                self.assertEqual(response.status_code, 400)
                self.assertEqual(response.json(), {"error": "Invalid sync scope"})

    def test_real_sync_store_returns_unchanged_transcript_for_full_pull(self) -> None:
        context = ContextStub()
        unchanged_transcript = {
            "items": [{"id": "item-a", "text": "unchanged chat"}],
            "title": "Existing chat",
        }
        context.runtime.transcript_responses = [unchanged_transcript, unchanged_transcript]
        database = Path(tempfile.mkdtemp()) / "sync.sqlite"

        @contextmanager
        def connect():
            db = sqlite3.connect(database, timeout=10)
            try:
                yield db
            finally:
                db.close()

        store = SyncStore(
            connect,
            transcript=context.runtime.transcript,
        )
        context.sync = lambda: store
        client = make_client(context)
        full = client.get("/api/sync/pull?scope=transcript:agent-a&after=0")
        self.assertEqual(full.status_code, 200)
        full_document = full.json()["documents"][0]
        self.assertIn("unchanged chat", full_document["payload"])
        checkpoint = full.json()["checkpoint"]["seq"]
        self.assertEqual(full_document["seq"], checkpoint)
        unchanged = client.get(f"/api/sync/pull?scope=transcript:agent-a&after={checkpoint}")
        self.assertEqual(unchanged.status_code, 200)
        self.assertEqual(unchanged.json()["documents"], [])
        self.assertEqual(unchanged.json()["checkpoint"]["seq"], checkpoint)

    def test_entity_pull_resets_when_client_checkpoint_exceeds_server_high(self) -> None:
        context = ContextStub()
        database = Path(tempfile.mkdtemp()) / "sync.sqlite"

        @contextmanager
        def connect():
            db = sqlite3.connect(database, timeout=10)
            try:
                yield db
            finally:
                db.close()

        store = SyncStore(
            connect,
            transcript=context.runtime.transcript,
        )
        with connect() as db:
            put(db, "workspace", "current", {"id": "current"})

        response = store.pull(
            "state:entities:v1", after=100, reset_support=True
        )
        self.assertTrue(response["reset"])
        self.assertEqual(response["maxSeq"], 1)

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
        self.assertEqual(response.status_code, 400)
        self.assertNotEqual(response.json(), {"error": "Invalid sync scope"})

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

    def test_noop_draft_write_does_not_publish_resource_change(self) -> None:
        context = ContextStub()
        context.store.draft_revision_increment = 0
        payload = json.dumps({"id": "device:lead", "device": "device", "session": "lead", "text": "draft"})
        response = make_client(context).post(
            "/api/sync/drafts",
            headers={"X-Canvas-Workspace": "workspace-a"},
            json={"rows": [{"newDocumentState": {
                "id": "device:lead", "payload": payload, "_deleted": False,
            }}]},
        )
        self.assertEqual(response.status_code, 200)
        self.assertEqual(context.hub._revision, 0)

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
                pull = client.get("/api/sync/pull?scope=state:entities:v1&after=0")
        self.assertEqual(identity.status_code, 200)
        self.assertEqual(identity.json(), {
            "workspaceId": "workspace-a", "syncProtocol": 2,
        })
        self.assertEqual(pull.status_code, 200)
        self.assertEqual(pull.json()["generation"], 3)

if __name__ == "__main__":
    unittest.main()
