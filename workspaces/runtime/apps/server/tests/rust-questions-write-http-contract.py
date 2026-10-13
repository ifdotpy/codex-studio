#!/usr/bin/env python3
"""Question state writes, provider refusal, and durable complaint events."""

from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
import unittest
from contextlib import closing
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def seed_records(backend) -> None:
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import os, sys, time
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime

runtime = Runtime(Path(os.environ["CODEX_AGENTS_STATE_DIR"]))
try:
    lead = {{"id":"questions-write-lead", "rootId":"questions-write-lead", "epoch":0,
        "status":"idle", "name":"Question write lead", "role":"orchestrator",
        "cwd":str(Path(os.environ["CODEX_CANVAS_CWD"])), "isLead":True,
        "autoWake":True, "approvalPolicy":"never"}}
    worker = {{**lead, "id":"questions-write-worker", "rootId":lead["id"], "isLead":False}}
    now = time.time()
    with runtime.lock, runtime.db() as db:
        for agent in (lead, worker):
            runtime.put(db, "agents", agent, sync_rooms=False)
        for key, method, status in (
            ("defer", "agent/asyncQuestion", "pending"),
            ("delete", "agent/asyncQuestion", "pending"),
            ("native", "item/tool/requestUserInput", "pending"),
            ("answered", "agent/asyncQuestion", "answered"),
        ):
            runtime.put(db, "requests", {{
                "id":"questions-write-" + key, "agent":lead["id"], "method":method,
                "status":status, "createdAt":now,
                "epoch":0,
                "params":{{"questions":[{{"id":"value", "question":"Value?"}}]}},
            }}, sync_rooms=False)
        runtime.put(db, "complaints", {{
            "id":"questions-write-existing", "leadId":lead["id"], "author":worker["id"],
            "text":"Please review", "version":1, "status":"open", "created":now,
            "updated":now, "readAt":None, "recipient":"user", "responses":[],
        }}, sync_rooms=False)
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
