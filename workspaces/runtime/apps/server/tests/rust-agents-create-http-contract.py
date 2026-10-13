#!/usr/bin/env python3
"""Persisted agent creation, settings, and rename behavior across HTTP backends."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import hashlib
import importlib.util
import json
import os
import sqlite3
import sys
import unittest
import uuid
from contextlib import closing, contextmanager
from pathlib import Path


isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime

HERE = Path(__file__).resolve().parent
SERVER_SOURCE = HERE.parent / "src"
REVIEW_CORPUS = json.loads((HERE / "fixtures/rust-agents-create/review-corpus.json").read_text())
from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot

_fixture_spec = importlib.util.spec_from_file_location(
    "agents_create_runtime_fixture", HERE / "runtime-contract.py"
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


@contextmanager
def database_connection(path):
    with closing(sqlite3.connect(path)) as database:
        with database:
            yield database


def seed_python_agent(backend) -> str:
    """Use Python's production writers to create a stored agent and projection."""
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = make_runtime(QuietRuntime, backend, _fixture.FakeServer)
    try:
        agent = runtime.create(
            {
                "name": "Python-written rename fixture",
                "cwd": str(backend.cwd),
                "prompt": "Read the existing agent before applying the title.",
                "autoWake": False,
            },
            defer=True,
        )
        return agent["id"]
    finally:
        runtime.close()


def rename_state(state: Path, agent_id: str, request_id: str):
    with database_connection(state / "canvas.sqlite3") as database:
        return {
            "agent": database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,)
            ).fetchone(),
            "projection": database.execute(
                "SELECT seq,hash,payload,deleted FROM sync_entities "
                "WHERE collection='agent' AND id=?",
                (agent_id,),
            ).fetchone(),
            "receipt": database.execute(
                "SELECT signature,result FROM runtime_operation_receipts WHERE id=?",
                (request_id,),
            ).fetchone(),
        }


def normalized_agent(value):
    result = dict(value)
    result.pop("creationIntent", None)
    result["id"] = "<agent>"
    result["rootId"] = "<agent>"
    result["created"] = 0
    if "serverId" in result:
        result["serverId"] = "<server>"
    return result


def python_draft(backend, request, draft=True):
    """Create a draft through the Python runtime so Rust can prove migration replay."""
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
    try:
        created = runtime.new_lead(request) if draft else runtime.create(request)
        with runtime.db() as database:
            stored = runtime.agent(created["id"], database)
            projection = json.loads(database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (created["id"],),
            ).fetchone()[0])["value"]
        return {"record": stored, "projection": projection}
    finally:
        runtime.close()


class AgentsCreateHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.agent_id = seed_python_agent(cls.backend)
        cls.backend.start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def test_draft_creation_replays_same_content_and_syncs_python_shape(self):
        reference_request = {
            "id": "06c9cf18-bf6e-4d6f-bd38-7a886fb32111",
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
        }
        reference = python_draft(self.backend, reference_request)
        self.backend.start()
        python_replay = self.backend.request(
            "POST", "/api/leads", body=reference_request, headers=self.headers()
        )
        self.assertEqual(python_replay.status, 200, python_replay.body[:1000])
        self.assertEqual(python_replay.json()["id"], reference_request["id"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            python_record = json.loads(database.execute(
                "SELECT record FROM runtime_agents WHERE id=?",
                (reference_request["id"],),
            ).fetchone()[0])
            python_projection = json.loads(database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (reference_request["id"],),
            ).fetchone()[0])["value"]
        self.assertEqual(normalized_agent(python_record), normalized_agent(reference["record"]))
        normalized_projection = normalized_agent(python_projection)
        expected_projection = normalized_agent(reference["projection"])
        self.assertEqual(
            set(normalized_projection),
            set(expected_projection),
            f"stored-only={set(normalized_projection) - set(expected_projection)}; "
            f"runtime-only={set(expected_projection) - set(normalized_projection)}",
        )
        self.assertEqual(normalized_projection, expected_projection)

        request = {
            "id": "b7f4742c-f61d-4c77-bcfe-51edfc3d17a9",
            "cwd": str(self.backend.cwd),
            "workspaceMode": "worktree",
        }
        response = self.backend.request(
            "POST", "/api/leads", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        record = response.json()
        self.assertEqual(record["id"], request["id"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            stored = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
            ).fetchone()
            events = database.execute(
                "SELECT COUNT(*) FROM runtime_events WHERE agent=?", (request["id"],)
            ).fetchone()[0]
            projection = database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (request["id"],),
            ).fetchone()
        self.assertIsNotNone(stored)
        stored_record = json.loads(stored[0])
        self.assertTrue(stored_record["quickCreate"])
        self.assertTrue(stored_record["needsTitle"])
        self.assertEqual(stored_record["cwd"], str(self.backend.cwd))
        self.assertEqual(normalized_agent(stored_record), normalized_agent(reference["record"]))
        self.assertEqual(events, 0)
        self.assertIsNotNone(projection)
        self.assertEqual(normalized_agent(json.loads(projection[0])["value"]), normalized_agent(reference["projection"]))
        replay = self.backend.request(
            "POST", "/api/leads", body=request, headers=self.headers()
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], request["id"])
        changed = {**request, "yolo_mode": False}
        before_conflict = sqlite3.connect(self.backend.state / "canvas.sqlite3")
        try:
            old_record = before_conflict.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
            ).fetchone()
            old_projection = before_conflict.execute(
                "SELECT seq,hash,payload,deleted FROM sync_entities "
                "WHERE collection='agent' AND id=?",
                (request["id"],),
            ).fetchone()
        finally:
            before_conflict.close()
        conflict = self.backend.request(
            "POST", "/api/leads", body=changed, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        after = sqlite3.connect(self.backend.state / "canvas.sqlite3")
        try:
            self.assertEqual(
                after.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
                ).fetchone(),
                old_record,
            )
            self.assertEqual(
                after.execute(
                    "SELECT seq,hash,payload,deleted FROM sync_entities "
                    "WHERE collection='agent' AND id=?",
                    (request["id"],),
                ).fetchone(),
                old_projection,
            )
        finally:
            after.close()

    def test_persisted_configure_and_exclusions_update_python_written_agent(self):
        configure = self.backend.request(
            "POST",
            "/api/configure",
            body={"id": self.agent_id, "maxAgents": 10, "tokenBudget": 1234},
            headers=self.headers(),
        )
        self.assertEqual(configure.status, 200, configure.body[:1000])
        self.assertEqual(configure.json()["maxAgents"], 10)
        self.assertEqual(configure.json()["tokenBudget"], 1234)
        conversation = self.backend.request(
            "POST",
            "/api/conversation",
            body={"id": self.agent_id, "excluded_account_keys": ["other-account"]},
            headers=self.headers(),
        )
        self.assertEqual(conversation.status, 200, conversation.body[:1000])
        self.assertEqual(conversation.json()["excludedAccountKeys"], ["other-account"])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            row = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.agent_id,)
            ).fetchone()
            projection = database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.agent_id,),
            ).fetchone()
        stored = json.loads(row[0])
        self.assertEqual(stored["maxAgents"], 10)
        self.assertEqual(stored["tokenBudget"], 1234)
        self.assertEqual(stored["excludedAccountKeys"], ["other-account"])
        self.assertEqual(
            json.loads(projection[0])["value"]["excludedAccountKeys"],
            ["other-account"],
        )

    def test_rust_dependency_refusal_writes_nothing(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Python reference backend implements provider settings")
        previous = self.draft()
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            before = database.execute(
                "SELECT name FROM sqlite_master WHERE type='table' "
                "AND name LIKE 'runtime_%' ORDER BY name"
            ).fetchall()
            records = {
                table[0]: database.execute(f'SELECT * FROM "{table[0]}"').fetchall()
                for table in before
            }
        response = self.backend.request(
            "POST",
            "/api/conversation/delete",
            body={"id": self.agent_id},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 501, response.body[:1000])
        self.assertEqual(
            response.json(),
            {"error": "Not implemented: requires agent-management/conversation-termination"},
        )
        reuse = self.backend.request(
            "POST",
            "/api/leads",
            body={"reuse_empty": True, "previous": previous["id"]},
            headers=self.headers(),
        )
        self.assertEqual(reuse.status, 501, reuse.body[:1000])
        self.assertEqual(
            reuse.json(),
            {"error": "Not implemented: requires agent-management/empty-chat-reuse"},
        )
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            after = database.execute(
                "SELECT name FROM sqlite_master WHERE type='table' "
                "AND name LIKE 'runtime_%' ORDER BY name"
            ).fetchall()
            self.assertEqual(after, before)
            self.assertEqual(
                {
                    table[0]: database.execute(f'SELECT * FROM "{table[0]}"').fetchall()
                    for table in after
                },
                records,
            )

    def test_rust_agent_creation_writes_one_pending_event_and_replays(self):
        request = {
            "id": "45c18ad9-c7c1-4dba-a65b-986914448b77",
            "cwd": str(self.backend.cwd),
            "prompt": "Persist the initial task before any provider starts.",
            "name": "Persisted lead",
            "workspaceMode": "worktree",
        }
        reference = python_draft(self.backend, {**request, "id": str(uuid.uuid4())}, draft=False)
        self.backend.start()
        response = self.backend.request(
            "POST", "/api/agents", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["id"], request["id"])
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            # The HTTP harness pins provider replay; Python may consume its event immediately.
            with database_connection(self.backend.state / "canvas.sqlite3") as database:
                events = database.execute("SELECT kind,text FROM runtime_events WHERE agent=?", (request["id"],)).fetchall()
            self.assertIn(("user", request["prompt"]), events)
            return
        self.assertEqual(response.json()["status"], "queued")
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            agent = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
            ).fetchone()
            event = database.execute(
                "SELECT kind,text,status FROM runtime_events WHERE id=?",
                (request["id"] + ":initial",),
            ).fetchone()
            projection = database.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (request["id"],),
            ).fetchone()
        self.assertEqual(normalized_agent(json.loads(agent[0])), normalized_agent(reference["record"]))
        self.assertEqual(normalized_agent(json.loads(projection[0])["value"]), normalized_agent(reference["projection"]))
        self.assertEqual(
            event,
            ("user", request["prompt"], "pending"),
        )
        self.assertIsNotNone(projection)
        replay = self.backend.request(
            "POST", "/api/agents", body=request, headers=self.headers()
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], request["id"])
        explicit_max = self.backend.request(
            "POST",
            "/api/agents",
            body={**request, "maxAgents": 64},
            headers=self.headers(),
        )
        self.assertEqual(explicit_max.status, 400, explicit_max.body[:1000])
        changed = {**request, "prompt": "Different content must not replace the task."}
        before = (agent, event, projection)
        conflict = self.backend.request(
            "POST", "/api/agents", body=changed, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        with database_connection(self.backend.state / "canvas.sqlite3") as database:
            after = (
                database.execute(
                    "SELECT record FROM runtime_agents WHERE id=?", (request["id"],)
                ).fetchone(),
                database.execute(
                    "SELECT kind,text,status FROM runtime_events WHERE id=?",
                    (request["id"] + ":initial",),
                ).fetchone(),
                database.execute(
                    "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                    (request["id"],),
                ).fetchone(),
            )
        self.assertEqual(after, before)

    def test_rename_reads_python_written_agent_and_projection(self):
        before = rename_state(self.backend.state, self.agent_id, "title-request-1")
        self.assertIsNotNone(before["agent"])
        self.assertIsNotNone(before["projection"])
        request = {
            "id": self.agent_id,
            "name": "  Café review  ",
            "request_id": "title-request-1",
        }
        response = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        response_body = response.json()
        self.assertEqual(
            {key: response_body[key] for key in ("id", "name", "request_id", "status")},
            {
                "id": self.agent_id,
                "name": "Café review",
                "request_id": "title-request-1",
                "status": "applied",
            },
        )
        after = rename_state(self.backend.state, self.agent_id, "title-request-1")
        self.assertEqual(
            response_body["_syncEntitiesAfter"], before["projection"][0]
        )
        self.assertEqual(len(response_body["_syncEntities"]), 1)
        entity = response_body["_syncEntities"][0]
        self.assertEqual(entity["id"], f"entity:agent:{self.agent_id}")
        self.assertEqual(entity["seq"], after["projection"][0])
        self.assertFalse(entity["_deleted"])
        self.assertEqual(
            json.loads(entity["payload"])["value"]["name"], "Café review"
        )
        agent = json.loads(after["agent"][0])
        self.assertEqual(agent["name"], "Café review")
        self.assertTrue(agent["manualName"])
        self.assertFalse(agent["needsTitle"])
        projection = json.loads(after["projection"][2])
        self.assertEqual(projection["value"]["name"], "Café review")
        self.assertTrue(projection["value"]["manualName"])
        signature, receipt_result = after["receipt"]
        body = {"operation": "rename", "chat": self.agent_id, "name": "Café review"}
        expected_signature = hashlib.sha256(
            json.dumps(body, sort_keys=True).encode()
        ).hexdigest()
        self.assertEqual(signature, expected_signature)
        self.assertEqual(
            json.loads(receipt_result),
            {
                "id": self.agent_id,
                "name": "Café review",
                "request_id": "title-request-1",
                "status": "applied",
            },
        )

        replay = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(
            replay.json(),
            {
                "id": self.agent_id,
                "name": "Café review",
                "request_id": "title-request-1",
                "status": "applied",
            },
        )
        self.assertNotIn("_syncEntities", replay.json())
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "title-request-1"),
            after,
        )

    def test_rename_conflict_refuses_without_writing(self):
        request = {
            "id": self.agent_id,
            "name": "Original title",
            "request_id": "title-request-conflict",
        }
        first = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(first.status, 200, first.body[:1000])
        before = rename_state(
            self.backend.state, self.agent_id, "title-request-conflict"
        )
        request["name"] = "Changed title"
        conflict = self.backend.request(
            "POST", "/api/rename", body=request, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(
            conflict.json(), {"error": "This request id has different content"}
        )
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "title-request-conflict"),
            before,
        )

    def test_missing_smart_title_request_refuses_without_writing(self):
        before = rename_state(self.backend.state, self.agent_id, "unused-title-id")
        response = self.backend.request(
            "POST",
            "/api/rename",
            body={"id": self.agent_id, "name": None},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json()["error"], "Invalid request")
        self.assertIn(response.json()["location"], (["body"], ["body", "request_id"]))
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "unused-title-id"), before
        )

    def test_blank_manual_title_refuses_without_writing(self):
        before = rename_state(self.backend.state, self.agent_id, "unused-title-id")
        response = self.backend.request(
            "POST",
            "/api/rename",
            body={"id": self.agent_id, "name": " "},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(
            response.json(), {"error": "A name must have 1 to 80 characters"}
        )
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "unused-title-id"), before
        )

    def test_smart_title_reports_named_501_without_writing(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("the Python reference backend implements live title generation")
        before = rename_state(self.backend.state, self.agent_id, "smart-title-id")
        response = self.backend.request(
            "POST",
            "/api/rename",
            body={"id": self.agent_id, "name": None, "request_id": "smart-title-id"},
            headers=self.headers(),
        )
        self.assertEqual(response.status, 501, response.body[:1000])
        self.assertEqual(
            response.json(),
            {"error": "Not implemented: requires provider-session/title-generation"},
        )
        self.assertEqual(
            rename_state(self.backend.state, self.agent_id, "smart-title-id"), before
        )

    def post(self, path, body, status=200):
        response = self.backend.request("POST", path, body=body, headers=self.headers())
        self.assertEqual(response.status, status, response.body[:1500])
        return response.json()

    def draft(self, **fields):
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "workspaceMode": "worktree", **fields}
        self.post("/api/leads", body)
        return body

    def stored(self, agent_id):
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()[0])

    def projection(self, agent_id):
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            return json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?", (agent_id,)).fetchone()[0])["value"]

    def save(self, record):
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(record), record["id"]))

    def rust_only(self):
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Rust strict persisted validation or unavailable service boundary")


    def test_probe_correction_all_creation_values_and_immutable_replay(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "prompt": "Task", "maxAgents": 64}
        self.post("/api/agents", body)
        variations = {**REVIEW_CORPUS["replayChanges"], "cwd": str(self.backend.root), "parent": self.agent_id}
        for field, value in variations.items():
            with self.subTest(field=field):
                before = no_write_snapshot(self.backend.state)
                self.post("/api/agents", {**body, field: value}, 400)
                self.assertEqual(no_write_snapshot(self.backend.state), before)
        self.post("/api/rename", {"id": body["id"], "name": "Changed"})
        self.assertEqual(self.post("/api/agents", body)["name"], "Changed")
        draft = self.draft()
        self.post("/api/rename", {"id": draft["id"], "name": "Changed draft"})
        self.assertEqual(self.post("/api/leads", draft)["name"], "Changed draft")

    def test_probe_admission_orphan_alias_refuses_without_any_write(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "workspaceMode": "worktree"}
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_lead_requests VALUES(?,?,?)", (body["id"], str(uuid.uuid4()), "{}"))
        before = no_write_snapshot(self.backend.state)
        result = self.post("/api/leads", body, 500)
        self.assertIn("runtime_lead_requests/" + body["id"], result["error"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_probe_admission_invalid_stored_agent_is_refused_by_rename(self):
        self.rust_only()
        body = self.draft()
        record = self.stored(body["id"])
        try:
            self.save({**record, "cwd": 42})
            before = no_write_snapshot(self.backend.state)
            result = self.post("/api/rename", {"id": body["id"], "name": "Invalid kept"}, 500)
            self.assertIn("runtime_agents/" + body["id"], result["error"])
            self.assertEqual(no_write_snapshot(self.backend.state), before)
        finally:
            self.save(record)

    def test_probe_correction_existing_project_is_validated_with_explicit_account(self):
        self.rust_only()
        self.draft()
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            original = db.execute("SELECT record FROM runtime_projects WHERE id=?", (str(self.backend.cwd),)).fetchone()[0]
            db.execute("UPDATE runtime_projects SET record='{' WHERE id=?", (str(self.backend.cwd),))
        try:
            before = no_write_snapshot(self.backend.state)
            result = self.post("/api/leads", {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "account_key": "default"}, 500)
            self.assertIn("runtime_projects/" + str(self.backend.cwd), result["error"])
            self.assertEqual(no_write_snapshot(self.backend.state), before)
        finally:
            with database_connection(self.backend.state / "canvas.sqlite3") as db:
                db.execute("UPDATE runtime_projects SET record=? WHERE id=?", (original, str(self.backend.cwd)))

    def test_probe_active_exclusions_are_allowed_and_id_only_is_refused(self):
        body = self.draft()
        original = self.stored(body["id"])
        try:
            self.save({**original, "status": "running", "inFlight": True})
            result = self.post("/api/conversation", {"id": body["id"], "excluded_account_keys": []})
            self.assertEqual(result["excludedAccountKeys"], [])
            self.post("/api/conversation", {"id": body["id"]}, 400)
        finally:
            self.save(original)

    def test_probe_correction_native_thread_block_survives_rename(self):
        body = self.draft()
        record = self.stored(body["id"])
        block = {"threadId": "blocked-thread", "error": {"codexErrorInfo": "misalignmentPolicyViolation"}}
        self.save({**record, "threadId": "blocked-thread", "nativeThreadBlock": block})
        self.post("/api/rename", {"id": body["id"], "name": "Still blocked"})
        view = self.projection(body["id"])
        self.assertFalse(view["canSend"])
        self.assertEqual(view["nativeThreadBlock"], block)

    def test_probe_correction_conversation_null_catalog_preferences_are_not_absent(self):
        self.rust_only()
        for field in REVIEW_CORPUS["catalogNulls"]:
            with self.subTest(field=field):
                before = no_write_snapshot(self.backend.state)
                result = self.post("/api/conversation", {"id": self.agent_id, field: None}, 501)
                self.assertIn("agent-management/conversation-settings", result["error"])
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_probe_boundaries_explicit_nulls_are_located_schema_refusals(self):
        corpus = [("/api/leads", {"cwd": str(self.backend.cwd)}, ("reuse_empty", "yolo_mode", "cwd", "account_key", "workspaceMode")),
                  ("/api/agents", {"prompt": "Task", "cwd": str(self.backend.cwd)}, ("yolo_mode", "concurrency", "maxAgents", "name", "role", "cwd", "account_key", "workspaceMode")),
                  ("/api/conversation", {"id": self.agent_id}, ("yolo_mode", "excluded_account_keys", "expected_account_key", "cwd", "subagent_concurrency")),
                  ("/api/configure", {"id": self.agent_id}, ("concurrency", "maxAgents"))]
        for path, body, fields in corpus:
            for field in fields:
                for value in (None, {}, []):
                    if field == "excluded_account_keys" and value == []:
                        continue
                    with self.subTest(path=path, field=field, value=value):
                        result = self.post(path, {**body, field: value}, 400)
                        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
                            self.assertIn(field, result["location"])

    def test_probe_boundaries_previous_without_reuse_creates_and_alias_replays(self):
        previous = self.draft(yolo_mode=False)
        body = self.draft(previous=previous["id"], reuse_empty=False)
        self.assertNotEqual(body["id"], previous["id"])
        self.assertFalse(self.stored(body["id"])["yoloMode"])
        alias = str(uuid.uuid4())
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_lead_requests VALUES(?,?,?)", (alias, previous["id"], self.stored(previous["id"])["quickCreateRequest"]))
        self.assertEqual(self.post("/api/leads", {**previous, "id": alias})["id"], previous["id"])
        self.draft(reuse_empty=True)

    def test_probe_boundaries_home_expansion_and_missing_cwd(self):
        self.rust_only()
        (self.backend.home / "project").mkdir(exist_ok=True)
        body = {"id": str(uuid.uuid4()), "prompt": "Task", "cwd": "~/project"}
        result = self.post("/api/agents", body)
        self.assertEqual(result["cwd"], str(self.backend.home / "project"))
        self.post("/api/agents", {"id": str(uuid.uuid4()), "prompt": "Task"}, 400)

    def test_probe_extra_legacy_single_mode_and_deleted_root_keep_tombstones(self):
        body = self.draft()
        original = self.stored(body["id"])
        self.save({**original, "agentMode": "single", "subagentConcurrencyVersion": 1, "concurrency": 32, "deletedAt": 123})
        self.post("/api/configure", {"id": body["id"], "maxAgents": 13})
        saved = self.stored(body["id"])
        self.assertEqual((saved["concurrency"], saved["agentMode"], saved["subagentConcurrencyVersion"]), (0, "single", 2))
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            self.assertEqual(db.execute("SELECT deleted FROM sync_entities WHERE collection='agent' AND id=?", (body["id"],)).fetchone()[0], 1)

    def test_probe_correction_parentless_nonlead_has_no_lead_headroom_or_mode(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "prompt": "Task", "role": "reviewer", "concurrency": 100}
        self.post("/api/agents", body)
        self.assertEqual(self.stored(body["id"])["maxAgents"], 64)
        self.post("/api/configure", {"id": body["id"], "maxAgents": 12})
        saved = self.stored(body["id"])
        for field in ("concurrency", "agentMode", "subagentConcurrencyVersion"):
            self.assertNotIn(field, saved)
        self.post("/api/conversation", {"id": body["id"]})

    def test_probe_extra_null_creation_effort_and_invalid_stored_name(self):
        self.rust_only()
        for route, body in (("/api/leads", {"cwd": str(self.backend.cwd)}), ("/api/agents", {"cwd": str(self.backend.cwd), "prompt": "Task"})):
            for field in REVIEW_CORPUS["creationCatalogNulls"]:
                before = no_write_snapshot(self.backend.state)
                self.post(route, {**body, "id": str(uuid.uuid4()), field: None}, 501)
                self.assertEqual(no_write_snapshot(self.backend.state), before)
        body = self.draft()
        original = self.stored(body["id"])
        try:
            self.save({**original, "name": 42})
            before = no_write_snapshot(self.backend.state)
            result = self.post("/api/conversation", {"id": body["id"], "excluded_account_keys": []}, 500)
            self.assertIn("runtime_agents/" + body["id"], result["error"])
            self.assertEqual(no_write_snapshot(self.backend.state), before)
        finally:
            self.save(original)

    def test_probe_extra_disconnected_account_replay_does_not_refresh(self):
        self.rust_only()
        body = self.draft()
        self.backend.stop()
        path = self.backend.state / "accounts/registry.json"
        original = path.read_bytes()
        registry = json.loads(original)
        registry["accounts"]["default"]["disconnected"] = True
        path.write_text(json.dumps(registry))
        try:
            self.backend.start()
            before = no_write_snapshot(self.backend.state)
            self.post("/api/leads", body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)
        finally:
            self.backend.stop()
            path.write_bytes(original)
            self.backend.start()

    def test_profiles_are_loaded_validated_and_only_catalog_profiles_are_partial(self):
        self.rust_only()
        body = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "prompt": "Task", "profile_id": "unknown-profile"}
        before = no_write_snapshot(self.backend.state)
        self.post("/api/agents", body, 400)
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        profile = {"id": "plain-profile", "name": "Plain", "role": "reviewer", "model": "gpt-6-astra", "effort": None, "instructions": "Follow the fixture instructions."}
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_profiles VALUES(?,?)", (profile["id"], json.dumps(profile)))
        before = no_write_snapshot(self.backend.state)
        self.post("/api/agents", {**body, "profile_id": profile["id"]}, 501)
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        for model, status in (("gpt-6-astra", 501), (42, 500)):
            with database_connection(self.backend.state / "canvas.sqlite3") as db:
                db.execute("UPDATE runtime_profiles SET record=? WHERE id=?", (json.dumps({**profile, "model": model}), profile["id"]))
            before = no_write_snapshot(self.backend.state)
            self.post("/api/agents", {**body, "id": str(uuid.uuid4()), "profile_id": profile["id"]}, status)
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_probe_admission_completed_worker_result_file_survives_configure(self):
        self.rust_only()
        body = self.draft()
        child = {**self.stored(body["id"]), "id": str(uuid.uuid4()), "rootId": body["id"], "parentId": body["id"], "isLead": False, "status": "completed", "lastCompletedTurn": "turn-x", "lastAnswer": "done"}
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            db.execute("INSERT INTO runtime_agents VALUES(?,?)", (child["id"], json.dumps(child)))
            db.execute("INSERT INTO runtime_work VALUES(?,?)", ("result-work", json.dumps({"id": "result-work", "rootId": body["id"], "owner": child["id"], "results": [{"agent": child["id"], "resultFile": "/fixture/result.txt", "created": 1}]})))
        self.post("/api/configure", {"id": body["id"], "maxAgents": 17})
        self.assertEqual(self.projection(child["id"])["overview"]["resultFile"], "/fixture/result.txt")

    def test_every_declared_creation_settings_boundary_refuses_without_writes(self):
        self.rust_only()
        for path, preferences, dependency in REVIEW_CORPUS["boundaries"]:
            with self.subTest(path=path, preferences=preferences):
                body = {"id": self.agent_id} if path in ("/api/configure", "/api/conversation") else {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd)}
                if path == "/api/agents":
                    body["prompt"] = "Task"
                before = no_write_snapshot(self.backend.state)
                self.assertEqual(self.post(path, {**body, **preferences}, 501)["error"], "Not implemented: requires " + dependency)
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_probe_refusal_configure_response_failure_rolls_back_every_write(self):
        self.rust_only()
        body = self.draft()
        original = self.stored(body["id"])
        incomplete = dict(original)
        incomplete.pop("maxAgents")
        try:
            self.save(incomplete)
            before = no_write_snapshot(self.backend.state)
            result = self.post("/api/configure", {"id": body["id"], "tokenBudget": 13}, 500)
            self.assertIn("runtime_agents/" + body["id"], result["error"])
            self.assertEqual(no_write_snapshot(self.backend.state), before)
        finally:
            self.save(original)

    def test_probe_new_previous_eligibility_and_nonempty_reuse(self):
        previous = self.draft(yolo_mode=False)
        original = self.stored(previous["id"])
        try:
            for change in ({"deletedAt": 1}, {"isLead": False}):
                with self.subTest(change=change):
                    self.save({**original, **change})
                    before = no_write_snapshot(self.backend.state)
                    self.post("/api/leads", {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": False}, 400)
                    self.assertEqual(no_write_snapshot(self.backend.state), before)
            self.save({**original, "prompt": "This conversation has content"})
            result = self.post("/api/leads", {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": True})
            self.assertNotEqual(result["id"], previous["id"])
            self.assertEqual(result["cwd"], original["cwd"])
            self.assertFalse(result["yoloMode"])
            self.draft(previous="")
        finally:
            self.save(original)

    def test_previous_reuse_requires_empty_compatible_workspace_and_provider(self):
        self.rust_only()
        previous = self.draft()
        original = self.stored(previous["id"])
        try:
            before = no_write_snapshot(self.backend.state)
            result = self.post("/api/leads", {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": True}, 501)
            self.assertIn("agent-management/empty-chat-reuse", result["error"])
            self.assertEqual(no_write_snapshot(self.backend.state), before)
            for change in ({"layrReady": True}, {"workspaceMode": "image"}, {"provider": "claude"}, {"threadId": "existing-thread"}):
                with self.subTest(change=change):
                    self.save({**original, **change})
                    result = self.post("/api/leads", {"id": str(uuid.uuid4()), "previous": previous["id"], "reuse_empty": True})
                    self.assertNotEqual(result["id"], previous["id"])
        finally:
            self.save(original)

    def test_probe_new_active_exclusions_keep_nullable_control_presence(self):
        body = self.draft()
        original = self.stored(body["id"])
        try:
            self.save({**original, "status": "running", "inFlight": True})
            for field in ("next_turn", "request_id"):
                before = no_write_snapshot(self.backend.state)
                result = self.post("/api/conversation", {"id": body["id"], "excluded_account_keys": [], field: None}, 400)
                self.assertIn("Wait for this turn", result["error"])
                self.assertEqual(no_write_snapshot(self.backend.state), before)
        finally:
            self.save(original)

    def test_probe_new_legacy_replay_explains_missing_intent_without_writes(self):
        self.rust_only()
        original = self.stored(self.agent_id)
        self.assertNotIn("creationIntent", original)
        before = no_write_snapshot(self.backend.state)
        result = self.post("/api/agents", {"id": self.agent_id, "name": original["name"], "prompt": original["prompt"], "cwd": original["cwd"]}, 400)
        self.assertIn("No saved creation intent", result["error"])
        self.assertIn("inspect the existing agent", result["error"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_probe_trim_creation_intent_and_rename_use_python_whitespace(self):
        self.rust_only()
        request = {"id": str(uuid.uuid4()), "cwd": str(self.backend.cwd), "name": "\u001cName\u001f", "prompt": "\u001dTask\u001e", "workspaceMode": "worktree"}
        reference = python_draft(self.backend, {**request, "id": str(uuid.uuid4())}, draft=False)
        self.backend.start()
        self.post("/api/agents", request)
        record = self.stored(request["id"])
        self.assertEqual(normalized_agent(record), normalized_agent(reference["record"]))
        self.assertEqual(normalized_agent(self.projection(request["id"])), normalized_agent(reference["projection"]))
        self.assertEqual(record["creationIntent"]["name"], "Name")
        self.assertEqual(record["creationIntent"]["prompt"], "Task")
        with database_connection(self.backend.state / "canvas.sqlite3") as db:
            self.assertEqual(db.execute("SELECT text FROM runtime_events WHERE agent=?", (request["id"],)).fetchall(), [("Task",)])
        self.post("/api/agents", {**request, "name": "Name", "prompt": "Task"})
        rename = {"id": request["id"], "request_id": "python-trim-rename", "name": "\u001cRenamed\u001f"}
        result = self.post("/api/rename", rename)
        self.assertEqual(result["name"], "Renamed")
        before = no_write_snapshot(self.backend.state)
        self.assertEqual(self.post("/api/rename", {**rename, "name": "Renamed"})["name"], "Renamed")
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        before = no_write_snapshot(self.backend.state)
        self.post("/api/rename", {"id": request["id"], "name": "\u001c\u001f"}, 400)
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_shared_projection_local_server_identity(self):
        body = self.draft()
        self.assertIsInstance(self.projection(body["id"])["serverId"], str)


if __name__ == "__main__":
    unittest.main()
