#!/usr/bin/env python3
"""Differential HTTP contract for work writes and action-specific 501s."""

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

HERE = Path(os.path.abspath(os.path.dirname(__file__)))
sys.path.insert(0, str(HERE))
from oracle_guard import OracleGuard

_oracle_guard = OracleGuard()
_filesystem_audit_events = {"open", "os.listdir", "os.scandir", "os.stat", "os.lstat", "os.fstatat"}
sys.addaudithook(
    lambda event, args: _oracle_guard.audit(event, args)
    if event in _filesystem_audit_events
    else None
)
os.stat = lambda path, *args, **kwargs: _oracle_guard.metadata(_oracle_guard.stat, path, *args, **kwargs)
os.lstat = lambda path, *args, **kwargs: _oracle_guard.metadata(_oracle_guard.lstat, path, *args, **kwargs)

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime
from http_backend_harness import BackendProcess, TimedHTTPContractTest, _report_contract_backend_timing
from codex_sync_entities import register_functions

_fixture_spec = importlib.util.spec_from_file_location("work_write_runtime_fixture", HERE / "runtime-contract.py")
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)
_oracle_guard.self_test()


def quiet_runtime(state):
    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass
    return make_runtime(QuietRuntime, state, _fixture.FakeServer)


def fixture_connection(path: Path):
    database = sqlite3.connect(path)
    register_functions(database)
    return database


def pause_agent(backend, agent_id: str) -> None:
    with closing(fixture_connection(backend.state / "canvas.sqlite3")) as database, database:
        row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()
        agent = json.loads(row[0])
        agent.update(status="paused", autoWake=False, moveImportPending=True)
        agent.pop("restartRecovery", None)
        database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), agent_id))


def seed_agents(backend) -> tuple[str, str]:
    runtime = quiet_runtime(backend.state)
    try:
        root = runtime.create({"name": "Work write lead", "cwd": str(backend.cwd), "prompt": "Work write contracts", "autoWake": True}, defer=True)
        root.update(status="idle", autoWake=True)
        with runtime.db() as database:
            runtime.put(database, "agents", root)
        worker = runtime.create({"name": "Work write worker", "cwd": str(backend.cwd), "prompt": "Receive decisions", "autoWake": True}, parent=root["id"], defer=True)
        root.update(status="paused", autoWake=False, concurrency=32, subagentConcurrencyVersion=2, agentMode="multi", moveImportPending=True)
        worker.update(status="paused", autoWake=False, worktree=False, moveImportPending=True)
        with runtime.db() as database:
            runtime.put(database, "agents", root)
            runtime.put(database, "agents", worker)
        return root["id"], worker["id"]
    finally:
        runtime.close()


def start_backend_with_seeded_agents(test_case):
    scratch_root = Path(os.environ["TMPDIR"])
    backend = BackendProcess(scratch_root)
    test_case.addClassCleanup(_report_contract_backend_timing, backend)
    test_case.addClassCleanup(backend.close)
    backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
    agent_ids = seed_agents(backend)
    ready = backend.start()
    backend.ready_samples.append(ready)
    return backend, agent_ids


def _normal(value, work_ids=None):
    if isinstance(value, str) and work_ids:
        for identity, alias in sorted(work_ids.items(), key=lambda item: -len(item[0])):
            value = value.replace(identity, alias)
        return value
    if isinstance(value, list):
        return [_normal(item, work_ids) for item in value]
    if isinstance(value, dict):
        result = {}
        for key, item in value.items():
            if key in {"created", "updated"}:
                continue
            if key == "id" and isinstance(item, str) and work_ids and item in work_ids:
                result[key] = work_ids[item]
            elif isinstance(item, str) and work_ids and item in work_ids:
                result[key] = work_ids[item]
            else:
                result[key] = _normal(item, work_ids)
        return result
    return value


