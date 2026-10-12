"""Keep new main-branch operations reachable through the strict API boundary."""
from __future__ import annotations

import json
from types import SimpleNamespace
import unittest
from urllib.parse import quote
from unittest.mock import Mock

from fastapi.testclient import TestClient

from codex_project_folders import SidebarOrderConflict
from studio_api.app import create_app
from studio_api.context import ApiContext


class MainIntegrationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.runtime = SimpleNamespace(
            projects=Mock(), rename=Mock(), queue_action=Mock(), workspace_task_feed=Mock(),
            rules=Mock()
        )
        context = ApiContext.for_schema()
        context.canvas.runtime = self.runtime
        context.remote = SimpleNamespace(request_origin=lambda *_args: "http://testserver")
        self.client = TestClient(create_app(context), headers={"X-Canvas-Token": context.token})
        self.addCleanup(self.client.close)

    def test_sidebar_order_shape_receipt_and_conflict(self) -> None:
        body = {"action": "reorder", "request_id": "order-1", "expected_revision": 0,
                "groups": {"project": ["b", "a"]}, "migration": True}
        result = {"revision": 1, "groups": body["groups"]}
        self.runtime.projects.return_value = result
        response = self.client.post("/api/projects", json=body)
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), result)
        self.runtime.projects.assert_called_once_with(body)
        self.runtime.projects.side_effect = SidebarOrderConflict("Sidebar order changed")
        response = self.client.post("/api/projects", json=body)
        self.assertEqual(response.status_code, 409)
        self.assertEqual(response.json(), {"error": "Sidebar order changed"})

    def test_smart_rename_keeps_request_identity_and_receipt_states(self) -> None:
        body = {"id": "lead", "name": None, "request_id": "rename-1"}
        for receipt in (
            {"id": "lead", "request_id": "rename-1", "status": "pending"},
            {"id": "lead", "request_id": "rename-1", "status": "applied", "name": "Title"},
            {"id": "lead", "request_id": "rename-1", "status": "failed", "error": "Try again"},
        ):
            self.runtime.rename.return_value = receipt
            response = self.client.post("/api/rename", json=body)
            self.assertEqual(response.status_code, 200)
            self.assertEqual(response.json(), receipt)
            self.runtime.rename.assert_called_with("lead", None, "rename-1")

    def test_send_now_keeps_identity_and_queue_errors(self) -> None:
        body = {"agent": "lead", "action": "send_now", "request_id": "queue-1",
                "id": "message-1", "expected_revision": "r1", "expectedText": "Later"}
        result = {"items": [{"id": "message-1", "text": "Later", "kind": "user",
                             "status": "pending", "created": 1, "error": "Retry pending"}],
                  "revision": "r2", "capabilities": {"reorder": True, "receipts": True}}
        receipt = {"status": "updated", "revision": "r2", "capabilities": result["capabilities"]}
        self.runtime.queue_action.return_value = receipt
        response = self.client.post("/api/queue", json=body)
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), receipt)
        self.runtime.queue_action.assert_called_once_with("lead", body)
        self.runtime.queue_action.return_value = result
        response = self.client.get("/api/queue?agent=lead")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), result)

    def test_delivery_and_smart_rename_require_identity_before_dispatch(self) -> None:
        for body in (
            {"agent": "lead", "action": "send_now", "id": "message-1"},
            {"agent": "lead", "action": "send_now", "id": "message-1", "request_id": ""},
        ):
            response = self.client.post("/api/queue", json=body)
            self.assertEqual(response.status_code, 400)
        self.runtime.queue_action.assert_not_called()
        response = self.client.post("/api/rename", json={"id": "lead", "name": None})
        self.assertEqual(response.status_code, 400)
        self.runtime.rename.assert_not_called()

    def test_duplicate_excluded_account_keys_are_rejected_at_http_boundary(self) -> None:
        response = self.client.post(
            "/api/conversation",
            json={"id": "lead", "excluded_account_keys": ["second", "second"]},
        )
        self.assertEqual(response.status_code, 400)
        self.assertEqual(
            response.json(),
            {"error": "Invalid request", "location": ["body", "excluded_account_keys"]},
        )

    def test_sync_stream_rejects_invalid_resource_query_with_location(self) -> None:
        response = self.client.get("/api/sync/stream?protocol=3&resources=invalid")
        self.assertEqual(response.status_code, 400)
        self.assertEqual(
            response.json(),
            {"error": "Invalid request", "location": ["query", "resources"]},
        )

    def test_monitor_log_requires_id_with_query_location(self) -> None:
        response = self.client.get("/api/monitor/log")
        self.assertEqual(response.status_code, 400)
        self.assertEqual(
            response.json(),
            {"error": "Invalid request", "location": ["query", "id"]},
        )

    def test_task_feed_json_cursors_are_validated_without_echoing_input(self) -> None:
        cases = (
            ("cursor", {"id": "task-1", "updated": "private-marker"}),
            ("cursor", {"id": "task-1"}),
            ("cursor", {"id": "task-1", "updated": {"private-marker": 1}}),
            ("before", {"id": "task-1", "created": "private-marker"}),
            ("before", {"id": "task-1"}),
            ("before", {"id": "task-1", "created": {"private-marker": 1}}),
        )
        for parameter, value in cases:
            with self.subTest(parameter=parameter, value=value):
                encoded = quote(json.dumps(value), safe="")
                response = self.client.get(f"/api/workspace/tasks?{parameter}={encoded}")
                self.assertEqual(response.status_code, 400)
                self.assertEqual(
                    response.json(),
                    {"error": "Invalid request", "location": ["query", parameter]},
                )
                self.assertNotIn("private-marker", response.text)
        self.runtime.workspace_task_feed.assert_not_called()

    def test_empty_task_feed_cursor_round_trips(self) -> None:
        self.runtime.workspace_task_feed.return_value = {
            "tasks": [], "cursor": {"updated": 0, "id": ""}, "hasMore": False,
        }
        first = self.client.get("/api/workspace/tasks")
        self.assertEqual(first.status_code, 200, first.text)
        cursor = first.json()["cursor"]
        self.assertEqual(cursor, {"updated": 0, "id": ""})

        encoded_cursor = quote(json.dumps(cursor), safe="")
        second = self.client.get(f"/api/workspace/tasks?cursor={encoded_cursor}")
        self.assertEqual(second.status_code, 200, second.text)
        self.runtime.workspace_task_feed.assert_called_with(
            None, cursor={"updated": 0, "id": ""}, before=None, limit=100,
        )

    def test_receipt_json_query_errors_use_the_located_envelope(self) -> None:
        response = self.client.get("/api/messages/receipts?ids=private-marker")
        self.assertEqual(response.status_code, 400)
        self.assertEqual(
            response.json(),
            {"error": "Invalid request", "location": ["query", "ids"]},
        )
        self.assertNotIn("private-marker", response.text)

    def test_rule_kind_requirements_are_validated_at_the_http_boundary(self) -> None:
        invalid_bodies = (
            {"agent": "lead", "kind": "low_workers", "command": "echo"},
            {"agent": "lead", "kind": "file", "path": ""},
            {"agent": "lead", "kind": "once", "at": 4_102_444_800},
        )
        for body in invalid_bodies:
            with self.subTest(body=body):
                response = self.client.post("/api/rules", json=body)
                self.assertEqual(response.status_code, 400)
                self.assertEqual(response.json()["error"], "Invalid request")
                self.assertEqual(response.json()["location"][0], "body")
        self.runtime.rules.assert_not_called()

    def test_project_body_limit_rejects_before_dispatch(self) -> None:
        response = self.client.post("/api/projects", content=b" " * (6 * 1024 * 1024 + 1),
                                    headers={"Content-Type": "application/json"})
        self.assertEqual(response.status_code, 413)
        self.assertEqual(
            response.json(),
            {"error": "Invalid request size", "location": ["body"]},
        )
        self.runtime.projects.assert_not_called()

    def test_discover_optional_body_accepts_absent_and_object_rejects_null(self) -> None:
        result = {
            "accounts": [],
            "archivedAccounts": [],
            "defaultAccountKey": "default",
            "logins": [],
            "supportsDisconnect": False,
            "supportsDelete": False,
        }
        accounts = SimpleNamespace(
            discovered=True,
            snapshot=Mock(return_value=result),
            discover=Mock(return_value=result),
        )
        runtime = SimpleNamespace(accounts=accounts)
        context = ApiContext.for_schema()
        context.canvas.runtime = runtime
        context.remote = SimpleNamespace(request_origin=lambda *_args: "http://testserver")
        headers = {"X-Canvas-Token": context.token}
        with TestClient(create_app(context)) as client:
            absent = client.post("/api/accounts/discover", headers=headers)
            self.assertEqual(absent.status_code, 200, absent.text)
            self.assertEqual(absent.json(), result)

            empty_object = client.post(
                "/api/accounts/discover",
                content=b"{}",
                headers={**headers, "Content-Type": "application/json"},
            )
            self.assertEqual(empty_object.status_code, 200, empty_object.text)
            self.assertEqual(empty_object.json(), result)

            null_body = client.post(
                "/api/accounts/discover",
                content=b"null",
                headers={**headers, "Content-Type": "application/json"},
            )
            self.assertEqual(null_body.status_code, 400, null_body.text)
            self.assertEqual(
                null_body.json(),
                {"error": "JSON object required", "location": ["body"]},
            )

        self.assertEqual(accounts.discover.call_count, 2)
