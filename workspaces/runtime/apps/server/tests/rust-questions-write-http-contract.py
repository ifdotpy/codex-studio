#!/usr/bin/env python3
"""Question state writes, provider refusal, and durable complaint events."""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
import unittest
import uuid
from contextlib import closing
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def seed_records(backend) -> None:
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import os, sys, time, uuid
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime

runtime = Runtime(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
try:
    # Keep the dispatcher from claiming events between each write and its
    # durability assertion; the contract compares pending event records.
    lead = {{"id":"questions-write-lead", "rootId":"questions-write-lead", "epoch":0,
        "status":"idle", "name":"Question write lead", "role":"orchestrator",
        "cwd":str(Path(os.environ["CODEX_CANVAS_CWD"])), "isLead":True,
        "autoWake":True, "moveImportPending":True, "approvalPolicy":"never"}}
    worker = {{**lead, "id":"questions-write-worker", "rootId":lead["id"], "isLead":False}}
    deleted_agent = {{**worker, "id":"questions-write-deleted-agent"}}
    now = time.time()
    with runtime.lock, runtime.db() as db:
        deleted_agent["deletedAt"] = now
        stopped_lead = {{**lead, "id":"questions-write-stopped-lead", "rootId":"questions-write-stopped-lead", "autoWake":False}}
        deleted_lead = {{**lead, "id":"questions-write-deleted-lead", "rootId":"questions-write-deleted-lead", "deletedAt":now}}
        answered_lead = {{**lead, "id":"questions-write-answered-lead", "rootId":"questions-write-answered-lead"}}
        malformed_agent = {{**worker, "id":"questions-write-malformed-agent"}}
        for agent in (lead, worker, deleted_agent, stopped_lead, deleted_lead, answered_lead, malformed_agent):
            runtime.put(db, "agents", agent, sync_rooms=False)
        for key, method, status in (
            ("defer", "agent/asyncQuestion", "pending"),
            ("delete", "agent/asyncQuestion", "pending"),
            ("native", "item/tool/requestUserInput", "pending"),
            ("elicitation", "mcpServer/elicitation/request", "pending"),
            ("answered", "agent/asyncQuestion", "answered"),
            ("answering", "agent/asyncQuestion", "answering"),
            ("uncertain", "agent/asyncQuestion", "uncertain"),
            ("deleted-agent", "agent/asyncQuestion", "pending"),
            ("malformed-agent", "agent/asyncQuestion", "pending"),
            ("malformed-request", "agent/asyncQuestion", "pending"),
        ):
            agent_id = deleted_agent["id"] if key == "deleted-agent" else (
                malformed_agent["id"] if key == "malformed-agent" else lead["id"]
            )
            runtime.put(db, "requests", {{
                "id":"questions-write-" + key, "agent":agent_id, "method":method,
                "status":status, "createdAt":now,
                "epoch":0,
                "params":({{"mode":"form", "requestedSchema":{{"properties":{{}}}}}}
                          if key == "elicitation" else {{"questions":[{{"id":"value", "question":"Value?"}}]}}),
            }}, sync_rooms=False)
        runtime.put(db, "complaints", {{
            "id":"questions-write-malformed-complaint", "leadId":lead["id"],
            "author":worker["id"], "text":"Malformed", "version":1,
            "status":"open", "created":now, "updated":now, "readAt":None,
            "recipient":"user", "responses":[],
        }}, sync_rooms=False)
        runtime.put(db, "complaints", {{
            "id":"questions-write-existing", "leadId":lead["id"], "author":worker["id"],
            "text":"Please review", "version":1, "status":"open", "created":now,
            "updated":now, "readAt":None, "recipient":"user", "responses":[],
        }}, sync_rooms=False)
        runtime.put(db, "complaints", {{
            "id":"questions-write-missing-author", "leadId":lead["id"], "author":"missing-agent",
            "text":"Please review", "version":1, "status":"open", "created":now,
            "updated":now, "readAt":None, "recipient":"user", "responses":[],
        }}, sync_rooms=False)
        for name, lead_id, responses in (
            ("running", lead["id"], []),
            ("stopped", stopped_lead["id"], []),
            ("deleted", deleted_lead["id"], []),
            ("answered", answered_lead["id"], [{{"id":"lead-answer", "author":answered_lead["id"], "text":"Already handled", "status":"in_progress", "at":now}}]),
        ):
            runtime.put(db, "complaints", {{
                "id":"questions-write-" + name + "-lead", "leadId":lead_id,
                "author":worker["id"], "text":"Owner reply", "version":1,
                "status":"open", "created":now, "updated":now, "readAt":None,
                "recipient":"lead", "responses":responses,
            }}, sync_rooms=False)
    runtime.complaint("questions-write-lead", {{"action":"submit", "text":"Original"}}, "user:migrated-submit", user=True)
    migrated_id = str(uuid.uuid5(uuid.NAMESPACE_URL, "complaint:user:migrated-submit"))
    with runtime.lock, runtime.db() as db:
        migrated = runtime.complaint_detail(migrated_id)
        migrated.update(version=3, status="resolved", responses=[{{"id":"old", "author":"questions-write-lead", "text":"Done", "status":"resolved", "at":now}}])
        runtime.put(db, "complaints", migrated, sync_rooms=False)
        runtime.put(db, "complaints", {{
            "id":"questions-write-migrated-response", "leadId":lead["id"],
            "author":worker["id"], "text":"Please review", "version":1,
            "status":"open", "created":now, "updated":now, "readAt":None,
            "recipient":"user", "responses":[],
        }}, sync_rooms=False)
    runtime.complaint_response_from_user({{
        "action":"respond", "id":"migrated-response-request",
        "lead":"questions-write-lead",
        "complaint_id":"questions-write-migrated-response", "text":"Fixed",
        "status":"resolved", "version":1,
    }}, "user:migrated-response-request")
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=backend._environment(),
        check=True,
        timeout=30,
    )


