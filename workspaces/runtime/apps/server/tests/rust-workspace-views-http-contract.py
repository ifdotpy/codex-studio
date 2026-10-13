#!/usr/bin/env python3
"""HTTP contracts for persisted task details and workspace task feeds."""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlencode
import unittest
from unittest.mock import patch

SERVER_SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SERVER_SRC))

from http_backend_harness import TimedHTTPContractTest, start_contract_backend
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()


class WorkspaceViewsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        subprocess.run(["git", "init", "-q"], cwd=cls.backend.cwd, check=True)
        subprocess.run(["git", "config", "user.name", "Workspace Views Test"], cwd=cls.backend.cwd, check=True)
        subprocess.run(["git", "config", "user.email", "workspace-views@example.invalid"], cwd=cls.backend.cwd, check=True)
        (cls.backend.cwd / "tracked.txt").write_text("committed\n", encoding="utf-8")
        subprocess.run(["git", "add", "tracked.txt"], cwd=cls.backend.cwd, check=True)
        subprocess.run(["git", "commit", "-qm", "baseline"], cwd=cls.backend.cwd, check=True)
        (cls.backend.cwd / "tracked.txt").write_text("working tree change\n", encoding="utf-8")
        from codex_runtime import Runtime

        environment = cls.backend._environment()
        with patch.dict(os.environ, environment, clear=True):
            runtime = Runtime(cls.backend.state)
            try:
                lead = runtime.create(
                    {"name": "Workspace views lead", "cwd": str(cls.backend.cwd), "prompt": ""},
                    draft=True,
                    defer=True,
                )
                worker = runtime.create(
                    {"name": "Workspace views worker", "role": "reviewer", "prompt": "Inspect the fixture tasks."},
                    lead["id"],
                    defer=True,
                )
                other = runtime.create(
                    {"name": "Workspace views other", "cwd": str(cls.backend.cwd), "prompt": ""},
                    draft=True,
                    defer=True,
                )
                with runtime.lock, runtime.db() as db:
                    for number in range(105):
                        owner = worker["id"] if number % 2 else lead["id"]
                        runtime.put(db, "tasks", {
                            "id": f"workspace-task-{number:03}",
                            "agent": owner,
                            "kind": "tool",
                            "status": "completed",
                            "created": float(number + 1),
                            "finished": float(number + 2),
                            "name": "fixture tool",
                            "type": "webSearch",
                            "itemId": f"item-{number}",
                            "tail": "private tail",
                            "arguments": "private arguments",
                            "error": "private error",
                        })
                    runtime.put(db, "tasks", {
                        "id": "workspace-views-other-task",
                        "agent": other["id"],
                        "kind": "command",
                        "status": "completed",
                        "created": 500.0,
                    })
                    runtime.put(db, "work", {
                        "id": "workspace-view-work",
                        "rootId": lead["id"],
                        "title": "Review workspace state",
                        "status": "ready",
                        "created": 1.0,
                        "updated": 1.0,
                        "version": 1,
                        "dependencies": [],
                    })
                    runtime.put(db, "annotations", {
                        "id": "workspace-view-annotation",
                        "agent": lead["id"],
                        "rootId": lead["id"],
                        "path": "tracked.txt",
                        "line": 1,
                        "text": "Review this line",
                        "created": 1.0,
                    })
                    runtime.put(db, "requests", {
                        "id": "workspace-view-request",
                        "agent": lead["id"],
                        "method": "agent/asyncQuestion",
                        "status": "pending",
                        "createdAt": 1.0,
                    })
                    runtime.put(db, "complaints", {
                        "id": "workspace-view-complaint",
                        "leadId": lead["id"],
                        "author": worker["id"],
                        "text": "Needs a lead response",
                        "status": "open",
                        "created": 1.0,
                        "updated": 1.0,
                        "readAt": None,
                        "version": 1,
                        "recipient": lead["id"],
                        "responses": [],
                    })
                    runtime.put(db, "complaints", {
                        "id": "workspace-view-older-complaint",
                        "leadId": lead["id"],
                        "author": worker["id"],
                        "text": "Older complaint",
                        "status": "open",
                        "created": 0.5,
                        "updated": 0.5,
                        "readAt": None,
                        "version": 1,
                        "recipient": lead["id"],
                        "responses": [],
                    })
                cls.lead_id = lead["id"]
                cls.worker_id = worker["id"]
                cls.other_id = other["id"]
            finally:
                runtime.close()
        cls.backend.start()

    @property
    def backend(self):
        return type(self).backend

    def test_task_detail_reads_python_written_runtime_task(self) -> None:
        response = self.backend.request(
            "GET", "/api/task?" + urlencode({"id": "workspace-task-100"}),
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        task = response.json()
        self.assertEqual(task["id"], "workspace-task-100")
        self.assertEqual(task["agent"], self.lead_id)
        self.assertEqual(task["name"], "fixture tool")
        self.assertEqual(task["tail"], "private tail")
        self.assertNotIn("durationMs", task)
        self.assertNotIn("outputTruncated", task)

    def test_workspace_task_feed_pages_team_records_and_excludes_other_chat(self) -> None:
        query = urlencode({"agent": self.lead_id, "limit": 100})
        first = self.backend.request("GET", "/api/workspace/tasks?" + query)
        self.assertEqual(first.status, 200, first.body[:1000])
        first_page = first.json()
        self.assertEqual(len(first_page["tasks"]), 100)
        self.assertTrue(first_page["hasMore"])
        self.assertEqual(first_page["tasks"][0]["id"], "workspace-task-104")
        self.assertNotIn("workspace-views-other-task", {task["id"] for task in first_page["tasks"]})
        self.assertFalse(
            any({"tail", "arguments", "error"} & task.keys() for task in first_page["tasks"])
        )

        older = self.backend.request(
            "GET",
            "/api/workspace/tasks?" + urlencode({
                "agent": self.lead_id,
                "before": json.dumps(first_page["nextBefore"], separators=(",", ":")),
            }),
        )
        self.assertEqual(older.status, 200, older.body[:1000])
        self.assertEqual(
            [task["id"] for task in older.json()["tasks"]],
            ["workspace-task-004", "workspace-task-003", "workspace-task-002", "workspace-task-001", "workspace-task-000"],
        )

        changes = self.backend.request(
            "GET",
            "/api/workspace/tasks?" + urlencode({
                "agent": self.lead_id,
                "cursor": '{"id":"","updated":0}',
            }),
        )
        self.assertEqual(changes.status, 200, changes.body[:1000])
        self.assertEqual(len(changes.json()["tasks"]), 100)
        self.assertTrue(changes.json()["hasMoreChanges"])

    def test_short_changes_page_advances_and_empty_poll_preserves_cursor(self) -> None:
        first = self.backend.request(
            "GET",
            "/api/workspace/tasks?" + urlencode({
                "agent": self.lead_id,
                "cursor": json.dumps({"updated": 105, "id": "workspace-task-103"}),
                "limit": 100,
            }),
        )
        self.assertEqual(first.status, 200, first.body[:1000])
        first_page = first.json()
        self.assertEqual([task["id"] for task in first_page["tasks"]], ["workspace-task-104"])
        self.assertEqual(first_page["cursor"], {"updated": 106.0, "id": "workspace-task-104"})

        second = self.backend.request(
            "GET",
            "/api/workspace/tasks?" + urlencode({
                "agent": self.lead_id,
                "cursor": json.dumps(first_page["cursor"]),
            }),
        )
        self.assertEqual(second.status, 200, second.body[:1000])
        self.assertEqual(second.json()["tasks"], [])
        self.assertEqual(second.json()["cursor"], first_page["cursor"])

    def test_empty_query_strings_mean_absent(self) -> None:
        response = self.backend.request("GET", "/api/workspace/tasks?agent=&cursor=&before=")
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertIn(
            "workspace-views-other-task",
            {task["id"] for task in response.json()["tasks"]},
        )

    def test_bad_cursor_is_located_and_does_not_write(self) -> None:
        before = self._task_count()
        response = self.backend.request(
            "GET",
            "/api/workspace/tasks?" + urlencode({"cursor": "{not-json"}),
        )
        after = self._task_count()
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json()["location"], ["query", "cursor"])
        self.assertNotIn("not-json", response.body.decode())
        self.assertEqual(after, before)

    def test_unknown_persisted_property_is_refused_by_rust(self) -> None:
        row_id = "workspace-views-other-task"
        original = self._record("runtime_tasks", row_id)
        self._write_record("runtime_tasks", row_id, {**original, "unexpectedStoredProperty": True})
        try:
            response = self.backend.request("GET", "/api/task?" + urlencode({"id": row_id}))
            if self.rust_backend:
                self.assertEqual(response.status, 500, response.body[:1000])
                self.assertIn(f"Invalid stored object runtime_tasks row {row_id}", response.json()["error"])
                self.assertNotIn("unexpectedStoredProperty", response.body.decode())
            else:
                self.assertEqual(response.status, 200, response.body[:1000])
        finally:
            self._write_record("runtime_tasks", row_id, original)

    def test_agent_used_for_deletion_check_is_validated(self) -> None:
        original = self._record("runtime_agents", self.other_id)
        self._write_record("runtime_agents", self.other_id, {})
        try:
            response = self.backend.request(
                "GET", "/api/task?" + urlencode({"id": "workspace-views-other-task"}),
            )
            if self.rust_backend:
                self.assertEqual(response.status, 500, response.body[:1000])
                self.assertIn(
                    f"Invalid stored object runtime_agents row {self.other_id}",
                    response.json()["error"],
                )
            else:
                self.assertEqual(response.status, 200, response.body[:1000])
        finally:
            self._write_record("runtime_agents", self.other_id, original)

    def test_task_feed_refuses_missing_agent_and_mismatched_row_id(self) -> None:
        row_id = "workspace-task-000"
        original = self._record("runtime_tasks", row_id)
        without_agent = {key: value for key, value in original.items() if key != "agent"}
        self._write_record("runtime_tasks", row_id, without_agent)
        try:
            response = self.backend.request(
                "GET", "/api/workspace/tasks?" + urlencode({"agent": self.lead_id}),
            )
            if self.rust_backend:
                self.assertEqual(response.status, 500, response.body[:1000])
                self.assertIn(f"Invalid stored object runtime_tasks row {row_id}", response.json()["error"])
            else:
                self.assertEqual(response.status, 200, response.body[:1000])
        finally:
            self._write_record("runtime_tasks", row_id, original)

        self._write_record("runtime_tasks", row_id, {**original, "id": "different-task-id"})
        try:
            for path in (
                "/api/task?" + urlencode({"id": row_id}),
                "/api/workspace/tasks?" + urlencode({"agent": self.lead_id}),
            ):
                response = self.backend.request("GET", path)
                if self.rust_backend:
                    self.assertEqual(response.status, 500, response.body[:1000])
                    self.assertIn(f"Invalid stored object runtime_tasks row {row_id}", response.json()["error"])
                else:
                    self.assertEqual(response.status, 200, response.body[:1000])
        finally:
            self._write_record("runtime_tasks", row_id, original)

    def test_workspace_snapshot_and_work_views_use_python_written_state(self) -> None:
        workspace = self.backend.request(
            "GET", "/api/workspace?agent=" + self.lead_id,
        )
        self.assertEqual(workspace.status, 200, workspace.body[:1000])
        snapshot = workspace.json()
        self.assertEqual(snapshot["work"][0]["id"], "workspace-view-work")
        self.assertEqual(snapshot["work"][0]["displayStatus"], "ready")
        self.assertEqual(snapshot["annotations"][0]["id"], "workspace-view-annotation")
        self.assertIn("workspace-task-104", {task["id"] for task in snapshot["tasks"]})
        self.assertIn("workspace-view-complaint", {item["id"] for item in snapshot["inbox"]})
        complaint_ids = [item["id"] for item in snapshot["inbox"] if item["kind"] == "complaint"]
        self.assertEqual(
            complaint_ids,
            ["workspace-view-complaint", "workspace-view-older-complaint"],
        )

        work = self.backend.request(
            "GET", "/api/workspace?agent=" + self.lead_id + "&view=work",
        )
        annotations = self.backend.request(
            "GET", "/api/workspace?agent=" + self.lead_id + "&view=annotations",
        )
        inbox = self.backend.request(
            "GET", "/api/workspace?agent=" + self.lead_id + "&view=inbox",
        )
        self.assertEqual(work.status, 200, work.body[:1000])
        self.assertEqual(annotations.status, 200, annotations.body[:1000])
        self.assertEqual(inbox.status, 200, inbox.body[:1000])
        self.assertEqual(work.json()["work"][0]["id"], "workspace-view-work")
        self.assertEqual(annotations.json()["annotations"][0]["id"], "workspace-view-annotation")
        self.assertIn("workspace-view-complaint", {item["id"] for item in inbox.json()["inbox"]})

        etag = workspace.headers.get("etag", "")
        self.assertRegex(etag, r'^"[0-9a-f]{64}"$')
        conditional = self.backend.request(
            "GET",
            "/api/workspace?agent=" + self.lead_id,
            headers={"If-None-Match": etag},
        )
        self.assertEqual(conditional.status, 304, conditional.body[:1000])
        self.assertEqual(conditional.headers.get("etag"), etag)

    def test_changes_reports_git_output_etag_and_image_execution_dependency(self) -> None:
        changes = self.backend.request(
            "GET", "/api/changes?" + urlencode({"agent": self.lead_id}),
        )
        self.assertEqual(changes.status, 200, changes.body[:1000])
        self.assertIn("tracked.txt", {file["path"] for file in changes.json()["files"]})
        self.assertIn("working tree change", changes.json()["diff"])
        etag = changes.headers.get("etag", "")
        self.assertRegex(etag, r'^"[0-9a-f]{64}"$')
        conditional = self.backend.request(
            "GET",
            "/api/changes?" + urlencode({"agent": self.lead_id}),
            headers={"If-None-Match": etag},
        )
        self.assertEqual(conditional.status, 304, conditional.body[:1000])

        original = self._record("runtime_agents", self.lead_id)
        self._write_record("runtime_agents", self.lead_id, {**original, "executionMode": "vm"})
        try:
            unsupported = self.backend.request(
                "GET", "/api/changes?" + urlencode({"agent": self.lead_id}),
            )
            if self.rust_backend:
                self.assertEqual(unsupported.status, 501, unsupported.body[:1000])
                self.assertIn("workspace-state/image-execution", unsupported.json()["error"])
            else:
                self.assertEqual(unsupported.status, 200, unsupported.body[:1000])
        finally:
            self._write_record("runtime_agents", self.lead_id, original)

    def _task_count(self) -> int:
        uri = f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro"
        with sqlite3.connect(uri, uri=True) as connection:
            return connection.execute("SELECT count(*) FROM runtime_tasks").fetchone()[0]

    def _record(self, table: str, row_id: str) -> dict:
        uri = f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro"
        with sqlite3.connect(uri, uri=True) as connection:
            record = connection.execute(
                f"SELECT record FROM {table} WHERE id=?", (row_id,),
            ).fetchone()[0]
        return json.loads(record)

    def _write_record(self, table: str, row_id: str, record: dict) -> None:
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            connection.execute(
                f"UPDATE {table} SET record=? WHERE id=?", (json.dumps(record), row_id),
            )


if __name__ == "__main__":
    unittest.main(verbosity=2)
