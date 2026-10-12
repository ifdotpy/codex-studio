#!/usr/bin/env python3
"""Persisted work, plan and queue reads plus declared partial writes."""

from __future__ import annotations

import importlib.util
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
            lead.update(autoWake=False, status="idle")
            runtime.put(database, "agents", lead)
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


def append_python_queue_item(state: Path, agent_id: str) -> None:
    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(state, _fixture.FakeServer)
    try:
        agent = runtime.agent(agent_id)
        with runtime.db() as database:
            agent.update(autoWake=True, status="idle")
            runtime.put(database, "agents", agent)
            runtime.enqueue(database, agent, "user", "Revision behavior fixture", "revision-queue-2")
            agent.update(autoWake=False, status="idle")
            runtime.put(database, "agents", agent)
    finally:
        runtime.close()


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

        queue = self.backend.request(
            "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(queue.status, 200, queue.body[:1000])
        self.assertEqual([item["id"] for item in queue.json()["items"]], ["python-queue-1"])
        self.assertEqual(queue.json()["items"][0]["text"], "Python-written queue item")
        self.assertIn("error", queue.json()["items"][0])
        stable = self.backend.request(
            "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(stable.json()["revision"], queue.json()["revision"])
        self.backend.stop()
        append_python_queue_item(self.backend.state, self.agent_id)
        self.backend.start()
        changed = self.backend.request(
            "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(changed.status, 200, changed.body[:1000])
        self.assertNotEqual(changed.json()["revision"], queue.json()["revision"])
        self.assertEqual(len(changed.json()["items"]), 2)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_partial_writes_refuse_without_writing(self):
        cases = (
            ("POST", "/api/work", {"agent": self.agent_id, "action": "create", "title": "No write"}, "event-driver/work-items"),
            ("POST", "/api/queue", {"agent": self.agent_id, "action": "cancel", "message_id": "python-queue-1"}, "event-driver/queue-mutations"),
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

    def test_reads_refuse_deleted_agents(self):
        state_file = self.backend.state / "canvas.sqlite3"
        with sqlite3.connect(state_file) as database:
            record = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0])
            record["deletedAt"] = 1
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(record), self.agent_id),
            )
        try:
            for path in ("work", "queue", "plan"):
                response = self.backend.request(
                    "GET", f"/api/{path}?agent={self.agent_id}", headers=self._headers()
                )
                self.assertNotEqual(response.status, 200, response.body[:1000])
        finally:
            record.pop("deletedAt")
            with sqlite3.connect(state_file) as database:
                database.execute(
                    "UPDATE runtime_agents SET record=? WHERE id=?",
                    (json.dumps(record), self.agent_id),
                )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_rows_name_the_table_and_id(self):
        state_file = self.backend.state / "canvas.sqlite3"
        bad_work = {
            "id": "wrong-type-work", "rootId": 7, "title": "bad",
            "status": "ready", "created": 1, "updated": 1, "version": 0,
        }
        with sqlite3.connect(state_file) as database:
            database.execute(
                "INSERT INTO runtime_work(id,record) VALUES(?,?)",
                (bad_work["id"], json.dumps(bad_work)),
            )
        response = self.backend.request(
            "GET", f"/api/work?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn("runtime_work/wrong-type-work", response.body.decode())
        with sqlite3.connect(state_file) as database:
            database.execute("DELETE FROM runtime_work WHERE id=?", (bad_work["id"],))
            database.execute(
                "INSERT INTO runtime_work(id,record) VALUES(?,?)",
                ("binary-work", sqlite3.Binary(b"{}")),
            )
        response = self.backend.request(
            "GET", f"/api/work?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn("runtime_work/binary-work", response.body.decode())
        with sqlite3.connect(state_file) as database:
            database.execute("DELETE FROM runtime_work WHERE id='binary-work'")
            prior_plan = database.execute(
                "SELECT record FROM runtime_plans WHERE id=?", (self.agent_id,)
            ).fetchone()
            plan = {"id": self.agent_id, "rootId": self.agent_id, "text": "bad", "version": -1, "updated": 1}
            database.execute(
                "INSERT OR REPLACE INTO runtime_plans(id,record) VALUES(?,?)",
                (self.agent_id, json.dumps(plan)),
            )
        response = self.backend.request(
            "GET", f"/api/plan?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn(f"runtime_plans/{self.agent_id}", response.body.decode())
        with sqlite3.connect(state_file) as database:
            if prior_plan:
                database.execute(
                    "UPDATE runtime_plans SET record=? WHERE id=?",
                    (prior_plan[0], self.agent_id),
                )
            else:
                database.execute("DELETE FROM runtime_plans WHERE id=?", (self.agent_id,))
            agent = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0])
            agent["queueMutationRevision"] = "invalid"
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(agent), self.agent_id),
            )
        response = self.backend.request(
            "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn(f"runtime_agents/{self.agent_id}", response.body.decode())
        agent.pop("queueMutationRevision")
        with sqlite3.connect(state_file) as database:
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(agent), self.agent_id),
            )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_queue_assets_check_live_owner_and_refuse_dangling_asset(self):
        state_file = self.backend.state / "canvas.sqlite3"
        asset = {
            "id": "deleted-owner-asset", "agent": "deleted-owner", "name": "asset",
            "mime": "text/plain", "image": False, "size": 1, "hash": "a" * 64,
            "created": 1.0, "path": "/private/path",
        }
        with sqlite3.connect(state_file) as database:
            owner = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()[0])
            owner.update(id="deleted-owner", rootId="deleted-owner", deletedAt=1)
            database.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?,?)",
                (owner["id"], json.dumps(owner)),
            )
            database.execute(
                "INSERT INTO runtime_assets(id,record) VALUES(?,?)",
                (asset["id"], json.dumps(asset)),
            )
            database.execute(
                "INSERT OR REPLACE INTO runtime_event_meta(id,record) VALUES(?,?)",
                ("python-queue-1", json.dumps({"assets": [asset["id"]]})),
            )
        response = self.backend.request(
            "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn("runtime_agents/deleted-owner", response.body.decode())
        self.backend.stop()
        with sqlite3.connect(state_file) as database:
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (
                json.dumps({**owner, "deletedAt": None}), "deleted-owner"
            ))
            database.execute(
                "DELETE FROM runtime_assets WHERE id=?", (asset["id"],)
            )
        self.backend.start()
        response = self.backend.request(
            "GET", f"/api/queue?agent={self.agent_id}", headers=self._headers()
        )
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn("runtime_assets/deleted-owner-asset", response.body.decode())
        self.backend.stop()
        with sqlite3.connect(state_file) as database:
            database.execute("DELETE FROM runtime_agents WHERE id='deleted-owner'")
            database.execute("DELETE FROM runtime_event_meta WHERE id='python-queue-1'")
        self.backend.start()

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_startup_bootstraps_optional_work_tables(self):
        self.backend.stop()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            database.execute("DROP TABLE runtime_assets")
            database.execute("DROP TABLE runtime_work")
            database.execute("DROP TABLE runtime_plans")
            database.execute("DROP TABLE runtime_annotations")
            database.execute("DROP TABLE runtime_event_meta")
        self.backend.start()

        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            tables = {row[0] for row in database.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )}
        self.assertTrue({
            "runtime_assets", "runtime_work", "runtime_plans",
            "runtime_annotations", "runtime_event_meta",
        }.issubset(tables))

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
        self.assertEqual([item["id"] for item in queue.json()["items"]], ["python-queue-1", "revision-queue-2"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
