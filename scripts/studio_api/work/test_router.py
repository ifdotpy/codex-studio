"""Isolated route and schema checks for work and messaging APIs."""

from __future__ import annotations

import unittest
import sqlite3
import threading
from contextlib import nullcontext
from pathlib import Path
from types import SimpleNamespace
from typing import TYPE_CHECKING, cast
from unittest.mock import Mock, call, patch

from fastapi import FastAPI
from fastapi.exceptions import RequestValidationError
from fastapi.testclient import TestClient
from pydantic import BaseModel, TypeAdapter
from starlette.requests import Request
from starlette.responses import JSONResponse, Response

from studio_api.context import ApiContext, RemoteAccessContract
from studio_api.models import JsonValue
from studio_api.work.router import create_router

if TYPE_CHECKING:
    from codex_canvas import Canvas


class FakeContext:
    def __init__(self) -> None:
        self.runtime = SimpleNamespace()
        self.canvas = SimpleNamespace()
        self.sent_values: list[object] = []

    def send(self, request: Request, value: object, status: int = 200, **_: object) -> Response:
        self.sent_values.append(value)
        if status >= 400:
            return JSONResponse(value, status_code=status)
        route = request.scope["route"]
        response_model = route.response_model
        parsed = TypeAdapter(response_model).validate_python(value)
        if isinstance(parsed, BaseModel):
            content = cast(JsonValue, parsed.model_dump(mode="json", by_alias=True, exclude_unset=True))
        else:
            content = cast(JsonValue, parsed)
        return JSONResponse(content, status_code=status)


def make_client(context: FakeContext) -> TestClient:
    app = FastAPI()
    app.include_router(create_router(cast(ApiContext, context)))

    @app.exception_handler(RequestValidationError)
    def invalid_request(_: Request, __: RequestValidationError) -> JSONResponse:
        return JSONResponse({"error": "Invalid request"}, status_code=400)

    @app.exception_handler(ValueError)
    def invalid_operation(_: Request, __: ValueError) -> JSONResponse:
        return JSONResponse({"error": "Invalid request"}, status_code=400)

    return TestClient(app)


def make_real_sender_client(runtime: SimpleNamespace, canvas: SimpleNamespace | None = None) -> TestClient:
    services = canvas or SimpleNamespace(root=Path("."), runtime=runtime)
    services.runtime = runtime
    context = ApiContext(
        cast("Canvas", services), token="test-token",
        remote=cast(RemoteAccessContract, object()), schema_only=True,
    )
    app = FastAPI()
    app.include_router(create_router(context))
    return TestClient(app)


