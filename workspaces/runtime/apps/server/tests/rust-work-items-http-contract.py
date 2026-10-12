#!/usr/bin/env python3
"""Persisted work, plan and queue reads plus declared partial writes."""

from __future__ import annotations

import importlib.util
import hashlib
import json
import os
import sqlite3
import sys
import unittest
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

HERE = Path(__file__).resolve().parent
SERVER_SOURCE = HERE.parent / "src"
sys.path.insert(0, str(SERVER_SOURCE))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend

_fixture_spec = importlib.util.spec_from_file_location(
    "work_items_runtime_fixture", HERE / "runtime-contract.py"
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


def seed_python_state(backend) -> str:
    """Create representative state with the Python runtime's actual writers."""
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    try:
        lead = runtime.create({
            "name": "Python work reader fixture",
            "cwd": str(backend.cwd),
            "prompt": "Seed persisted work state for the Rust HTTP contract.",
            "autoWake": True,
        }, defer=True)
        work = runtime.work_action(lead["id"], {
            "action": "create", "title": "Python-written work item",
            "description": "Keep legacy fields readable.",
        })
        runtime.plan_action(lead["id"], {"version": 0, "text": "Python-written plan"})
        lead = runtime.agent(lead["id"])
        lead["autoWake"] = True
        lead["status"] = "idle"
        with runtime.db() as database:
            runtime.put(database, "agents", lead)
            runtime.enqueue(database, lead, "user", "Python-written queue item", "python-queue-1")
        return lead["id"]
    finally:
        runtime.close()


def work_state_snapshot(state: Path):
    with sqlite3.connect(state / "canvas.sqlite3") as database:
        names = {
            row[0] for row in database.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )
        }
        snapshot = {}
        for name in (
            "runtime_agents", "runtime_work", "runtime_plans", "runtime_annotations",
            "runtime_events", "runtime_event_meta", "runtime_operation_receipts",
        ):
            if name in names:
                snapshot[name] = database.execute(f'SELECT * FROM "{name}" ORDER BY rowid').fetchall()
        return snapshot


class WorkItemsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_python_state(cls.backend)
        cls.backend.start()

    def _headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_reads_python_written_work_plan_and_queue_state(self):
        work = self.backend.request(
            "GET", f"/api/work?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(work.status, 200, work.body[:1000])
        self.assertEqual(work.json()["items"][0]["title"], "Python-written work item")
        self.assertEqual(work.json()["items"], work.json()["tasks"])

        plan = self.backend.request(
            "GET", f"/api/plan?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(plan.status, 200, plan.body[:1000])
        self.assertEqual(plan.json()["text"], "Python-written plan")

        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            queue = self.backend.request(
                "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
            )
            self.assertEqual(queue.status, 200, queue.body[:1000])
            self.assertEqual([item["id"] for item in queue.json()["items"]], ["python-queue-1"])
            self.assertEqual(queue.json()["items"][0]["text"], "Python-written queue item")
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
                agent = json.loads(database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
                ).fetchone()[0])
            expected_revision = hashlib.sha256(json.dumps(
                [agent["epoch"], agent.get("queueMutationRevision", 0), queue.json()["items"]],
                sort_keys=True,
            ).encode()).hexdigest()
            self.assertEqual(queue.json()["revision"], expected_revision)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_partial_writes_refuse_without_writing(self):
        cases = (
            ("POST", "/api/work", {"agent": self.agent_id, "action": "create", "title": "No write"}, "event-driver/work-items"),
            ("POST", "/api/queue", {"agent": self.agent_id, "action": "cancel", "message_id": "python-queue-1"}, "event-driver/queue-mutations"),
            ("POST", "/api/plan", {"agent": self.agent_id, "version": 1, "text": "No write"}, "event-driver/plan-updates"),
            ("POST", "/api/annotation", {"agent": self.agent_id, "path": "README.md", "line": 1, "text": "No write"}, "event-driver/annotations"),
            ("POST", "/api/organization", {"id": self.agent_id, "pinned": True}, "workspace-state/organization-effects"),
        )
        before = work_state_snapshot(self.backend.state)
        for body in (
            {"agent": self.agent_id, "action": "cancel"},
            {"agent": self.agent_id, "action": "send_now", "message_id": "python-queue-1"},
        ):
            invalid_queue = self.backend.request(
                "POST", "/api/queue", body=body, headers=self._headers(),
            )
            self.assertEqual(invalid_queue.status, 400, invalid_queue.body[:1000])
            self.assertEqual(invalid_queue.json(), {"error": "Invalid request", "location": ["body"]})
            self.assertEqual(work_state_snapshot(self.backend.state), before)
        for method, path, body, dependency in cases:
            with self.subTest(path=path):
                response = self.backend.request(method, path, body=body, headers=self._headers())
                self.assertEqual(response.status, 501, response.body[:1000])
                self.assertEqual(
                    response.json(),
                    {"error": f"Not implemented: requires {dependency}"},
                )
                self.assertEqual(work_state_snapshot(self.backend.state), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_startup_and_reads_tolerate_absent_optional_work_tables(self):
        self.backend.stop()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("DROP TABLE runtime_work")
            database.execute("DROP TABLE runtime_plans")
            database.execute("DROP TABLE runtime_annotations")
            database.execute("DROP TABLE runtime_event_meta")
            database.execute("DROP TABLE runtime_assets")
        self.backend.start()

        work = self.backend.request(
            "GET", f"/api/work?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(work.status, 200, work.body[:1000])
        self.assertEqual(work.json(), {"items": [], "tasks": []})
        plan = self.backend.request(
            "GET", f"/api/plan?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(plan.status, 200, plan.body[:1000])
        self.assertEqual(plan.json()["text"], "")
        queue = self.backend.request(
            "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(queue.status, 200, queue.body[:1000])
        self.assertEqual([item["id"] for item in queue.json()["items"]], ["python-queue-1"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
