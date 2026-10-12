#!/usr/bin/env python3
"""HTTP contracts for persisted task details and workspace task feeds."""

from __future__ import annotations

import os
import sqlite3
import sys
import json
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
                    source = db.execute(
                        "SELECT record FROM runtime_tasks WHERE id=?",
                        ("workspace-views-other-task",),
                    ).fetchone()[0]
                    malformed = json.loads(source)
                    malformed["unexpectedStoredProperty"] = True
                    db.execute(
                        "UPDATE runtime_tasks SET record=? WHERE id=?",
                        (json.dumps(malformed), "workspace-views-other-task"),
                    )
                cls.lead_id = lead["id"]
                cls.worker_id = worker["id"]
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
        response = self.backend.request(
            "GET", "/api/task?" + urlencode({"id": "workspace-views-other-task"}),
        )
        if self.rust_backend:
            self.assertEqual(response.status, 500, response.body[:1000])
            self.assertIn("Invalid stored object runtime_tasks", response.json()["error"])
            self.assertNotIn("unexpectedStoredProperty", response.body.decode())
        else:
            self.assertEqual(response.status, 200, response.body[:1000])

    def test_workspace_and_changes_report_their_unavailable_services(self) -> None:
        workspace = self.backend.request("GET", "/api/workspace")
        changes = self.backend.request(
            "GET", "/api/changes?" + urlencode({"agent": self.lead_id}),
        )
        if self.rust_backend:
            self.assertEqual(workspace.status, 501, workspace.body[:1000])
            self.assertIn("work-policy/views", workspace.json()["error"])
            self.assertIn("coordination-policy/inbox", workspace.json()["error"])
            self.assertEqual(changes.status, 501, changes.body[:1000])
            self.assertIn("workspace-state/git-changes", changes.json()["error"])
        else:
            self.assertEqual(workspace.status, 200, workspace.body[:1000])
            self.assertIn("tasks", workspace.json())
            self.assertEqual(changes.status, 200, changes.body[:1000])
            self.assertIn("files", changes.json())

    def _task_count(self) -> int:
        uri = f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro"
        with sqlite3.connect(uri, uri=True) as connection:
            return connection.execute("SELECT count(*) FROM runtime_tasks").fetchone()[0]


if __name__ == "__main__":
    unittest.main(verbosity=2)
