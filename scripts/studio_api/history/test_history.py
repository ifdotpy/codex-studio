"""HTTP contracts for transcript, search, branch, and checkpoint routes."""

from __future__ import annotations

from pathlib import Path
import json
import sqlite3
import sys
import threading
from types import ModuleType
import unittest
from contextlib import contextmanager
from typing import TYPE_CHECKING, Callable, Iterator, cast
from unittest.mock import patch

from fastapi import APIRouter, FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from starlette.responses import JSONResponse

SCRIPTS = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPTS))

from studio_api.history.router import create_router
from studio_api.history.models import (
    CheckpointCaptureResponse,
    SearchItemResponse,
    SearchResponse,
    TranscriptAgent,
    TranscriptContextUsage,
    TranscriptInput,
    TranscriptItemResponse,
    TranscriptMessageRecord,
    TranscriptPageResponse,
    TranscriptRecord,
)
from studio_api.models import ErrorResponse, ResponseModel

if TYPE_CHECKING:
    from codex_canvas import Canvas
    from studio_api.context import ApiContext, HeaderCollection, RemoteAccessContract


class FakeRuntime:
    closed = False

    def __init__(self) -> None:
        self.calls: list[tuple[str, object]] = []
        self.lock = threading.RLock()
        self.branch_record: dict[str, object] = {}

    @contextmanager
    def db(self) -> Iterator[sqlite3.Connection]:
        db = sqlite3.connect(":memory:")
        try:
            yield db
        finally:
            db.close()

    def agent(self, _key: str, _db: sqlite3.Connection) -> dict[str, object]:
        return self.branch_record

    @staticmethod
    def empty_lead(_db: object, _record: dict[str, object]) -> bool:
        return False

    def agent_entity_view(
        self, db: sqlite3.Connection, record: dict[str, object]
    ) -> dict[str, object]:
        from codex_runtime import Runtime

        view = cast(Callable[..., dict[str, object]], Runtime.agent_entity_view)
        return view(self, db, record)

    def transcript(self, key: str | None, **kwargs: object) -> dict[str, object]:
        self.calls.append(("transcript", {"key": key, **kwargs}))
        return {
            "items": [{"id": "m-1", "role": "user", "title": "You", "text": "hello", "at": 1.0,
                       "inputs": [{"kind": "user", "text": "hello", "truncated": False,
                                   "assets": []}]}],
            "truncated": False,
            "nextCursor": None,
            "nextAfterCursor": None,
            "historyVersion": "thread:checkpoint",
            "unavailable": None,
            "agent": {"id": "agent-1", "status": "idle"},
        }

    def search_item(self, key: str | None) -> dict[str, object]:
        self.calls.append(("search_item", key))
        return {"id": "m-1", "kind": "message", "agent": "agent-1", "text": "hello"}

    def search_work(self, query: str | None, *, limit: str) -> dict[str, object]:
        self.calls.append(("search_work", {"query": query, "limit": limit}))
        return {"results": [], "query": query or "", "limit": int(limit)}

    def workspace_snapshot(self, agent: str | None) -> dict[str, object]:
        self.calls.append(("workspace_snapshot", agent))
        return {"checkpoints": [{"id": "cp-1", "label": "Saved", "created": 1.0}]}

    def branch_conversation(self, agent: str, value: dict[str, object]) -> dict[str, object]:
        self.calls.append(("branch", {"agent": agent, **value}))
        from codex_runtime import Runtime

        class EntityProducer:
            closed = False

            @staticmethod
            def empty_lead(_db: object, _agent: dict[str, object]) -> bool:
                return False

        record: dict[str, object] = {
            "id": "branch-1", "name": "Branch", "status": "idle", "isLead": True,
            "cwd": "/workspace", "model": "gpt-6-luna", "accountKey": "default",
            "threadId": "thread-branch", "providerSecret": "never-public",
        }
        agent_view = cast(Callable[..., dict[str, object]], Runtime.agent_entity_view)
        result = agent_view(EntityProducer(), None, record)
        result["draft"] = {"text": "hello", "prefixText": "", "assets": []}
        self.branch_record = result
        return result

    def checkpoint_capture(self, agent: str, label: str) -> dict[str, object]:
        self.calls.append(("checkpoint_capture", {"agent": agent, "label": label}))
        return {"id": "cp-2", "agent": agent, "label": label, "created": 2.0}

    @staticmethod
    def checkpoint_summary(value: dict[str, object]) -> dict[str, object]:
        return value

    def checkpoint_preview(self, agent: str, checkpoint: str) -> dict[str, object]:
        self.calls.append(("checkpoint_preview", {"agent": agent, "checkpoint": checkpoint}))
        return {
            "checkpoint": {"id": checkpoint, "agent": agent, "label": "Saved", "created": 1.0},
            "expectedTree": "tree-before",
            "diff": "",
            "patch": "",
            "truncated": False,
            "canRestore": True,
        }

    def restore_checkpoint(self, agent: str, value: dict[str, object]) -> dict[str, object]:
        self.calls.append(("checkpoint_restore", {"agent": agent, **value}))
        return {"status": "restored", "checkpoint": value.get("checkpoint")}