class WorkRouterTests(unittest.TestCase):
    def test_panel_layout_route_validates_current_and_legacy_feedback_and_preserves_http_errors(self) -> None:
        from codex_progress_layout import LayoutConflict

        body = {
            "agent": "agent-1", "revision": "revision-1", "client": "phone", "sequence": 1,
            "renderer": "progress-markdown-v1", "width": 320, "height": 150,
            "contentWidth": 100, "contentHeight": 40, "fits": True, "reason": None,
        }
        result = {
            "version": 1, "agent": "agent-1", "revision": "revision-1",
            "sha256": "a" * 64, "status": "fits", "reports": [{
                "client": "phone", "sequence": 1, "renderer": "progress-markdown-v1",
                "width": 320, "height": 150, "contentWidth": 100, "contentHeight": 40,
                "fits": True, "reason": None, "measuredAt": 1.0, "expiresAt": 91.0,
            }],
        }
        context = FakeContext()
        client = make_client(context)

        with patch("codex_progress_layout.record_layout", return_value=result) as record:
            response = client.post("/api/panel/layout", json=body)
        self.assertEqual(response.status_code, 200, response.text)
        self.assertNotIn("revision", response.json()["reports"][0])
        record.assert_called_once()

        legacy = {**result, "reports": [{**result["reports"][0], "revision": "revision-1"}]}
        with patch("codex_progress_layout.record_layout", return_value=legacy):
            response = client.post("/api/panel/layout", json=body)
        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["reports"][0]["revision"], "revision-1")

        with patch("codex_progress_layout.record_layout", side_effect=LayoutConflict("stale")):
            stale = client.post("/api/panel/layout", json=body)
        self.assertEqual(stale.status_code, 409, stale.text)

        invalid = client.post("/api/panel/layout", json={key: value for key, value in body.items() if key != "revision"})
        self.assertEqual(invalid.status_code, 400, invalid.text)

    def test_chat_query_documents_parameters_and_keeps_first_duplicate(self) -> None:
        context = FakeContext()
        context.runtime.chat_read = Mock(return_value={
            "room": {"id": "feed:team", "name": "Team", "members": []},
            "messages": [], "nextBefore": None, "nextAfter": None,
        })
        client = make_client(context)

        response = client.get("/api/agent-chat?room=feed%3Ateam&limit=5&before=11&before=12")

        self.assertEqual(response.status_code, 200, response.text)
        context.runtime.chat_read.assert_called_once_with("feed:team", before=11, after=None, limit=5)
        parameters = {row["name"] for row in client.get("/openapi.json").json()["paths"]["/api/agent-chat"]["get"]["parameters"]}
        self.assertTrue({"room", "before", "after", "limit"} <= parameters)

    def test_chat_query_skips_blank_duplicate_values_like_parse_qs(self) -> None:
        context = FakeContext()
        context.runtime.chat_read = Mock(return_value={
            "room": {"id": "feed:team", "name": "Team", "members": []},
            "messages": [], "nextBefore": None, "nextAfter": None,
        })
        response = make_client(context).get("/api/agent-chat?room=feed&before=&before=11&limit=&limit=5")

        self.assertEqual(response.status_code, 200, response.text)
        context.runtime.chat_read.assert_called_once_with("feed", before=11, after=None, limit=5)

    def test_invalid_first_duplicate_query_is_rejected_before_runtime_call(self) -> None:
        context = FakeContext()
        context.runtime.chat_read = Mock()
        response = make_client(context).get("/api/agent-chat?room=feed&before=secret&before=12")

        self.assertEqual(response.status_code, 400)
        self.assertNotIn("secret", response.text)
        context.runtime.chat_read.assert_not_called()

    def test_unknown_queue_body_field_fails_before_runtime_call(self) -> None:
        context = FakeContext()
        context.runtime.queue_action = Mock()
        response = make_client(context).post("/api/queue", json={
            "agent": "agent-1", "action": "edit", "id": "message-1", "text": "updated",
            "unreviewed": True,
        })

        self.assertEqual(response.status_code, 400)
        context.runtime.queue_action.assert_not_called()

    def test_tool_request_cancel_keeps_exact_identity_and_rejects_extra_fields(self) -> None:
        context = FakeContext()
        context.runtime.request_action = Mock(return_value={"id": "request-1", "stage": "cancelled"})
        client = make_client(context)

        response = client.post("/api/tool-requests/cancel", json={
            "agent": "agent-1", "request_id": "request-1",
        })
        rejected = client.post("/api/tool-requests/cancel", json={
            "agent": "agent-1", "request_id": "request-2", "retry": True,
        })

        self.assertEqual(response.status_code, 200, response.text)
        context.runtime.request_action.assert_called_once_with(
            "agent-1", {"action": "cancel", "request_id": "request-1"},
        )
        self.assertEqual(rejected.status_code, 400)
        self.assertEqual(context.runtime.request_action.call_count, 1)

    def test_work_and_queue_writes_forward_durable_receipt_ids(self) -> None:
        context = FakeContext()
        context.runtime.work_action = Mock(return_value={
            "id": "task-1", "rootId": "agent-1", "title": "Task", "status": "ready",
            "created": 1.0, "updated": 1.0, "version": 0,
        })
        context.runtime.queue_action = Mock(return_value={
            "status": "updated", "revision": "revision-2",
            "capabilities": {"reorder": True, "receipts": True},
        })
        client = make_client(context)

        work_body = {"agent": "agent-1", "action": "create", "id": "operation-1", "title": "Task"}
        queue_body = {
            "agent": "agent-1", "action": "edit", "id": "message-1",
            "request_id": "operation-2", "expected_revision": "revision-1",
            "expectedText": "before", "text": "after",
        }
        self.assertEqual(client.post("/api/work", json=work_body).status_code, 200)
        self.assertEqual(client.post("/api/queue", json=queue_body).status_code, 200)

        context.runtime.work_action.assert_called_once_with("agent-1", work_body, "operation-1")
        context.runtime.queue_action.assert_called_once_with("agent-1", queue_body)

    def test_receipt_ids_remain_strict_json_strings(self) -> None:
        context = FakeContext()
        context.runtime.user_delivery_receipts = Mock(return_value={"agent": "agent-1", "items": []})
        client = make_client(context)

        accepted = client.get('/api/messages/receipts?agent=agent-1&ids=%5B%22message-1%22%5D')
        rejected = client.get('/api/messages/receipts?agent=agent-1&ids=%5B%22message-1%22%2C1%5D')

        self.assertEqual(accepted.status_code, 200, accepted.text)
        context.runtime.user_delivery_receipts.assert_called_once_with("agent-1", ["message-1"])
        self.assertEqual(rejected.status_code, 400)
        self.assertEqual(context.runtime.user_delivery_receipts.call_count, 1)

    def test_queue_request_schema_is_discriminated_and_requires_action_fields(self) -> None:
        schema = make_client(FakeContext()).get("/openapi.json").json()
        request = schema["paths"]["/api/queue"]["post"]["requestBody"]["content"]["application/json"]["schema"]
        self.assertIn("oneOf", request)
        edit_ref = next(ref for ref in request["oneOf"] if ref["$ref"].endswith("QueueEditBody"))
        edit = schema["components"]["schemas"]["QueueEditBody"]
        self.assertEqual(edit_ref["$ref"], "#/components/schemas/QueueEditBody")
        self.assertTrue({"action", "text"} <= set(edit["required"]))
        self.assertEqual(edit["properties"]["action"]["const"], "edit")

    def test_queue_preserves_message_id_alias_and_validates_identity_before_runtime(self) -> None:
        context = FakeContext()
        context.runtime.queue_action = Mock(return_value={"status": "cancelled"})
        client = make_client(context)

        accepted = client.post("/api/queue", json={
            "agent": "agent-1", "action": "cancel", "message_id": "message-1",
        })
        rejected = client.post("/api/queue", json={"agent": "agent-1", "action": "cancel"})

        self.assertEqual(accepted.status_code, 200, accepted.text)
        context.runtime.queue_action.assert_called_once_with("agent-1", {
            "agent": "agent-1", "action": "cancel", "message_id": "message-1",
        })
        self.assertEqual(rejected.status_code, 400)
        self.assertEqual(context.runtime.queue_action.call_count, 1)

    def test_queue_assets_use_attachment_view_contract(self) -> None:
        context = FakeContext()
        asset = {
            "id": "asset-1", "agent": "agent-1", "name": "photo.png",
            "mime": "image/png", "image": True, "size": 12, "hash": "sha256",
            "created": 1.5,
        }
        context.runtime.queue_action = Mock(return_value={
            "items": [{
                "id": "message-1", "text": "", "kind": "user", "status": "pending",
                "created": 2.0, "assets": [asset], "delivery": "queue", "requestedDelivery": "queue",
            }],
            "revision": "revision-1", "capabilities": {"reorder": True, "receipts": True},
        })

        response = make_client(context).get("/api/queue?agent=agent-1")

        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["items"][0]["assets"], [asset])

    def test_workspace_task_feed_uses_required_task_fields_and_structured_cursors(self) -> None:
        context = FakeContext()
        task = {
            "id": "agent-1:item-1", "agent": "agent-1", "kind": "tool", "status": "completed",
            "created": 1.0, "finished": 2.0, "name": "web_search", "type": "webSearch",
            "itemId": "item-1", "startedAtMs": 1000, "completedAtMs": 2000,
        }
        context.runtime.workspace_task_feed = Mock(return_value={
            "tasks": [task], "cursor": {"updated": 2.0, "id": "agent-1:item-1"},
            "hasMore": True, "hasMoreChanges": False,
            "nextBefore": {"created": 1.0, "id": "agent-1:item-1"}, "reset": False,
        })

        response = make_client(context).get("/api/workspace/tasks")

        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json()["tasks"], [task])
        self.assertEqual(response.json()["cursor"], {"updated": 2.0, "id": "agent-1:item-1"})
        self.assertEqual(response.json()["nextBefore"], {"created": 1.0, "id": "agent-1:item-1"})

    def test_complaint_detail_has_typed_version_text_and_responses(self) -> None:
        context = FakeContext()
        complaint = {
            "id": "complaint-1", "leadId": "agent-1", "author": "user", "text": "Help",
            "version": 1, "status": "open", "created": 1.0, "updated": 2.0,
            "responses": [{
                "id": "response-1", "author": "agent-1", "text": "On it",
                "status": "in_progress", "at": 2.0,
            }],
        }
        context.runtime.complaint_detail = Mock(return_value=complaint)

        response = make_client(context).get("/api/complaint?id=complaint-1")

        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json(), complaint)

    def test_plan_response_preserves_native_report_separately_from_legacy_text(self) -> None:
        context = FakeContext()
        legacy_plan = {
            "id": "agent-1", "rootId": "agent-1", "text": "Saved editor text",
            "version": 1, "updated": 2.0, "steps": [],
        }
        native_plan = {
            **legacy_plan,
            "native": {
                "threadId": "thread-1", "turnId": "turn-1",
                "explanation": "Review the evidence.",
                "plan": [{"step": "Inspect evidence", "status": "completed"}],
            },
        }
        context.runtime.plan_action = Mock(side_effect=[legacy_plan, native_plan])
        client = make_client(context)

        legacy_response = client.get("/api/plan?agent=agent-1")
        native_response = client.get("/api/plan?agent=agent-1")

        self.assertEqual(legacy_response.status_code, 200, legacy_response.text)
        self.assertEqual(legacy_response.json()["text"], "Saved editor text")
        self.assertNotIn("native", legacy_response.json())
        self.assertEqual(native_response.status_code, 200, native_response.text)
        self.assertEqual(native_response.json()["text"], "Saved editor text")
        self.assertEqual(native_response.json()["native"], native_plan["native"])
        context.runtime.plan_action.assert_has_calls([call("agent-1"), call("agent-1")])

    def test_post_work_list_action_preserves_legacy_list_response(self) -> None:
        context = FakeContext()
        context.runtime.work_action = Mock(return_value={"items": [], "tasks": []})
        response = make_client(context).post("/api/work", json={
            "agent": "agent-1", "action": "list",
        })

        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(response.json(), {"items": [], "tasks": []})
        context.runtime.work_action.assert_called_once_with(
            "agent-1", {"agent": "agent-1", "action": "list"}, None,
        )

    def test_real_context_sender_validates_work_union_and_root_array(self) -> None:
        runtime = SimpleNamespace(work_action=Mock(return_value={"items": [], "tasks": []}))
        canvas = SimpleNamespace(
            root=Path("."), runtime=runtime,
            messages=Mock(return_value=[]),
        )
        client = make_real_sender_client(runtime, canvas)

        work = client.post("/api/work", json={"agent": "agent-1", "action": "list"})
        messages = client.get("/api/messages?room=chat-1")

        self.assertEqual(work.status_code, 200, work.text)
        self.assertEqual(work.json(), {"items": [], "tasks": []})
        self.assertEqual(messages.status_code, 200, messages.text)
        self.assertEqual(messages.json(), [])

    def test_profile_and_rule_editor_payload_fields_are_preserved(self) -> None:
        context = FakeContext()
        context.runtime.profiles = Mock(return_value={"id": "profile-1", "deleted": None})
        context.runtime.rules = Mock(return_value={"id": "rule-1", "agent": "agent-1"})
        client = make_client(context)

        profile_response = client.post("/api/profiles", json={
            "id": "profile-1", "isNew": True, "name": "Builder",
            "role": "implementer", "model": "model-x", "effort": "high",
            "instructions": "Do the work.", "action": "save",
        })
        rule_response = client.post("/api/rules", json={
            "id": "rule-1", "isNew": True, "agent": "agent-1", "name": "Check",
            "kind": "event", "event": "worker_completed", "text": "Continue.",
            "command": "", "action": "save",
        })

        self.assertEqual(profile_response.status_code, 200, profile_response.text)
        self.assertEqual(rule_response.status_code, 200, rule_response.text)
        profile_payload = context.runtime.profiles.call_args.args[0]
        self.assertTrue(profile_payload["isNew"])
        rule_payload = context.runtime.rules.call_args.args[0]
        self.assertTrue(rule_payload["isNew"])
        self.assertEqual(rule_payload["text"], "Continue.")

    def test_runtime_question_changes_and_rule_projection_fields_are_typed(self) -> None:
        context = FakeContext()
        context.runtime.question_history = Mock(return_value={"items": [{
            "id": "request-1", "agent": "agent-1", "method": "agent/asyncQuestion",
            "status": "pending", "deferredAt": 2, "deferredBy": "user", "questions": [],
        }]})
        context.runtime.changes = Mock(return_value={
            "scope": "chat", "files": [], "diff": "", "patch": "", "truncated": False,
            "revision": None, "git": True, "turnId": "turn-1", "reportedAt": 1.0,
        })
        context.runtime.rules = Mock(return_value={"rules": [{
            "id": "rule-1", "agent": "agent-1", "rootId": "agent-1", "epoch": 2,
            "name": "Check", "description": "Rule description", "kind": "event",
            "status": "active", "created": 1.0,
            "intervalSeconds": 60, "nextAt": 2.0, "event": "worker_completed",
            "command": "", "text": "Continue.", "inFlight": False, "checks": 3,
            "wakes": 1, "lastOutput": "ok", "lastExitCode": 0, "lastFinished": 2.0,
            "error": "Previous check failed", "fingerprint": [3, 12, 5],
            "restartCheck": {"epoch": 2, "checks": 3, "monitorId": "monitor-1"},
        }]})
        client = make_client(context)

        self.assertEqual(client.get("/api/questions?agent=agent-1").status_code, 200)
        self.assertEqual(client.get("/api/changes?agent=agent-1&scope=chat").json()["turnId"], "turn-1")
        rule_response = client.get("/api/rules")
        self.assertEqual(rule_response.status_code, 200, rule_response.text)
        self.assertEqual(rule_response.json()["rules"][0]["lastOutput"], "ok")
        self.assertEqual(rule_response.json()["rules"][0]["description"], "Rule description")
        self.assertEqual(rule_response.json()["rules"][0]["error"], "Previous check failed")
        self.assertEqual(rule_response.json()["rules"][0]["restartCheck"]["monitorId"], "monitor-1")

    def test_message_post_preserves_canvas_receipt_and_request_identity(self) -> None:
        context = FakeContext()
        context.runtime.federation = Mock(return_value=SimpleNamespace(has_room=lambda _: False))
        context.runtime.db = lambda: nullcontext(SimpleNamespace(
            execute=lambda *_: SimpleNamespace(fetchone=lambda: None),
        ))
        context.canvas.post = Mock(return_value={
            "id": "5ff6719c-18ec-4a53-8abc-d1948652574e", "room": "chat-1", "author": "user",
            "text": "hello", "at": 1.0, "deliveries": {}, "status": "delivered",
        })

        response = make_client(context).post("/api/messages", json={
            "id": "5ff6719c-18ec-4a53-8abc-d1948652574e", "room": "chat-1", "text": "hello",
        })

        self.assertEqual(response.status_code, 200, response.text)
        context.canvas.post.assert_called_once_with("chat-1", "hello", "5ff6719c-18ec-4a53-8abc-d1948652574e")
        self.assertEqual(response.json()["id"], "5ff6719c-18ec-4a53-8abc-d1948652574e")

    def test_managed_message_keeps_federation_branch_and_canvas_mirror(self) -> None:
        message_id = "5ff6719c-18ec-4a53-8abc-d1948652574e"
        context = FakeContext()
        runtime_db = sqlite3.connect(":memory:", check_same_thread=False)
        runtime_db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY)")
        runtime_db.execute("INSERT INTO runtime_agents VALUES (?)", ("agent-1",))
        runtime_db.commit()
        context.runtime.db = lambda: runtime_db
        context.runtime.send = Mock(return_value={"id": message_id, "status": "queued"})
        federation = SimpleNamespace(has_room=lambda _: False)
        context.runtime.federation = lambda: federation
        canvas_db = sqlite3.connect(":memory:", check_same_thread=False)
        canvas_db.execute("CREATE TABLE messages(id, room, author, text, at, deliveries)")
        canvas_db.commit()
        context.canvas.lock = threading.RLock()
        context.canvas.connect = lambda: canvas_db

        response = make_client(context).post("/api/messages", json={
            "id": message_id, "room": "agent-1", "text": "  hello  ",
        })

        self.assertEqual(response.status_code, 200, response.text)
        context.runtime.send.assert_called_once_with(
            "agent-1", "  hello  ", message_id, delivery="queue", assets=[],
        )
        mirror = canvas_db.execute("SELECT id,room,author,text FROM messages").fetchone()
        self.assertEqual(mirror, (message_id, "agent-1", "user", "hello"))
        runtime_db.close()
        canvas_db.close()

    def test_federated_user_message_keeps_existing_service_path(self) -> None:
        message_id = "5ff6719c-18ec-4a53-8abc-d1948652574e"
        context = FakeContext()
        federation = SimpleNamespace(
            has_room=lambda room: room == "federated:room-1",
            user_message=Mock(return_value={"status": "queued"}),
        )
        context.runtime.federation = lambda: federation
        context.runtime.db = Mock()

        response = make_client(context).post("/api/messages", json={
            "id": message_id, "room": "federated:room-1", "text": "hello",
        })

        self.assertEqual(response.status_code, 200, response.text)
        federation.user_message.assert_called_once_with("federated:room-1", "hello", message_id)
        self.assertEqual(response.json()["id"], message_id)

    def test_workspace_projection_and_legacy_array_have_explicit_schemas(self) -> None:
        context = FakeContext()
        context.runtime.workspace_part = Mock(return_value={"inbox": []})
        context.canvas.messages = Mock(return_value=[])
        client = make_client(context)

        self.assertEqual(client.get("/api/workspace?view=inbox").json(), {"inbox": []})
        self.assertEqual(client.get("/api/messages?room=chat-1").json(), [])
        schema = client.get("/openapi.json").json()
        workspace_response = schema["paths"]["/api/workspace"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
        message_response = schema["paths"]["/api/messages"]["get"]["responses"]["200"]["content"]["application/json"]["schema"]
        message_parameters = {row["name"] for row in schema["paths"]["/api/messages"]["get"]["parameters"]}
        self.assertIn("WorkspaceView", str(workspace_response))
        message_model = schema["components"]["schemas"]["MessageHistory"]
        self.assertEqual(message_model.get("type"), "array")
        self.assertIn("MessageHistory", str(message_response))
        self.assertEqual(message_parameters, {"room"})


if __name__ == "__main__":
    unittest.main()
