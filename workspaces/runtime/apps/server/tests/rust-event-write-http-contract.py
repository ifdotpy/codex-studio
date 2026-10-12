#!/usr/bin/env python3
"""Plan and annotation writes, idempotency, and pending-event durability."""

from __future__ import annotations

import importlib.util
import json
import os
import sqlite3
import sys
import unittest
from contextlib import closing
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

HERE = Path(__file__).resolve().parent
SERVER_SOURCE = HERE.parent / "src"
sys.path.insert(0, str(SERVER_SOURCE))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend

_fixture_spec = importlib.util.spec_from_file_location(
    "event_write_runtime_fixture", HERE / "runtime-contract.py"
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


def seed_agent(backend) -> str:
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    auto_wake = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
    try:
        agent = runtime.create(
            {
                "name": "Rust event-write fixture",
                "cwd": str(backend.cwd),
                "prompt": "Seed an idle agent for HTTP write contracts.",
                "autoWake": auto_wake,
            },
            defer=True,
        )
        agent.update(autoWake=auto_wake, status="idle")
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
        return agent["id"]
    finally:
        runtime.close()


def seed_agent_with_event_rule(backend) -> str:
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    try:
        agent = runtime.create(
            {
                "name": "Rust event-rule fixture",
                "cwd": str(backend.cwd),
                "prompt": "Seed an agent with an active event rule.",
                "autoWake": True,
            },
            defer=True,
        )
        agent.update(autoWake=True, status="idle")
        rule = {
            "id": f"event-rule-{agent['id']}",
            "agent": agent["id"],
            "epoch": agent["epoch"],
            "status": "active",
            "kind": "event",
            "event": "worker_completed",
            "text": "Continue after work completes.",
        }
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
            runtime.put(database, "rules", rule)
        return agent["id"]
    finally:
        runtime.close()


def state_snapshot(state: Path):
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        tables = [
            row[0]
            for row in database.execute(
                "SELECT name FROM sqlite_master WHERE type='table' "
                "AND (name LIKE 'runtime_%' OR name LIKE 'sync_%') ORDER BY name"
            )
        ]
        return {
            name: sorted(database.execute(f'SELECT * FROM "{name}"').fetchall(), key=repr)
            for name in tables
        }


class EventWriteHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_agent(cls.backend)
        cls.backend.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self):
        backend = self.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def test_plan_and_annotation_write_contract(self):
        plan_body = {
            "agent": self.agent_id,
            "id": "plan-request-1",
            "version": 0,
            "text": "  Ship the first Rust write path.  ",
        }
        plan = self.backend.request(
            "POST", "/api/plan", body=plan_body, headers=self.headers()
        )
        self.assertEqual(plan.status, 200, plan.body[:1000])
        self.assertEqual(plan.json()["version"], 1)
        self.assertEqual(plan.json()["text"], "Ship the first Rust write path.")

        note_body = {
            "agent": self.agent_id,
            "id": "annotation-request-1",
            "path": "src/main.rs",
            "line": 3,
            "text": "  Keep the write transactional.  ",
            "turnId": "turn-1",
        }
        note = self.backend.request(
            "POST", "/api/annotation", body=note_body, headers=self.headers()
        )
        self.assertEqual(note.status, 200, note.body[:1000])
        self.assertEqual(note.json()["id"], note_body["id"])
        self.assertEqual(note.json()["rootId"], self.agent_id)
        self.assertEqual(note.json()["text"], "Keep the write transactional.")
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            statuses = database.execute(
                "SELECT id,status FROM runtime_events WHERE id IN (?,?) ORDER BY id",
                (f"plan:{self.agent_id}:1", f"annotation:{note_body['id']}"),
            ).fetchall()
        expected_status = "pending" if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else "cancelled"
        self.assertEqual([status for _, status in statuses], [expected_status, expected_status])

        committed = state_snapshot(self.backend.state)
        for body in (
            {**plan_body, "version": 999},
            {**note_body, "line": 0},
        ):
            path = "/api/plan" if "version" in body else "/api/annotation"
            response = self.backend.request(
                "POST", path, body=body, headers=self.headers()
            )
            self.assertEqual(response.status, 400, response.body[:1000])
            self.assertEqual(state_snapshot(self.backend.state), committed)

        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self._assert_rust_replay_and_python_scheduler(plan_body, note_body)

    def _assert_rust_replay_and_python_scheduler(self, plan_body, note_body):
        before_replay = state_snapshot(self.backend.state)
        for path, body in (("/api/plan", plan_body), ("/api/annotation", note_body)):
            replay = self.backend.request(
                "POST", path, body=body, headers=self.headers()
            )
            self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(state_snapshot(self.backend.state), before_replay)

        for path, body in (
            ("/api/plan", {**plan_body, "text": "conflicting plan"}),
            ("/api/annotation", {**note_body, "text": "conflicting note"}),
        ):
            refusal = self.backend.request(
                "POST", path, body=body, headers=self.headers()
            )
            self.assertEqual(refusal.status, 400, refusal.body[:1000])
        self.assertEqual(state_snapshot(self.backend.state), before_replay)

        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
            events = database.execute(
                "SELECT id,agent,kind,status FROM runtime_events "
                "WHERE id IN (?,?) ORDER BY id",
                (f"plan:{self.agent_id}:1", f"annotation:{note_body['id']}"),
            ).fetchall()
        self.assertEqual(
            events,
            [
                (f"annotation:{note_body['id']}", self.agent_id, "user", "pending"),
                (f"plan:{self.agent_id}:1", self.agent_id, "plan_update", "pending"),
            ],
        )

        self.backend.stop()

        class QuietRuntime(_fixture.Runtime):
            def schedule(self):
                pass

        runtime = QuietRuntime(self.backend.state, _fixture.FakeServer)
        try:
            from codex_wakeups import pending_batch

            agent = runtime.agent(self.agent_id)
            self.assertEqual(agent["status"], "queued")
            with runtime.db() as database:
                pending = pending_batch(runtime, database, agent)
            pending_ids = {row["id"] for row in pending}
            self.assertIn(f"plan:{self.agent_id}:1", pending_ids)
            self.assertIn(f"annotation:{note_body['id']}", pending_ids)
        finally:
            runtime.close()

    def test_active_event_rule_refuses_both_writes_without_changes(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Python backend owns its existing event-rule delivery path")
        self.backend.stop()
        agent_id = seed_agent_with_event_rule(self.backend)
        self.backend.start()
        before = state_snapshot(self.backend.state)
        requests = (
            (
                "/api/plan",
                {"agent": agent_id, "id": "rule-plan", "version": 0, "text": "blocked"},
            ),
            (
                "/api/annotation",
                {
                    "agent": agent_id,
                    "id": "rule-annotation",
                    "path": "src/main.rs",
                    "line": 1,
                    "text": "blocked",
                },
            ),
        )
        for path, body in requests:
            response = self.backend.request(
                "POST", path, body=body, headers=self.headers()
            )
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn("rule-driver/event-rules", response.body.decode())
            self.assertEqual(state_snapshot(self.backend.state), before)

    def test_rust_replays_python_annotation_receipt_by_content(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Rust/Python cross-runtime receipt replay")
        self.backend.stop()

        class QuietRuntime(_fixture.Runtime):
            def schedule(self):
                pass

        runtime = QuietRuntime(self.backend.state, _fixture.FakeServer)
        body = {
            "id": "python-annotation-replay",
            "path": "src/main.rs",
            "line": 7,
            "text": "written by Python",
        }
        try:
            agent = runtime.create(
                {
                    "name": "Python annotation receipt fixture",
                    "cwd": str(self.backend.cwd),
                    "prompt": "Seed a Python annotation receipt.",
                    "autoWake": True,
                },
                defer=True,
            )
            agent.update(autoWake=True, status="idle")
            with runtime.db() as database:
                runtime.put(database, "agents", agent)
            saved = runtime.annotate(agent["id"], body)
            body["agent"] = agent["id"]
        finally:
            runtime.close()

        self.backend.start()
        before = state_snapshot(self.backend.state)
        response = self.backend.request(
            "POST", "/api/annotation", body=body, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json(), saved)
        self.assertEqual(state_snapshot(self.backend.state), before)


if __name__ == "__main__":
    unittest.main()