class FakeCanvas:
    def __init__(self) -> None:
        self.calls: list[str | None] = []

    def transcript(self, key: str | None) -> dict[str, object]:
        self.calls.append(key)
        return {
            "items": [{
                "id": "legacy-message", "role": "assistant", "title": "Assistant", "text": "legacy text",
                "at": "2026-10-04T00:00:00Z",
            }],
            "truncated": False,
            "unavailable": None,
            "tail": "legacy tail",
        }

    runtime: FakeRuntime

class FakeContext:
    def __init__(self) -> None:
        self.canvas = FakeCanvas()
        self.runtime = FakeRuntime()
        self.send_options: list[dict[str, object]] = []

    def send(
        self, request: Request, value: object, status: int = 200, **kwargs: object
    ) -> JSONResponse:
        self.send_options.append(kwargs)
        if status >= 400:
            value = ErrorResponse.model_validate(value).model_dump(
                mode="json", exclude_unset=True
            )
        else:
            route = request.scope.get("route")
            response_model = getattr(route, "response_model", None)
            if not isinstance(response_model, type) or not issubclass(response_model, ResponseModel):
                raise TypeError("JSON route must declare a ResponseModel")
            value = response_model.model_validate(value).model_dump(
                mode="json", exclude_unset=True
            )
        return JSONResponse(value, status_code=status)


