#!/usr/bin/env python3
"""HTTP contract for atomic work mutations and action-specific fallbacks."""

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
sys.path.insert(0, str(HERE.parent / "src"))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend

_fixture_spec = importlib.util.spec_from_file_location(
    "work_write_runtime_fixture", HERE / "runtime-contract.py"
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
    try:
        agent = runtime.create({
            "name": "Work write HTTP contract",
            "cwd": str(backend.cwd),
            "prompt": "Exercise work writes through HTTP.",
            "autoWake": True,
        }, defer=True)
        agent.update(status="idle", autoWake=True)
        with runtime.db() as database:
            runtime.put(database, "agents", agent)
        return agent["id"]
    finally:
        runtime.close()
        backend.start()


def snapshot(state: Path, tables=None):
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        names = [row[0] for row in database.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'runtime_%' ORDER BY name"
        )]
        if tables is not None:
            names = [name for name in names if name in tables]
        return {
            name: sorted(database.execute(f'SELECT * FROM "{name}"').fetchall(), key=repr)
            for name in names
            # Event delivery can reserve pending events asynchronously after a
            # successful action; the test checks durable event identity below.
            if name != "runtime_events"
        }


def without_generated_fields(value):
    result = dict(value)
    result.pop("id", None)
    result.pop("created", None)
    result.pop("updated", None)
    result.pop("_syncEntities", None)
    result.pop("_syncEntitiesAfter", None)
    return result


class WorkWriteHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_agent(cls.backend)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_create_matches_python_record_shape_and_replays_receipt(self):
        body = {
            "agent": self.agent_id,
            "action": "create",
            "title": "Created over HTTP",
            "description": "Stored once",
            "id": "work-create-replay",
        }
        python_result = None
        python_receipt = None
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.backend.stop()
            with tempfile.TemporaryDirectory(prefix="work-write-python-ref-") as directory:
                state = Path(directory) / "state"
                state.mkdir()
                with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as source, closing(sqlite3.connect(state / "canvas.sqlite3")) as target:
                    source.backup(target)

                class QuietRuntime(_fixture.Runtime):
                    def schedule(self):
                        pass

                runtime = QuietRuntime(state, _fixture.FakeServer)
                try:
                    python_result = runtime.work_action(self.agent_id, body, body["id"])
                finally:
                    runtime.close()
                with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
                    python_receipt = database.execute(
                        "SELECT signature,result FROM runtime_operation_receipts WHERE id=?",
                        (body["id"],),
                    ).fetchone()
            self.backend.start()

        first = self.backend.request(
            "POST", "/api/work", body=body, headers=self.headers()
        )
        self.assertEqual(first.status, 200, first.body[:1000])
        result = first.json()
        self.assertEqual(result["title"], body["title"])
        self.assertEqual(result["description"], body["description"])
        self.assertEqual(result["status"], "ready")
        self.assertIsNone(result["owner"])
        self.assertEqual(result["blockedBy"], [])
        self.assertEqual(result["displayStatus"], "ready")

        after_first = snapshot(self.backend.state)
        replay = self.backend.request(
            "POST", "/api/work", body=body, headers=self.headers()
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(
            without_generated_fields(replay.json()),
            without_generated_fields(result),
        )
        self.assertEqual(snapshot(self.backend.state), after_first)

        if python_result is not None:
            self.assertEqual(
                without_generated_fields(result),
                without_generated_fields(python_result),
            )
            with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
                actual_receipt = database.execute(
                    "SELECT signature,result FROM runtime_operation_receipts WHERE id=?",
                    (body["id"],),
                ).fetchone()
            self.assertEqual(actual_receipt[0], python_receipt[0])
            self.assertEqual(
                without_generated_fields(json.loads(actual_receipt[1])),
                without_generated_fields(json.loads(python_receipt[1])),
            )

        conflict = self.backend.request(
            "POST", "/api/work", body={**body, "title": "Changed"}, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(snapshot(self.backend.state), after_first)

    def test_refusal_and_missing_services_do_not_write(self):
        before = snapshot(self.backend.state)
        invalid = self.backend.request(
            "POST",
            "/api/work",
            body={"agent": self.agent_id, "action": "create", "title": "   "},
            headers=self.headers(),
        )
        self.assertEqual(invalid.status, 400, invalid.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)

        cases = (
            ({"action": "create", "title": "Assigned", "owner": "worker"}, "worker-driver/work-assignment"),
            ({"action": "claim", "task_id": "task"}, "worker-driver/work-assignment"),
            ({"action": "submit", "task_id": "task", "result": "done", "checks": "ok", "revision": "abc"}, "workspace-state/submission-files"),
            ({"action": "accept", "task_id": "task", "result": "verified"}, "workspace-state/agent-archive"),
        )
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            for body, _dependency in cases:
                response = self.backend.request(
                    "POST", "/api/work", body={"agent": self.agent_id, **body}, headers=self.headers()
                )
                self.assertNotEqual(response.status, 200, response.body[:1000])
                self.assertEqual(snapshot(self.backend.state), before)
        else:
            # Python implements these effects; the cross-backend Rust gate owns
            # the explicit dependency envelope assertions for these actions.
            pass

    def test_update_cancel_and_reject_commit_state_and_decision_events(self):
        created = self.backend.request(
            "POST",
            "/api/work",
            body={"agent": self.agent_id, "action": "create", "title": "Before update"},
            headers=self.headers(),
        )
        self.assertEqual(created.status, 200, created.body[:1000])
        updated = self.backend.request(
            "POST",
            "/api/work",
            body={"agent": self.agent_id, "action": "update", "task_id": created.json()["id"], "title": "After update", "description": "Edited"},
            headers=self.headers(),
        )
        self.assertEqual(updated.status, 200, updated.body[:1000])
        self.assertEqual(updated.json()["title"], "After update")
        self.assertEqual(updated.json()["description"], "Edited")

        self.backend.stop()

        class QuietRuntime(_fixture.Runtime):
            def schedule(self):
                pass

        runtime = QuietRuntime(self.backend.state, _fixture.FakeServer)
        try:
            worker = runtime.create(
                {
                    "name": "Work decision worker",
                    "cwd": str(self.backend.cwd),
                    "prompt": "Receive durable work decisions.",
                    "autoWake": True,
                },
                parent=self.agent_id,
                defer=True,
            )
            worker.update(status="idle", autoWake=True)
            root = runtime.agent(self.agent_id)
            with runtime.db() as database:
                runtime.put(database, "agents", worker)
                runtime.put(database, "agents", root)
            cancel_work = runtime.work_action(self.agent_id, {"action": "create", "title": "Cancel task"})
            reject_work = runtime.work_action(self.agent_id, {"action": "create", "title": "Reject task"})
            with runtime.db() as database:
                cancel_row = database.execute(
                    "SELECT record FROM runtime_work WHERE id=?", (cancel_work["id"],)
                ).fetchone()
                cancel_record = json.loads(cancel_row[0])
                cancel_record["owner"] = worker["id"]
                database.execute(
                    "UPDATE runtime_work SET record=? WHERE id=?",
                    (json.dumps(cancel_record), cancel_work["id"]),
                )
                reject_row = database.execute(
                    "SELECT record FROM runtime_work WHERE id=?", (reject_work["id"],)
                ).fetchone()
                reject_record = json.loads(reject_row[0])
                reject_record.update(owner=worker["id"], status="review")
                reject_record["results"] = [{
                    "id": "fixture-result", "agent": worker["id"], "text": "done",
                    "checks": "checked", "revision": "abc1234", "files": [],
                    "created": 1.0, "resultFile": str(self.backend.cwd / "result.md"),
                }]
                database.execute(
                    "UPDATE runtime_work SET record=? WHERE id=?",
                    (json.dumps(reject_record), reject_work["id"]),
                )
                runtime.enqueue_recovery_event(
                    database, root, "work_review",
                    json.dumps({"task": reject_work["id"], "result": reject_record["results"][-1]}),
                    "work-result:fixture-result",
                )
            task_ids = (cancel_work["id"], reject_work["id"])
        finally:
            runtime.close()
        self.backend.start()

        cancel_body = {
            "agent": self.agent_id, "action": "cancel", "task_id": task_ids[0],
            "reason": "No longer needed", "id": "work-cancel-replay",
        }
        cancelled = self.backend.request("POST", "/api/work", body=cancel_body, headers=self.headers())
        self.assertEqual(cancelled.status, 200, cancelled.body[:1000])
        self.assertEqual(cancelled.json()["status"], "cancelled")
        cancel_snapshot = snapshot(self.backend.state, {"runtime_work", "runtime_operation_receipts"})
        cancel_replay = self.backend.request("POST", "/api/work", body=cancel_body, headers=self.headers())
        self.assertEqual(cancel_replay.status, 200, cancel_replay.body[:1000])
        self.assertEqual(without_generated_fields(cancel_replay.json()), without_generated_fields(cancelled.json()))
        self.assertEqual(
            snapshot(self.backend.state, {"runtime_work", "runtime_operation_receipts"}),
            cancel_snapshot,
        )
        reject_body = {
            "agent": self.agent_id, "action": "reject", "task_id": task_ids[1],
            "result": "Add more checks", "id": "work-reject-replay",
        }
        rejected = self.backend.request("POST", "/api/work", body=reject_body, headers=self.headers())
        self.assertEqual(rejected.status, 200, rejected.body[:1000])
        self.assertEqual(rejected.json()["status"], "ready")
        reject_snapshot = snapshot(self.backend.state, {"runtime_work", "runtime_operation_receipts"})
        reject_replay = self.backend.request("POST", "/api/work", body=reject_body, headers=self.headers())
        self.assertEqual(reject_replay.status, 200, reject_replay.body[:1000])
        self.assertEqual(without_generated_fields(reject_replay.json()), without_generated_fields(rejected.json()))
        self.assertEqual(
            snapshot(self.backend.state, {"runtime_work", "runtime_operation_receipts"}),
            reject_snapshot,
        )
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            states = dict(database.execute(
                "SELECT id,status FROM runtime_events WHERE id IN (?,?)",
                ("work-decision:" + task_ids[0] + ":cancel", "work-decision:" + task_ids[1] + ":1"),
            ))
        self.assertIn(states["work-decision:" + task_ids[0] + ":cancel"], {"pending", "reserved"})
        self.assertIn(states["work-decision:" + task_ids[1] + ":1"], {"pending", "reserved"})


if __name__ == "__main__":
    unittest.main()
