#!/usr/bin/env python3
"""Plan and annotation writes, idempotency, and pending-event durability."""

from __future__ import annotations

import importlib.util
import json
import os
import sqlite3
import sys
import tempfile
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


def seed_remote_agent(backend) -> str:
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    try:
        agent = runtime.create(
            {"name": "Rust remote event fixture", "cwd": str(backend.cwd),
             "prompt": "Seed a remote agent.", "autoWake": True}, defer=True
        )
        agent.update(autoWake=True, status="idle", remoteWorker={"server": "remote"})
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
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
        note_body = {
            "agent": self.agent_id,
            "id": "annotation-request-1",
            "path": "src/main.rs",
            "line": 3,
            "text": "  Keep the write transactional.  ",
            "turnId": "turn-1",
        }
        python_reference = self._python_write_reference(plan_body, note_body) if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else None
        rust_revisions_before = self._sync_revisions(self.backend.state / "canvas.sqlite3") if python_reference else None
        plan = self.backend.request(
            "POST", "/api/plan", body=plan_body, headers=self.headers()
        )
        self.assertEqual(plan.status, 200, plan.body[:1000])
        self.assertEqual(plan.json()["version"], 1)
        self.assertEqual(plan.json()["text"], "Ship the first Rust write path.")
        note = self.backend.request(
            "POST", "/api/annotation", body=note_body, headers=self.headers()
        )
        self.assertEqual(note.status, 200, note.body[:1000])
        self.assertEqual(note.json()["id"], note_body["id"])
        self.assertEqual(note.json()["rootId"], self.agent_id)
        self.assertEqual(note.json()["text"], "Keep the write transactional.")
        if python_reference is not None:
            self._assert_python_rust_rows_match(python_reference, rust_revisions_before)
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

    def _python_write_reference(self, plan_body, note_body):
        self.backend.stop()
        directory = tempfile.TemporaryDirectory(prefix="event-write-python-")
        self.addCleanup(directory.cleanup)
        state = Path(directory.name) / "state"
        state.mkdir()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as source, sqlite3.connect(state / "canvas.sqlite3") as target:
            source.backup(target)

            class QuietRuntime(_fixture.Runtime):
                def schedule(self):
                    pass

            runtime = QuietRuntime(state, _fixture.FakeServer)
            revisions_before = self._sync_revisions(state / "canvas.sqlite3")
        try:
            runtime.plan_action(self.agent_id, {"id": plan_body["id"], "version": plan_body["version"], "text": plan_body["text"]})
            runtime.annotate(self.agent_id, {key: value for key, value in note_body.items() if key != "agent"})
        finally:
            runtime.close()
        reference = Path(directory.name) / "python.sqlite3"
        with sqlite3.connect(state / "canvas.sqlite3") as source, sqlite3.connect(reference) as target:
            source.backup(target)
        self.backend.start()
        revisions_after = self._sync_revisions(reference)
        return reference, revisions_before, revisions_after

    @staticmethod
    def _sync_revisions(path):
        with sqlite3.connect(path) as database:
            return dict(database.execute("SELECT collection || ':' || id,seq FROM sync_entities"))

    def _assert_python_rust_rows_match(self, python_reference, rust_before):
        python_state, python_before, python_after = python_reference
        def rows(path, query, parameters=()):
            with sqlite3.connect(path) as database:
                return database.execute(query, parameters).fetchall()

        rust_db = self.backend.state / "canvas.sqlite3"
        event_query = "SELECT id,agent,kind,text,status,epoch,turn_id,error FROM runtime_events WHERE agent=? ORDER BY id"
        self.assertEqual(rows(rust_db, event_query, (self.agent_id,)), rows(python_state, event_query, (self.agent_id,)))
        for table, key, timestamp in (("runtime_plans", self.agent_id, "updated"), ("runtime_annotations", "annotation-request-1", "created")):
            actual = json.loads(rows(rust_db, f"SELECT record FROM {table} WHERE id=?", (key,))[0][0])
            expected = json.loads(rows(python_state, f"SELECT record FROM {table} WHERE id=?", (key,))[0][0])
            actual.pop(timestamp, None); expected.pop(timestamp, None)
            self.assertEqual(actual, expected)
        actual_receipt = rows(rust_db, "SELECT signature,result FROM runtime_operation_receipts WHERE id=?", ("annotation-request-1",))[0]
        expected_receipt = rows(python_state, "SELECT signature,result FROM runtime_operation_receipts WHERE id=?", ("annotation-request-1",))[0]
        self.assertEqual(actual_receipt[0], expected_receipt[0])
        actual_result, expected_result = json.loads(actual_receipt[1]), json.loads(expected_receipt[1])
        actual_result.pop("created", None); expected_result.pop("created", None)
        self.assertEqual(actual_result, expected_result)
        for path in (rust_db, python_state):
            record = json.loads(rows(path, "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,))[0][0])
            self.assertEqual(record["status"], "queued")
        rust_after = self._sync_revisions(rust_db)
        rust_start = max(rust_before.values(), default=0)
        python_start = max(python_before.values(), default=0)
        rust_deltas = {key: value-rust_start for key, value in rust_after.items() if value > rust_start}
        python_deltas = {key: value-python_start for key, value in python_after.items() if value > python_start}
        self.assertEqual(rust_deltas, python_deltas)

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

    def test_remote_event_writes_refuse_without_changes(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Rust remote-event boundary")
        agent_id = seed_remote_agent(self.backend)
        self.backend.start()
        before = state_snapshot(self.backend.state)
        for path, body in (
            ("/api/plan", {"agent": agent_id, "version": 0, "text": "blocked"}),
            ("/api/annotation", {"agent": agent_id, "path": "src/main.rs", "line": 1, "text": "blocked"}),
        ):
            response = self.backend.request("POST", path, body=body, headers=self.headers())
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertIn("cross-server/remote-events", response.body.decode())
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
