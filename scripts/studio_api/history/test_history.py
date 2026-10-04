"""HTTP contracts for transcript, search, branch, and checkpoint routes."""

from __future__ import annotations

from pathlib import Path
import json
import sqlite3
import sys
import threading
import unittest
from contextlib import contextmanager
from typing import TYPE_CHECKING, Callable, Iterator, cast
from unittest.mock import patch

from fastapi import FastAPI, Request
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from starlette.responses import JSONResponse

SCRIPTS = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPTS))

from studio_api.history.router import create_router
from studio_api.history.models import CheckpointCaptureResponse, TranscriptRecord
from studio_api.models import ErrorResponse, ResponseModel

if TYPE_CHECKING:
    from studio_api.context import ApiContext


class FakeRuntime:
    def __init__(self) -> None:
        self.calls: list[tuple[str, object]] = []

    def transcript(self, key: str | None, **kwargs: object) -> dict[str, object]:
        self.calls.append(("transcript", {"key": key, **kwargs}))
        return {
            "items": [{"id": "m-1", "role": "user", "text": "hello", "at": 1.0,
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
                "id": "legacy-message", "role": "assistant", "text": "legacy text",
                "at": "2026-10-04T00:00:00Z",
            }],
            "truncated": False,
            "unavailable": None,
            "tail": "legacy tail",
        }

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

    def test_transcript_contract_accepts_runtime_item_producer_record(self) -> None:
        from codex_runtime import Runtime

        connection = sqlite3.connect(":memory:")
        connection.executescript(
            "CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);"
            "CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY, body TEXT);"
            "CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY, record TEXT);"
            "CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT);"
        )

        class ItemProducer:
            @staticmethod
            def index_item(*_args: object) -> None:
                return None

            @staticmethod
            def touch_ui(_agent: str) -> None:
                return None

        producer = ItemProducer()
        item = cast(Callable[..., object], Runtime.item)
        item(producer, connection, "agent-1", "turn-1", "user", "hello", turnId="turn-9")
        record = connection.execute("SELECT record FROM runtime_items").fetchone()[0]
        parsed = TranscriptRecord.model_validate(json.loads(record))
        self.assertEqual(parsed.id, "agent-1:turn-1")
        self.assertEqual(parsed.role, "user")
        self.assertEqual(parsed.turnId, "turn-9")
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


if __name__ == "__main__":
    unittest.main()
