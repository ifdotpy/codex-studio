"""HTTP contracts for transcript, search, branch, and checkpoint routes."""

from __future__ import annotations

from pathlib import Path
import sys
import unittest
from typing import TYPE_CHECKING, cast
from unittest.mock import patch

from fastapi import FastAPI, Request
from fastapi.testclient import TestClient
from starlette.responses import JSONResponse

SCRIPTS = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPTS))

from studio_api.history.router import create_router
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
                       "providerPayload": {"parts": ["hello", 3, None]}}],
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
        return {"id": "branch-1", "draft": {"text": "hello", "prefixText": ""}}

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

    def send(
        self, request: Request, value: object, status: int = 200, **kwargs: object
    ) -> JSONResponse:
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
        self.client = TestClient(self.app)

    def test_transcript_query_uses_first_value_and_preserves_pagination_contract(self) -> None:
        response = self.client.get(
            "/api/transcript/page?id=agent-1&limit=&limit=17&limit=4"
        )
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["historyVersion"], "thread:checkpoint")
        self.assertEqual(
            response.json()["items"][0]["providerPayload"],
            {"parts": ["hello", 3, None]},
        )
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
        self.assertEqual(
            self.context.runtime.calls[-1],
            (
                "branch",
                {"agent": "agent-1", "message_id": "m-1", "id": "request-1", "before": True},
            ),
        )


if __name__ == "__main__":
    unittest.main()