class HistoryRouteTests(unittest.TestCase):
    def setUp(self) -> None:
        self.context = FakeContext()
        self.app = FastAPI()
        self.app.include_router(create_router(cast("ApiContext", self.context)))

        @self.app.exception_handler(RequestValidationError)
        async def request_error(_request: Request, _error: RequestValidationError) -> JSONResponse:
            return JSONResponse({"error": "Invalid request"}, status_code=400)

        self.client = TestClient(self.app)

    def test_transcript_query_uses_first_value_and_preserves_pagination_contract(self) -> None:
        response = self.client.get(
            "/api/transcript/page?id=agent-1&limit=&limit=17&limit=4"
        )
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["historyVersion"], "thread:checkpoint")
        self.assertEqual(response.json()["items"][0]["inputs"][0]["text"], "hello")
        self.assertEqual(
            self.context.runtime.calls[-1],
            ("transcript", {"key": "agent-1", "before": None, "around": None,
                             "after": None, "limit": "17"}),
        )
        parameters = self.app.openapi()["paths"]["/api/transcript/page"]["get"]["parameters"]
        self.assertEqual(
            {parameter["name"] for parameter in parameters},
            {"id", "before", "around", "after", "limit"},
        )
        self.assertEqual(
            {
                parameter["name"]
                for parameter in self.app.openapi()["paths"]["/api/search"]["get"]["parameters"]
            },
            {"q", "limit"},
        )
        limit_schema = next(
            parameter["schema"]
            for parameter in parameters
            if parameter["name"] == "limit"
        )
        self.assertEqual(limit_schema["anyOf"][0]["type"], "integer")
        self.assertIn("400", self.app.openapi()["paths"]["/api/transcript/page"]["get"]["responses"])
        self.assertIn("404", self.app.openapi()["paths"]["/api/transcript/page"]["get"]["responses"])

    def test_search_routes_keep_search_query_and_item_wire_shapes(self) -> None:
        search = self.client.get("/api/search?q=hello&limit=8")
        item = self.client.get("/api/search/item?id=m-1")
        self.assertEqual(search.json(), {"results": [], "query": "hello", "limit": 8})
        self.assertEqual(item.json(), {
            "id": "m-1", "kind": "message", "agent": "agent-1", "text": "hello",
        })

    def test_transcript_item_search_and_legacy_routes_call_their_services(self) -> None:
        item_value = {
            "id": "agent-1:event-1", "sourceId": "item-1", "agent": "agent-1",
            "role": "user", "text": "hello", "truncated": False,
        }
        search_value = {"results": [], "truncated": False}
        with (
            patch("codex_transcript_history.history_item", return_value=item_value) as item,
            patch("codex_transcript_history.search_history", return_value=search_value) as search,
        ):
            response = self.client.get(
                "/api/transcript/item?id=agent-1&message_id=agent-1%3Aevent-1"
            )
            searched = self.client.get("/api/transcript/search?id=agent-1&q=hello&limit=9")
        legacy = self.client.get("/api/transcript?id=legacy-1")
        self.assertEqual(response.json()["text"], "hello")
        self.assertEqual(searched.json(), search_value)
        item.assert_called_once_with(self.context.runtime, "agent-1", "agent-1:event-1")
        search.assert_called_once_with(self.context.runtime, "agent-1", "hello", "9")
        self.assertEqual(legacy.json()["tail"], "legacy tail")
        self.assertEqual(legacy.json()["items"][0]["at"], "2026-10-04T00:00:00Z")
        self.assertEqual(self.context.canvas.calls, ["legacy-1"])
        missing_id = self.client.get("/api/transcript")
        self.assertEqual(missing_id.status_code, 200)
        self.assertEqual(self.context.canvas.calls[-1], "")

    def test_checkpoint_reads_and_writes_use_explicit_responses(self) -> None:
        checkpoints = self.client.get("/api/checkpoints?agent=agent-1")
        self.assertTrue(self.context.send_options[-1]["etag"])
        capture = self.client.post("/api/checkpoint", json={"agent": "agent-1"})
        preview = self.client.post(
            "/api/checkpoint/preview", json={"agent": "agent-1", "checkpoint": "cp-1"}
        )
        restore = self.client.post(
            "/api/checkpoint/restore",
            json={"agent": "agent-1", "checkpoint": "cp-1", "expectedTree": "tree-before"},
        )
        self.assertEqual(checkpoints.json()["checkpoints"][0]["id"], "cp-1")
        self.assertEqual(capture.json()["label"], "Checkpoint")
        self.assertTrue(preview.json()["canRestore"])
        self.assertEqual(restore.json(), {"status": "restored", "checkpoint": "cp-1"})

    def test_invalid_branch_body_has_no_runtime_side_effect(self) -> None:
        for before in ("yes", None, 1):
            with self.subTest(before=before):
                response = self.client.post(
                    "/api/branch",
                    json={"agent": "agent-1", "message_id": "m-1", "id": "request-1", "before": before},
                )
                self.assertGreaterEqual(response.status_code, 400)
        self.assertFalse(any(name == "branch" for name, _ in self.context.runtime.calls))

    def test_branch_passes_stable_request_identity_without_rewriting_keys(self) -> None:
        response = self.client.post(
            "/api/branch",
            json={"agent": "agent-1", "message_id": "m-1", "id": "request-1", "before": True},
        )
        self.assertEqual(response.json()["draft"]["text"], "hello")
        self.assertEqual(response.json()["id"], "branch-1")
        self.assertNotIn("providerSecret", response.json())
        self.assertEqual(
            self.context.runtime.calls[-1],
            (
                "branch",
                {"agent": "agent-1", "message_id": "m-1", "id": "request-1", "before": True},
            ),
        )

    def test_numeric_query_limits_are_typed_and_invalid_values_fail_before_service(self) -> None:
        response = self.client.get("/api/search?q=hello&limit=8")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(
            self.context.runtime.calls[-1],
            ("search_work", {"query": "hello", "limit": "8"}),
        )
        call_count = len(self.context.runtime.calls)
        invalid = self.client.get("/api/search?q=hello&limit=not-a-number")
        self.assertEqual(invalid.status_code, 400)
        self.assertEqual(len(self.context.runtime.calls), call_count)

    def test_transcript_contract_accepts_runtime_item_producer_record(self) -> None:
        from codex_runtime import Runtime
        from codex_transcript_history import history_item
        from codex_work import WorkMixin

        connection = sqlite3.connect(":memory:")
        connection.row_factory = sqlite3.Row
        connection.executescript(
            "CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);"
            "CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY, body TEXT);"
            "CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY, record TEXT);"
            "CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, kind TEXT, status TEXT, "
            "created REAL, text TEXT, error TEXT);"
            "CREATE TABLE runtime_chat_messages(seq INTEGER PRIMARY KEY, id TEXT, room TEXT, "
            "sender TEXT, text TEXT, created REAL, deliveries TEXT);"
            "CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);"
            "CREATE TABLE analytics_items(id TEXT, agent TEXT, at REAL, type TEXT, record TEXT);"
            "INSERT INTO runtime_events VALUES ('event-1','agent-1','user','delivered',1.0,'hello',NULL);"
            "INSERT INTO runtime_event_meta VALUES ('event-1','{\"requestedDelivery\":\"after_turn\"}');"
            "INSERT INTO runtime_chat_messages VALUES (1,'room-message','room-1','agent-1',"
            "'hello room',2.0,'{}');"
        )

        class ItemProducer:
            def __init__(self) -> None:
                self.lock = threading.RLock()
                self.connection = connection
                self.current: dict[str, object] = {
                    "id": "agent-1", "status": "idle", "autoWake": False,
                    "inFlight": False, "turnId": None, "threadId": None,
                    "contextUsage": {"tokens": 25, "window": 4096, "at": 2.5},
                    "activity": {"phase": "thinking", "at": 2.5, "tools": []},
                    "compactions": 1, "compactionsObservedOnly": False,
                }

            @staticmethod
            def index_item(*_args: object) -> None:
                return None

            @staticmethod
            def touch_ui(_agent: str, _db: sqlite3.Connection | None = None) -> None:
                return None

            @contextmanager
            def db(self) -> Iterator[sqlite3.Connection]:
                yield self.connection

            @contextmanager
            def analytics_read_db(self) -> Iterator[sqlite3.Connection]:
                yield self.connection

            def agent(self, _agent: str, _db: sqlite3.Connection) -> dict[str, object]:
                return self.current

            @staticmethod
            def checked_actor(_db: sqlite3.Connection, _agent: str) -> dict[str, object]:
                return {"id": "agent-1", "deletedAt": None}

            @staticmethod
            def chat_rooms(_agent: str | None = None) -> list[dict[str, str]]:
                return [{"id": "room-1"}]

        producer = ItemProducer()
        item = cast(Callable[..., object], Runtime.item)
        asset = {
            "id": "asset-1", "agent": "agent-1", "name": "notes.txt",
            "mime": "text/plain", "image": False, "size": 5, "hash": "sha256:1",
            "created": 1.5,
        }
        item(
            producer, connection, "agent-1", "turn-1", "user", "hello",
            inputs=[{
                "id": "event-1", "created": 1.0, "kind": "user", "text": "hello",
                "assets": [asset],
            }],
            turnId="turn-9",
        )
        item_created = cast(
            float,
            connection.execute("SELECT created FROM runtime_items").fetchone()[0],
        )
        reasoning_record = {
            "id": "reasoning-1",
            "turnId": "turn-9",
            "startedAt": item_created,
            "finishedAt": item_created + 2.0,
        }
        connection.execute(
            "INSERT INTO analytics_items VALUES ('reasoning-1','agent-1',?,'reasoning',?)",
            (item_created, json.dumps(reasoning_record)),
        )
        record = connection.execute("SELECT record FROM runtime_items").fetchone()[0]
        parsed = TranscriptRecord.model_validate(json.loads(record))
        self.assertEqual(parsed.id, "agent-1:turn-1")
        self.assertEqual(parsed.role, "user")
        self.assertEqual(parsed.turnId, "turn-9")
        message_schema = TranscriptMessageRecord.model_json_schema()
        self.assertIn("role", message_schema["required"])
        self.assertNotIn("title", message_schema["required"])
        self.assertEqual(message_schema["properties"]["title"]["type"], "string")
        parsed_inputs = parsed.inputs
        assert parsed_inputs is not None
        parsed_assets = parsed_inputs[0].assets
        assert parsed_assets is not None
        self.assertEqual(parsed_assets[0].hash, "sha256:1")
        connection.commit()

        transcript = cast(Callable[..., dict[str, object]], Runtime.transcript)(
            producer, "agent-1", limit=120,
        )
        page = TranscriptPageResponse.model_validate(transcript)
        self.assertEqual([entry.role for entry in page.items], ["user", "reasoning"])
        reasoning_row = page.items[1].model_dump(mode="json", exclude_unset=True)
        self.assertNotIn("title", reasoning_row)
        input_row = cast(list[TranscriptInput], page.items[0].inputs)[0]
        self.assertEqual(input_row.clientMessageId, "event-1")
        self.assertEqual(input_row.deliveryStatus, "delivered")
        self.assertEqual(input_row.requestedDelivery, "after_turn")
        self.assertTrue(input_row.materialized)
        self.assertFalse(input_row.pending)
        transcript_agent = cast(TranscriptAgent, page.agent)
        usage = cast(TranscriptContextUsage, transcript_agent.contextUsage)
        self.assertEqual(usage.tokens, 25)

        history_service = cast(Callable[..., dict[str, object]], history_item)
        full_input = history_service(producer, "agent-1", "agent-1:event-1")
        full_message = TranscriptItemResponse.model_validate(full_input)
        self.assertEqual(full_message.role, "user")
        self.assertEqual(full_message.kind, "user")
        search_item = cast(Callable[..., dict[str, object]], WorkMixin.search_item)(
            producer, "agent-1:turn-1",
        )
        searched_message = SearchItemResponse.model_validate(search_item)
        self.assertEqual(searched_message.kind, "message")
        self.assertEqual(searched_message.role, "user")
        room_item = cast(Callable[..., dict[str, object]], WorkMixin.search_item)(
            producer, "room-message",
        )
        searched_room = SearchItemResponse.model_validate(room_item)
        self.assertEqual(searched_room.kind, "room")
        self.assertIsNone(searched_room.role)
        connection.close()

    def test_checkpoint_contract_accepts_capture_and_summary_producer_output(self) -> None:
        from codex_workspace import WorkspaceMixin

        class CheckpointProducer:
            def __init__(self) -> None:
                self.lock = threading.RLock()
                self.root = "/state"
                self.connection = sqlite3.connect(":memory:")
                self.connection.execute(
                    "CREATE TABLE runtime_items(agent TEXT, id TEXT, record TEXT, created REAL)"
                )
                self.connection.execute(
                    "INSERT INTO runtime_items VALUES ('agent-1','agent-1:item','{}',1.0)"
                )
                self.current = {
                    "id": "agent-1", "rootId": "root-1", "cwd": "/workspace",
                    "checkpointHistoryHead": None, "lastCompletedTurn": "turn-1",
                }

            @staticmethod
            def checked_actor_in_own_db(_agent: str) -> dict[str, object]:
                return {"id": "agent-1", "rootId": "root-1", "cwd": "/workspace"}

            @staticmethod
            def snapshot_tree(_actor: dict[str, object]) -> str:
                return "tree-1"

            @staticmethod
            def git(*_args: object) -> bytes:
                return b"commit-1"

            @contextmanager
            def db(self) -> Iterator[sqlite3.Connection]:
                yield self.connection

            def agent(self, _agent: str, _db: sqlite3.Connection) -> dict[str, object]:
                return cast(dict[str, object], self.current)

            @staticmethod
            def put(_db: sqlite3.Connection, _table: str, _record: dict[str, object]) -> None:
                return None

        producer = CheckpointProducer()
        capture = cast(Callable[..., object], WorkspaceMixin.capture_checkpoint)
        summarize = cast(Callable[..., dict[str, object]], WorkspaceMixin.checkpoint_summary)
        captured = cast(dict[str, object], capture(producer, "agent-1", "Saved", tree="tree-1"))
        summary = summarize(captured)
        parsed = CheckpointCaptureResponse.model_validate(summary)
        self.assertEqual(parsed.agent, "agent-1")
        self.assertEqual(parsed.historyBoundary, 1)
        self.assertNotIn("historyDelta", summary)
        producer.connection.close()

    def test_search_response_contract_accepts_work_search_producer_output(self) -> None:
        from codex_work import WorkMixin

        connection = sqlite3.connect(":memory:")
        connection.row_factory = sqlite3.Row
        connection.executescript(
            "CREATE TABLE runtime_items(id TEXT, record TEXT);"
            "CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED, agent UNINDEXED, kind UNINDEXED, body);"
            "CREATE TABLE runtime_chat_messages(id TEXT, room TEXT, sender TEXT, text TEXT, seq INTEGER);"
        )

        class SearchProducer:
            @contextmanager
            def read_db(self) -> Iterator[sqlite3.Connection]:
                yield connection

            @staticmethod
            def _search_phase(_db: sqlite3.Connection) -> str:
                return "idle"

            @staticmethod
            def records(_db: sqlite3.Connection, table: str) -> list[dict[str, object]]:
                if table == "agents":
                    return [{"id": "agent-1", "rootId": "agent-1"}]
                if table == "work":
                    return [{
                        "id": "task-1", "rootId": "agent-1", "title": "needle task",
                    }]
                return []

            @staticmethod
            def chat_rooms(_db: sqlite3.Connection, _agent: str | None = None) -> list[dict[str, str]]:
                return []

        produce = cast(Callable[..., dict[str, object]], WorkMixin.search_work)
        result = produce(SearchProducer(), "needle", limit=20)
        response = SearchResponse.model_validate(result)
        self.assertEqual(response.query, "needle")
        self.assertEqual(response.results[0].id, "task-1")
        self.assertEqual(response.results[0].agent, "agent-1")
        self.assertEqual(response.results[0].type, "work")
        self.assertEqual(response.results[0].text, "needle task")
        connection.close()

    def test_history_routes_through_core_context_sender_and_request_boundary(self) -> None:
        from studio_api.context import ApiContext

        class Remote:
            @staticmethod
            def origin() -> str:
                return "http://testserver"

            @staticmethod
            def request_origin(_headers: HeaderCollection, _peer: str, _port: int) -> str:
                return "http://testserver"

        class CanvasFixture:
            root = Path(".")

            def __init__(self) -> None:
                self.runtime = FakeRuntime()

        context = ApiContext(
            cast("Canvas", CanvasFixture()), token="history-test-token",
            remote=cast("RemoteAccessContract", Remote()),
            schema_only=True,
        )
        fake_modules: dict[str, ModuleType] = {}
        for domain in ("accounts", "agents", "federation", "insights", "io", "system", "voice", "work"):
            package_name = f"studio_api.{domain}"
            package = ModuleType(package_name)
            package.__path__ = []
            router_module = ModuleType(f"{package_name}.router")

            def empty_router(_context: object) -> APIRouter:
                return APIRouter()

            router_module.create_router = empty_router  # type: ignore[attr-defined]
            fake_modules[package_name] = package
            fake_modules[router_module.__name__] = router_module
        sync_router = ModuleType("studio_api.sync.router")
        sync_router.create_router = lambda _context: APIRouter()  # type: ignore[attr-defined]
        fake_modules[sync_router.__name__] = sync_router

        with patch.dict(sys.modules, fake_modules):
            from studio_api.app import create_app

            app = create_app(context)
            schema = cast(dict[str, object], app.openapi())
            paths = cast(dict[str, object], schema["paths"])
            transcript_path = cast(dict[str, object], paths["/api/transcript/page"])
            get_operation = cast(dict[str, object], transcript_path["get"])
            parameters = cast(list[dict[str, object]], get_operation["parameters"])
            limit_parameter = next(
                parameter for parameter in parameters
                if parameter["name"] == "limit"
            )
            limit_schema = cast(dict[str, object], limit_parameter["schema"])
            while "$ref" in limit_schema:
                reference = cast(str, limit_schema["$ref"])
                target: object = schema
                for part in reference.removeprefix("#/").split("/"):
                    target = cast(dict[str, object], target)[part]
                limit_schema = cast(dict[str, object], target)
            schema_types = [limit_schema.get("type")]
            schema_types.extend(
                branch.get("type")
                for branch in cast(list[dict[str, object]], limit_schema.get("anyOf", []))
            )
            self.assertIn("integer", schema_types)
            client = TestClient(app)
            auth = {"Origin": "http://testserver", "X-Canvas-Token": "history-test-token"}
            wrong_token = client.post(
                "/api/branch",
                json={"agent": "agent-1", "message_id": "m-1", "id": "request-1"},
                headers={**auth, "X-Canvas-Token": "wrong"},
            )
            self.assertEqual(wrong_token.status_code, 403)
            invalid = client.post(
                "/api/branch",
                json={"agent": "agent-1", "message_id": "m-1", "id": "request-1", "before": "yes"},
                headers=auth,
            )
            self.assertEqual(invalid.status_code, 400)
            self.assertEqual(invalid.json()["error"], "Invalid request")
            runtime = cast(FakeRuntime, context.runtime)
            self.assertFalse(any(call[0] == "branch" for call in runtime.calls))
            page = client.get("/api/transcript/page?id=agent-1", headers=auth)
            self.assertEqual(page.status_code, 200)
            self.assertEqual(page.json()["items"][0]["text"], "hello")
            first_checkpoint = client.get("/api/checkpoints?agent=agent-1", headers=auth)
            etag = first_checkpoint.headers["etag"]
            second_checkpoint = client.get(
                "/api/checkpoints?agent=agent-1",
                headers={**auth, "If-None-Match": etag},
            )
            self.assertEqual(second_checkpoint.status_code, 304)


if __name__ == "__main__":
    unittest.main()
