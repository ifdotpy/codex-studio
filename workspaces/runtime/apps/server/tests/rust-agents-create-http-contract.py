#!/usr/bin/env python3
"""Persisted agent creation, settings, and rename behavior across HTTP backends."""

from __future__ import annotations

import hashlib
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
    "agents_create_runtime_fixture", HERE / "runtime-contract.py"
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


def seed_python_agent(backend) -> str:
    """Use Python's production writers to create a stored agent and projection."""
    backend.stop()

    class QuietRuntime(_fixture.Runtime):
        def schedule(self):
            pass

    runtime = QuietRuntime(backend.state, _fixture.FakeServer)
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
    with sqlite3.connect(state / "canvas.sqlite3") as database:
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
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
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
        self.assertEqual(events, 0)
        self.assertIsNotNone(projection)
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
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
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
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
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
            body={"reuse_empty": True},
            headers=self.headers(),
        )
        self.assertEqual(reuse.status, 501, reuse.body[:1000])
        self.assertEqual(
            reuse.json(),
            {"error": "Not implemented: requires agent-management/empty-chat-reuse"},
        )
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
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
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("Python reference backend may start a real provider turn")
        request = {
            "id": "45c18ad9-c7c1-4dba-a65b-986914448b77",
            "cwd": str(self.backend.cwd),
            "prompt": "Persist the initial task before any provider starts.",
            "name": "Persisted lead",
            "workspaceMode": "worktree",
        }
        response = self.backend.request(
            "POST", "/api/agents", body=request, headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["id"], request["id"])
        self.assertEqual(response.json()["status"], "queued")
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
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
        self.assertEqual(json.loads(agent[0])["prompt"], request["prompt"])
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
        changed = {**request, "prompt": "Different content must not replace the task."}
        before = (agent, event, projection)
        conflict = self.backend.request(
            "POST", "/api/agents", body=changed, headers=self.headers()
        )
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as database:
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
        self.assertEqual(
            response.json(),
            {"error": "Invalid request", "location": ["body"]},
        )
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


if __name__ == "__main__":
    unittest.main()