def snapshot(state: Path):
    """Parse every table this write path can mutate, including durable events."""
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        works = database.execute("SELECT id,record FROM runtime_work ORDER BY rowid").fetchall()
        work_ids = {identity: f"work-{index}" for index, (identity, _) in enumerate(works)}
        normalized_work = [
            (work_ids[identity], _normal(json.loads(record), work_ids))
            for identity, record in works
        ]
        agents = []
        for identity, raw_record in database.execute("SELECT id,record FROM runtime_agents ORDER BY id"):
            record = json.loads(raw_record)
            # The dispatcher owns status, attempt, turn, error, recovery and preparation projections.
            for field in ("status", "inFlight", "startAttempt", "turnId", "threadId", "error",
                          "restartRecovery", "connectionRecovery", "worktreePreparation"):
                record.pop(field, None)
            agents.append((identity, _normal(record)))
        receipts = [(identity, signature, _normal(json.loads(result), work_ids)) for identity, signature, result in database.execute("SELECT id,signature,result FROM runtime_operation_receipts ORDER BY id")]
        events = []
        for row in database.execute("SELECT id,agent,kind,text,created,epoch,turn_id,error FROM runtime_events ORDER BY id"):
            identity, agent, kind, body, created, epoch, turn_id, error = row
            try:
                body = _normal(json.loads(body), work_ids)
            except (TypeError, ValueError):
                pass
            # The background dispatcher owns event status; compare durable event content and identity.
            events.append((_normal(identity, work_ids), agent, kind, body, epoch, turn_id, error))
        return {"runtime_work": normalized_work, "runtime_agents": agents, "runtime_operation_receipts": receipts, "runtime_events": events}


def reference_action(backend, agent_id: str, body: dict):
    """Run the Python writer on a byte-for-byte database copy of the input state."""
    with tempfile.TemporaryDirectory(prefix="work-write-python-ref-") as directory:
        state = Path(directory) / "state"
        state.mkdir()
        with closing(sqlite3.connect(backend.state / "canvas.sqlite3")) as source, closing(sqlite3.connect(state / "canvas.sqlite3")) as target:
            source.backup(target)
        runtime = quiet_runtime(state)
        try:
            result = runtime.work_action(agent_id, {"agent": agent_id, **body}, body.get("id"))
        finally:
            runtime.close()
        expected = snapshot(state)
        return result, expected


def normal_result(value):
    if isinstance(value, dict):
        value = {key: item for key, item in value.items() if key not in {"_syncEntities", "_syncEntitiesAfter"}}
        if "rootId" in value and "title" in value and "owner" in value:
            value = {**value, "id": "<work-item>"}
    return _normal(value)


class WorkWriteHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend, (cls.agent_id, cls.worker_id) = start_backend_with_seeded_agents(cls)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self):
        return {"Origin": self.backend.origin or "", "X-Canvas-Token": self.backend.session["token"], "X-Studio-API-Schema": self.backend.schema_hash}

    def post(self, body):
        return self.backend.request("POST", "/api/work", body={"agent": self.agent_id, **body}, headers=self.headers())

    def post_as(self, agent_id, body):
        return self.backend.request("POST", "/api/work", body={"agent": agent_id, **body}, headers=self.headers())

    def assert_matches_python(self, body):
        expected_result, expected_state = reference_action(self.backend, self.agent_id, body)
        response = self.post(body)
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(normal_result(response.json()), normal_result(expected_result))
        self.assertEqual(snapshot(self.backend.state), expected_state)
        return response.json()

    def test_create_matches_python_fields_receipt_replay_and_dependency_state(self):
        dependency = self.post({"action": "create", "title": "Accepted dependency"})
        self.assertEqual(dependency.status, 200, dependency.body[:1000])
        dependency_id = dependency.json()["id"]
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_work WHERE id=?", (dependency_id,)).fetchone()
            record = json.loads(row[0]); record["status"] = "accepted"
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(record), dependency_id))
        body = {"action": "create", "title": "Blocked create", "description": "Stored once", "status": "blocked", "dependencies": [dependency_id], "id": "work-create-replay"}
        result = self.assert_matches_python(body)
        self.assertEqual(result["dependencies"], [dependency_id])
        self.assertEqual(result["status"], "blocked")
        before = snapshot(self.backend.state)
        replay = self.post(body)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(normal_result(replay.json()), normal_result(result))
        self.assertEqual(snapshot(self.backend.state), before)
        conflict = self.post({**body, "title": "Changed"})
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)

    def test_claim_submit_accept_and_host_result_artifact(self):
        worker_task = self.post({"action": "create", "title": "Worker submission"})
        self.assertEqual(worker_task.status, 200, worker_task.body[:1000])
        worker_id = worker_task.json()["id"]
        claimed = self.assert_matches_python({
            "action": "claim", "task_id": worker_id, "owner": self.worker_id,
        })
        self.assertEqual(claimed["owner"], self.worker_id)
        submit = {
            "action": "submit", "task_id": worker_id, "version": claimed["version"],
            "result": "Worker result", "checks": "Contract passed", "revision": "abc1234",
            "files": ["src/example.rs"], "id": "work-submit-replay",
        }
        response = self.post_as(self.worker_id, submit)
        self.assertEqual(response.status, 200, response.body[:1000])
        result = response.json()["results"][-1]
        self.assertEqual(result["agent"], self.worker_id)
        self.assertEqual(result["text"], "Worker result")
        self.assertEqual(result["checks"], "Contract passed")
        result_path = Path(result["resultFile"])
        self.assertTrue(result_path.is_file())
        self.assertIn("Worker result", result_path.read_text())
        after_submit = snapshot(self.backend.state)
        replay = self.post_as(self.worker_id, submit)
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(normal_result(replay.json()), normal_result(response.json()))
        self.assertEqual(snapshot(self.backend.state), after_submit)
        before_archive = snapshot(self.backend.state)
        worker_accept = self.post({"action": "accept", "task_id": worker_id, "result": "Verified"})
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(worker_accept.status, 501, worker_accept.body[:1000])
            self.assertEqual(worker_accept.json().get("error"), "Not implemented: requires workspace-state/agent-archive")
            self.assertEqual(snapshot(self.backend.state), before_archive)
        else:
            self.assertEqual(worker_accept.status, 200, worker_accept.body[:1000])
            self.assertEqual(worker_accept.json()["status"], "accepted")
            self.assertEqual(worker_accept.json()["archive"]["status"], "kept")

        lead_task = self.post({"action": "create", "title": "Lead submission"})
        self.assertEqual(lead_task.status, 200, lead_task.body[:1000])
        lead_id = lead_task.json()["id"]
        lead_claim = self.post({"action": "claim", "task_id": lead_id, "owner": self.agent_id})
        self.assertEqual(lead_claim.status, 200, lead_claim.body[:1000])
        lead_submit = self.post({
            "action": "submit", "task_id": lead_id, "version": lead_claim.json()["version"],
            "result": "Lead result", "checks": "Contract passed", "revision": "abc1234",
            "id": "lead-submit-replay",
        })
        self.assertEqual(lead_submit.status, 200, lead_submit.body[:1000])
        accepted = self.post({
            "action": "accept", "task_id": lead_id, "version": lead_submit.json()["version"],
            "result": "Verified on host", "id": "lead-accept-replay",
        })
        self.assertEqual(accepted.status, 200, accepted.body[:1000])
        self.assertEqual(accepted.json()["status"], "accepted")
        self.assertEqual(accepted.json()["archive"]["status"], "skipped")
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(accepted.json()["archivePending"], False)
            vm_task = self.post({"action": "create", "title": "VM submission"})
            self.assertEqual(vm_task.status, 200, vm_task.body[:1000])
            vm_id = vm_task.json()["id"]
            vm_claim = self.post({"action": "claim", "task_id": vm_id, "owner": self.worker_id})
            self.assertEqual(vm_claim.status, 200, vm_claim.body[:1000])
            with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
                row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.worker_id,)).fetchone()
                worker = json.loads(row[0]); worker["executionMode"] = "vm"
                database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(worker), self.worker_id))
            before_vm_submit = snapshot(self.backend.state)
            vm_submit = self.post_as(self.worker_id, {
                "action": "submit", "task_id": vm_id, "result": "VM result",
                "checks": "Contract passed", "revision": "abc1234",
            })
            self.assertEqual(vm_submit.status, 501, vm_submit.body[:1000])
            self.assertEqual(vm_submit.json().get("error"), "Not implemented: requires workspace-state/submission-files")
            self.assertEqual(snapshot(self.backend.state), before_vm_submit)

    def test_refusals_stale_versions_deleted_agent_and_failure_roll_back_all_tables(self):
        created = self.post({"action": "create", "title": "Refusal target"})
        self.assertEqual(created.status, 200, created.body[:1000])
        body = {"action": "update", "task_id": created.json()["id"], "version": 0.0, "title": "Stale"}
        before = snapshot(self.backend.state)
        stale = self.post(body)
        self.assertEqual(stale.status, 400, stale.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)
        cases = ()
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            cases = (
                ({"action": "create", "title": "Assigned", "owner": self.worker_id}, "worker-driver/work-assignment"),
                ({"action": "update", "task_id": created.json()["id"], "owner": self.worker_id}, "worker-driver/work-assignment"),
            )
        for body, dependency in cases:
            before = snapshot(self.backend.state)
            response = self.post(body)
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(response.json().get("error"), f"Not implemented: requires {dependency}")
            self.assertEqual(snapshot(self.backend.state), before)
        cancel = {"action": "cancel", "task_id": created.json()["id"], "reason": " "}
        before = snapshot(self.backend.state)
        refused = self.post(cancel)
        self.assertEqual(refused.status, 400, refused.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)
        cancelled = self.assert_matches_python({"action": "cancel", "task_id": created.json()["id"], "reason": "Retire before replay"})
        self.assertEqual(cancelled["status"], "cancelled")
        recancel = {"action": "cancel", "task_id": created.json()["id"], "reason": " ", "id": "invalid-recancel-reason"}
        before = snapshot(self.backend.state)
        refused = self.post(recancel)
        self.assertEqual(refused.status, 400, refused.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_work WHERE id=?", (created.json()["id"],)).fetchone()
            work = json.loads(row[0]); work["owner"] = self.worker_id
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(work), created.json()["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("work-ready-before-failure", self.worker_id, "work_ready", json.dumps({"task":created.json()["id"]}), "pending", 1.0, 0, None, None))
            database.execute("CREATE TRIGGER fail_work_receipt BEFORE INSERT ON runtime_operation_receipts BEGIN SELECT RAISE(ABORT,'injected'); END")
        failure_body = {"action": "cancel", "task_id": created.json()["id"], "reason": "Roll back", "id": "work-injected-failure"}
        before = snapshot(self.backend.state)
        failed = self.post(failure_body)
        self.assertNotEqual(failed.status, 200, failed.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            database.execute("DROP TRIGGER fail_work_receipt")
        deleted_body = {"action": "update", "task_id": created.json()["id"], "title": "No", "id": "deleted-selected-agent"}
        self.assert_matches_python(deleted_body)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0]); agent["deletedAt"] = True
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))
        before = snapshot(self.backend.state)
        deleted = self.post(deleted_body)
        self.assertEqual(deleted.status, 400, deleted.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0]); agent.pop("deletedAt", None); agent.update(concurrency=0, agentMode="single", subagentConcurrencyVersion=2)
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))
        before = snapshot(self.backend.state)
        single = self.post({"action": "create", "title": "Blocked worker", "owner": self.worker_id, "status": "blocked", "id": "single-agent-delegation"})
        self.assertEqual(single.status, 400, single.body[:1000])
        self.assertEqual(snapshot(self.backend.state), before)
        with fixture_connection(self.backend.state / "canvas.sqlite3") as database:
            row = database.execute("SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)).fetchone()
            agent = json.loads(row[0]); agent.update(concurrency=32, agentMode="multi", subagentConcurrencyVersion=2)
            database.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(agent), self.agent_id))

    def test_update_cancel_reject_match_python_and_preserve_durable_events(self):
        dependency = self.assert_matches_python({"action": "create", "title": "Ready dependency", "id": "update-dep"})
        created = self.assert_matches_python({"action": "create", "title": "Before update", "id": "update-target"})
        update = {"action": "update", "task_id": created["id"], "version": 1, "title": "After update", "description": "Edited", "owner": None, "status": "blocked", "dependencies": [dependency["id"]], "id": "work-update-state"}
        updated = self.assert_matches_python(update)
        self.assertEqual((updated["title"], updated["status"], updated["dependencies"]), ("After update", "blocked", [dependency["id"]]))
        replay_body = {"action": "update", "task_id": created["id"], "id": "work-update-replay", "title": "Replay edit"}
        self.assert_matches_python(replay_body)
        before = snapshot(self.backend.state)
        changed = self.post({**replay_body, "title": "Different edit"})
        self.assertEqual(changed.status, 400, changed.body[:1000]); self.assertEqual(snapshot(self.backend.state), before)
        task_response = self.post({"action": "create", "title": "Cancel task"})
        review_response = self.post({"action": "create", "title": "Reject task"})
        self.assertEqual(task_response.status, 200, task_response.body[:1000])
        self.assertEqual(review_response.status, 200, review_response.body[:1000])
        task, review = task_response.json(), review_response.json()
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            task_row = database.execute("SELECT record FROM runtime_work WHERE id=?", (task["id"],)).fetchone()
            task_record = json.loads(task_row[0]); task_record["owner"] = self.worker_id
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(task_record), task["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("old-work-ready", self.worker_id, "work_ready", json.dumps({"task": task["id"]}), "pending", 1.0, 0, None, None))
            review_row = database.execute("SELECT record FROM runtime_work WHERE id=?", (review["id"],)).fetchone()
            review_record = json.loads(review_row[0]); review_record.update(owner=self.worker_id, status="review", results=[{"id":"fixture-result","agent":self.worker_id,"text":"done","checks":"checked","revision":"abc1234","files":[],"created":1.0,"resultFile":str(self.backend.cwd / "result.md")}])
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(review_record), review["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("work-result:fixture-result", self.agent_id, "work_review", json.dumps({"task":review["id"],"result":review_record["results"][-1]}), "pending", 1.0, 0, None, None))
        cancel_body = {"action":"cancel","task_id":task["id"],"reason":"No longer needed","id":"work-cancel-replay"}
        cancelled = self.assert_matches_python(cancel_body)
        self.assertEqual(cancelled["status"], "cancelled")
        self.assert_matches_python(cancel_body)
        changed_key = {**cancel_body,"reason":"Different reason"}
        before = snapshot(self.backend.state)
        conflict = self.post(changed_key)
        self.assertEqual(conflict.status, 400, conflict.body[:1000]); self.assertEqual(snapshot(self.backend.state), before)
        reject_body = {"action":"reject","task_id":review["id"],"result":"Add more checks","id":"work-reject-replay"}
        rejected = self.assert_matches_python(reject_body)
        self.assertEqual(rejected["status"], "ready")
        self.assert_matches_python(reject_body)
        pause_agent(self.backend, self.agent_id)
        root_response = self.post({"action": "create", "title": "Lead-owned result"})
        self.assertEqual(root_response.status, 200, root_response.body[:1000])
        root_review = root_response.json()
        with closing(fixture_connection(self.backend.state / "canvas.sqlite3")) as database, database:
            row = database.execute("SELECT record FROM runtime_work WHERE id=?", (root_review["id"],)).fetchone()
            record = json.loads(row[0]); record.update(owner=self.agent_id, status="review", results=[{"id":"lead-result","agent":self.agent_id,"text":"done","checks":"checked","revision":"abc1234","files":[],"created":1.0,"resultFile":str(self.backend.cwd / "lead-result.md")}])
            database.execute("UPDATE runtime_work SET record=? WHERE id=?", (json.dumps(record), root_review["id"]))
            database.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)", ("work-result:lead-result", self.agent_id, "work_review", json.dumps({"task":root_review["id"],"result":record["results"][-1]}), "pending", 1.0, 0, None, None))
        lead_reject = {"action": "reject", "task_id": root_review["id"], "result": "Add lead details", "id": "reject-lead-owned"}
        self.assert_matches_python(lead_reject)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            self.assertIsNotNone(database.execute("SELECT 1 FROM runtime_events WHERE id=?", (f"work-decision:{root_review['id']}:1",)).fetchone())
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            events = {row[0]: row[1:] for row in database.execute("SELECT id,agent,kind,text,status FROM runtime_events WHERE id IN (?,?,?)", ("old-work-ready", f"work-decision:{task['id']}:cancel", f"work-decision:{review['id']}:1"))}
        self.assertEqual(events["old-work-ready"][3], "stored_only")
        self.assertEqual(events[f"work-decision:{task['id']}:cancel"][3], "pending")
        self.assertEqual(events[f"work-decision:{review['id']}:1"][3], "pending")


if __name__ == "__main__":
    unittest.main()