def state_snapshot(state):
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        tables = [
            row[0]
            for row in database.execute(
                "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name"
            )
        ]
        return {
            table: sorted(
                database.execute(f'SELECT * FROM "{table}"').fetchall(), key=repr
            )
            for table in tables
        }


class QuestionsWriteHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        seed_records(cls.backend)
        cls.backend.start()
        cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def _headers(self):
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def _post(self, path, body):
        return self.backend.request("POST", path, body=body, headers=self._headers())

    def _unchanged_error(self, path, body, status, error):
        before = state_snapshot(self.backend.state)
        response = self._post(path, body)
        self.assertEqual(response.status, status, response.body[:1000])
        self.assertEqual(response.json()["error"], error)
        self.assertEqual(state_snapshot(self.backend.state), before)
        return response

    def test_defer_and_state_only_delete_commit_and_replay(self) -> None:
        before_noop = state_snapshot(self.backend.state)
        no_change = self._post(
            "/api/questions/defer", {"id": "questions-write-defer", "deferred": False}
        )
        self.assertEqual(no_change.status, 200, no_change.body[:1000])
        self.assertEqual(state_snapshot(self.backend.state), before_noop)
        deferred = self._post(
            "/api/questions/defer", {"id": "questions-write-defer", "deferred": True}
        )
        self.assertEqual(deferred.status, 200, deferred.body[:1000])
        self.assertEqual(deferred.json()["id"], "questions-write-defer")
        self.assertEqual(deferred.json()["status"], "pending")
        self.assertTrue(deferred.json()["deferred"])
        before_replay = state_snapshot(self.backend.state)
        self.assertEqual(self._post(
            "/api/questions/defer", {"id": "questions-write-defer", "deferred": True}
        ).status, 200)
        self.assertEqual(state_snapshot(self.backend.state), before_replay)

        deleted = self._post("/api/questions/delete", {"id": "questions-write-delete"})
        self.assertEqual(deleted.status, 200, deleted.body[:1000])
        self.assertEqual(deleted.json()["id"], "questions-write-delete")
        self.assertEqual(deleted.json()["status"], "deleted")
        before_replay = state_snapshot(self.backend.state)
        self.assertEqual(self._post(
            "/api/questions/delete", {"id": "questions-write-delete"}
        ).status, 200)
        self.assertEqual(state_snapshot(self.backend.state), before_replay)

    def test_provider_delivery_refusals_and_invalid_state_write_nothing(self) -> None:
        self._unchanged_error(
            "/api/questions/defer", {"id": "questions-write-answered", "deferred": True},
            400, "This question is no longer pending",
        )
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("provider answer delivery is implemented by the Python backend")
        self._unchanged_error(
            "/api/answer", {"id": "questions-write-native", "decision": "cancel"},
            501, "Not implemented: requires provider-session/question-answer",
        )
        self._unchanged_error(
            "/api/questions/delete", {"id": "questions-write-native"},
            501, "Not implemented: requires provider-session/question-answer",
        )
        self._unchanged_error(
            "/api/questions/delete", {"id": "questions-write-elicitation"},
            501, "Not implemented: requires provider-session/question-answer",
        )
        self._unchanged_error(
            "/api/questions/delete", {"id": "questions-write-answering"},
            400, "Question delivery is uncertain; wait for its receipt",
        )
        self._unchanged_error(
            "/api/questions/delete", {"id": "questions-write-uncertain"},
            400, "Question delivery is uncertain; wait for its receipt",
        )

    def test_deleted_question_agent_refuses_without_writes(self) -> None:
        self._unchanged_error(
            "/api/questions/delete", {"id": "questions-write-deleted-agent"},
            400, "This agent was deleted",
        )

    def test_malformed_stored_rows_name_the_runtime_table_and_id(self) -> None:
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.skipTest("raw malformed-record diagnostics are part of the Rust store contract")
        malformed_json = json.dumps("not a stored record")
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute(
                "UPDATE runtime_requests SET record=? WHERE id=?",
                (malformed_json, "questions-write-malformed-request"),
            )
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (malformed_json, "questions-write-malformed-agent"),
            )
            database.execute(
                "UPDATE runtime_complaints SET record=? WHERE id=?",
                (malformed_json, "questions-write-malformed-complaint"),
            )
            database.commit()
        self._unchanged_error(
            "/api/questions/defer",
            {"id": "questions-write-malformed-request", "deferred": True},
            500,
            "Invalid stored object runtime_requests/questions-write-malformed-request",
        )
        self._unchanged_error(
            "/api/questions/defer",
            {"id": "questions-write-malformed-agent", "deferred": True},
            500,
            "Invalid stored object runtime_agents/questions-write-malformed-agent",
        )
        self._unchanged_error(
            "/api/complaints",
            {"action": "respond", "id": "malformed", "complaint_id": "questions-write-malformed-complaint",
             "text": "Fixed", "status": "resolved", "version": 1},
            500,
            "Invalid stored object runtime_complaints/questions-write-malformed-complaint",
        )

    def test_complaint_submit_matches_python_uuid_and_never_resets_existing_rows(self) -> None:
        expected_id = str(uuid.uuid5(uuid.NAMESPACE_URL, "complaint:user:questions-write-submit"))
        submitted = self._post("/api/complaints", {
            "id": "questions-write-submit", "lead": "questions-write-lead",
            "text": "  Please fix the issue  ",
        })
        self.assertEqual(submitted.status, 200, submitted.body[:1000])
        record = submitted.json()
        self.assertEqual(record["id"], expected_id)
        self.assertEqual(record["text"], "Please fix the issue")
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            row = database.execute(
                "SELECT record FROM runtime_complaints WHERE id=?", (expected_id,)
            ).fetchone()
            event = database.execute(
                "SELECT agent,kind,text,status FROM runtime_events WHERE id=?",
                ("complaint:" + expected_id,),
            ).fetchone()
            receipts = database.execute(
                "SELECT count(*) FROM runtime_operation_receipts WHERE id=?",
                ("user:complaint:user:questions-write-submit",),
            ).fetchone()[0]
        self.assertEqual(json.loads(row[0]), record)
        self.assertEqual(event[0], "questions-write-lead")
        self.assertEqual(event[1], "complaint")
        self.assertEqual(event[3], "pending")
        event_payload = json.loads(event[2])
        self.assertEqual(event_payload, {
            "complaints": [{
                "complaint_id": expected_id, "author": "user", "author_name": "user",
                "text": record["text"], "status": "open", "created": record["created"],
            }],
            "required": (
                "Respond to each complaint with orchestration_complaint action=respond before "
                "finishing. The full complaint is in this message. No separate read call is required."
            ),
        })
        self.assertEqual(receipts, 0)

        replay_before = state_snapshot(self.backend.state)
        replay = self._post("/api/complaints", {
            "id": "questions-write-submit", "lead": "questions-write-lead",
            "text": "Please fix the issue",
        })
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json(), record)
        self.assertEqual(state_snapshot(self.backend.state), replay_before)

        changed_before = state_snapshot(self.backend.state)
        changed = self._post("/api/complaints", {
            "id": "migrated-submit", "lead": "questions-write-lead", "text": "Changed",
        })
        self.assertEqual(changed.status, 400, changed.body[:1000])
        self.assertEqual(changed.json()["error"], "This complaint id has different content")
        self.assertEqual(state_snapshot(self.backend.state), changed_before)
        migrated = self._post("/api/complaints", {
            "id": "migrated-submit", "lead": "questions-write-lead", "text": "Original",
        })
        self.assertEqual(migrated.status, 200, migrated.body[:1000])
        self.assertEqual(migrated.json()["version"], 3)
        self.assertEqual(migrated.json()["status"], "resolved")

    def test_python_response_receipt_replay_and_missing_author_are_read_only(self) -> None:
        before = state_snapshot(self.backend.state)
        replay = self._post("/api/complaints", {
            "action": "respond", "id": "migrated-response-request",
            "lead": "questions-write-lead",
            "complaint_id": "questions-write-migrated-response", "text": "Fixed",
            "status": "resolved", "version": 1,
        })
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["version"], 2)
        self.assertEqual(state_snapshot(self.backend.state), before)
        self._unchanged_error("/api/complaints", {
            "action": "respond", "id": "migrated-response-request",
            "lead": "different-lead",
            "complaint_id": "questions-write-migrated-response", "text": "Fixed",
            "status": "resolved", "version": 1,
        }, 400, "This request id has different content")
        self._unchanged_error("/api/complaints", {
            "action": "respond", "id": "missing-author-request",
            "complaint_id": "questions-write-missing-author", "text": "Fixed",
            "status": "resolved", "version": 1,
        }, 400, "Unknown managed agent")

    def test_response_authorization_tracks_running_stopped_deleted_and_answered_leads(self) -> None:
        self._unchanged_error("/api/complaints", {
            "action": "respond", "id": "running-lead-reply",
            "complaint_id": "questions-write-running-lead", "text": "Closing",
            "status": "resolved", "version": 1,
        }, 400, "This complaint requires a response from its orchestrator")
        for name in ("stopped", "deleted", "answered"):
            response = self._post("/api/complaints", {
                "action": "respond", "id": name + "-lead-reply",
                "complaint_id": "questions-write-" + name + "-lead", "text": "Closing",
                "status": "resolved", "version": 1,
            })
            self.assertEqual(response.status, 200, response.body[:1000])

    def test_event_failure_rolls_back_complaint_and_receipt(self) -> None:
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("""
                CREATE TRIGGER reject_question_complaint_event
                BEFORE INSERT ON runtime_events
                WHEN NEW.id LIKE 'complaint:%'
                BEGIN SELECT RAISE(ABORT, 'injected event failure'); END
            """)
        before = state_snapshot(self.backend.state)
        response = self._post("/api/complaints", {
            "id": "rollback-submit", "lead": "questions-write-lead", "text": "Rollback",
        })
        self.assertIn(response.status, (400, 500), response.body[:1000])
        self.assertEqual(state_snapshot(self.backend.state), before)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            database.execute("DROP TRIGGER reject_question_complaint_event")

    def test_complaint_submit_and_user_response_are_durable_and_replayable(self) -> None:
        submitted = self._post("/api/complaints", {
            "id": "questions-write-submit", "lead": "questions-write-lead",
            "text": "  Please fix the issue  ",
        })
        self.assertEqual(submitted.status, 200, submitted.body[:1000])
        record = submitted.json()
        self.assertEqual(record["text"], "Please fix the issue")
        self.assertEqual(record["recipient"], "lead")
        self.assertEqual(record["status"], "open")
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            count = database.execute(
                "SELECT count(*) FROM runtime_events WHERE id=?", ("complaint:" + record["id"],)
            ).fetchone()[0]
        self.assertEqual(count, 1)
        before_replay = state_snapshot(self.backend.state)
        replay = self._post("/api/complaints", {
            "id": "questions-write-submit", "lead": "questions-write-lead",
            "text": "  Please fix the issue  ",
        })
        self.assertEqual(replay.status, 200, replay.body[:1000])
        self.assertEqual(replay.json()["id"], record["id"])
        self.assertEqual(state_snapshot(self.backend.state), before_replay)
        self._unchanged_error("/api/complaints", {
            "id": "questions-write-submit", "lead": "questions-write-lead", "text": "Changed"
        }, 400, "This complaint id has different content")

        reply = self._post("/api/complaints", {
            "action": "respond", "id": "questions-write-response",
            "complaint_id": "questions-write-existing", "text": "  Fixed  ",
            "status": "resolved", "version": 1,
        })
        self.assertEqual(reply.status, 200, reply.body[:1000])
        self.assertEqual(reply.json()["version"], 2)
        self.assertEqual(reply.json()["responses"][-1]["id"], "user:questions-write-response")
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as database:
            event = database.execute(
                "SELECT kind,status FROM runtime_events WHERE id=?",
                ("complaint-response:user:questions-write-response",),
            ).fetchone()
        self.assertEqual(event, ("complaint_response", "pending"))
        before_replay = state_snapshot(self.backend.state)
        repeated = self._post("/api/complaints", {
            "action": "respond", "id": "questions-write-response",
            "complaint_id": "questions-write-existing", "text": "  Fixed  ",
            "status": "resolved", "version": 1,
        })
        self.assertEqual(repeated.status, 200, repeated.body[:1000])
        self.assertEqual(repeated.json()["version"], reply.json()["version"])
        self.assertEqual(repeated.json()["responses"], reply.json()["responses"])
        self.assertEqual(state_snapshot(self.backend.state), before_replay)
        self._unchanged_error("/api/complaints", {
            "action": "respond", "id": "questions-write-stale",
            "complaint_id": "questions-write-existing", "text": "Stale",
            "status": "resolved", "version": 1,
        }, 409, "This complaint changed. Review the latest response before replying")


if __name__ == "__main__":
    unittest.main(verbosity=2)
