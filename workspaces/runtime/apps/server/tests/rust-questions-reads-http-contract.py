#!/usr/bin/env python3
"""Persisted questions, complaint, and checkpoint reads through both backends."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
import sqlite3
import subprocess
import sys
import unittest
from contextlib import closing
from pathlib import Path


isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import runtime_environment
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def seed_python_records(backend) -> None:
    """Use the Python Runtime record writer to produce persisted fixtures."""
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import json, os, sys, time
from pathlib import Path
sys.path.insert(0, {str(source)!r})
sys.path.insert(0, {str(source.parent / "tests")!r})
from runtime_test_environment import make_runtime
from codex_runtime import Runtime

runtime = make_runtime(Runtime, Path(os.environ["CODEX_AGENTS_STATE_DIR"]), environment=dict(os.environ))
try:
    lead = {{
        "id": "questions-read-lead", "rootId": "questions-read-lead", "epoch": 0,
        "status": "idle", "name": "Question reader lead", "role": "orchestrator",
        "cwd": str(Path(os.environ["CODEX_CANVAS_CWD"])), "isLead": True,
        "autoWake": True, "approvalPolicy": "never",
    }}
    worker = {{**lead, "id": "questions-read-worker", "rootId": lead["id"],
              "name": "Question reader worker", "isLead": False}}
    deleted = {{**lead, "id": "questions-read-deleted", "rootId": lead["id"],
                "deletedAt": 1.0}}
    external = {{**lead, "id": "questions-read-external", "rootId": "questions-read-external",
                "name": "Other team", "isLead": False}}
    now = time.time()
    with runtime.lock, runtime.db() as db:
        for agent in (lead, worker, deleted, external):
            runtime.put(db, "agents", agent, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-old", "agent": lead["id"],
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": now - 30,
            "params": {{"questions": [{{"id": "name", "question": "Your name?"}}]}},
            "answerHistory": [{{"id": "name", "question": "Your name?", "isSecret": False, "answer": 9007199254740993}}],
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-new", "agent": worker["id"],
            "method": "mcpServer/elicitation/request", "status": "answered", "createdAt": now,
            "params": {{"mode": "form", "requestedSchema": {{"properties": {{
                "password": {{"title": "Password", "format": "password"}},
                "color": {{"title": "Color"}},
            }}}}}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-hidden", "agent": deleted["id"],
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": now,
            "params": {{"questions": [{{"id": "hidden"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-deleted", "agent": lead["id"], "deletedAt": 1.0,
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": 100.0,
            "params": {{"questions": [{{"id": "gone"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-external", "agent": external["id"],
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": now + 100,
            "params": {{"questions": [{{"id": "external"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-tie-first", "agent": lead["id"],
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": now + 10,
            "params": {{"questions": [{{"id": "first"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-tie-second", "agent": lead["id"],
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": now + 10,
            "params": {{"questions": [{{"id": "second"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-answered-early", "agent": lead["id"],
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": now + 100,
            "answeredAt": now - 40, "params": {{"questions": [{{"id": "early"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "requests", {{
            "id": "questions-read-zero-answer", "agent": lead["id"],
            "method": "agent/asyncQuestion", "status": "answered", "createdAt": now + 5,
            "answeredAt": 0, "params": {{"questions": [{{"id": "zero"}}]}},
        }}, sync_rooms=False)
        runtime.put(db, "complaints", {{
            "id": "questions-read-complaint", "leadId": lead["id"], "author": "user",
            "text": "Please review this", "version": 1, "status": "open",
            "created": 4.0, "updated": 4.0, "readAt": None, "recipient": "lead", "responses": [],
        }}, sync_rooms=False)
        runtime.put(db, "checkpoints", {{
            "id": "questions-read-checkpoint", "agent": lead["id"], "rootId": lead["id"],
            "label": "Python-written checkpoint", "tree": "abc", "created": 5.0,
            "items": [{{"path": "private.txt", "content": "not returned"}}],
            "historyDelta": [{{"secret": "not returned"}}],
            "extension": 9007199254740993,
        }}, sync_rooms=False)
        runtime.put(db, "checkpoints", {{
            "id": "questions-read-deleted-checkpoint", "agent": deleted["id"],
            "rootId": lead["id"], "label": "Deleted caller checkpoint", "created": 6.0,
        }}, sync_rooms=False)
        runtime.put(db, "checkpoints", {{
            "id": "questions-read-external-checkpoint", "agent": external["id"],
            "rootId": external["id"], "label": "Other team checkpoint", "created": 7.0,
        }}, sync_rooms=False)
    expected = {{
        "questions": runtime.question_history(lead["id"]),
        "complaint": runtime.complaint_detail("questions-read-complaint"),
        "checkpoints": {{"checkpoints": runtime.workspace_snapshot(lead["id"])["checkpoints"]}},
    }}
    (Path(os.environ["CODEX_CANVAS_CWD"]) / "questions-read-python-oracle.json").write_text(
        json.dumps(expected), encoding="utf-8"
    )
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=runtime_environment(backend),
        check=True,
        timeout=30,
    )


def state_snapshot(state):
    with closing(sqlite3.connect(state / "canvas.sqlite3")) as database:
        schema = database.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name"
        ).fetchall()
        tables = [row[1] for row in schema if row[0] == "table"]
        rows = {
            table: sorted(
                database.execute(f' SELECT * FROM "{table.replace(chr(34), chr(34) * 2)}"').fetchall(),
                key=repr,
            )
            for table in tables
        }
        return schema, rows


class QuestionsReadsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        seed_python_records(cls.backend)
        cls.backend.start()
        cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)

    def _headers(self):
        backend = self.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def _read(self, method, path, headers):
        before = state_snapshot(self.backend.state)
        response = self.backend.request(method, path, headers=headers)
        self.assertEqual(state_snapshot(self.backend.state), before, f"read wrote state: {method} {path}")
        return response

    def test_reads_python_written_questions_complaint_and_checkpoint(self) -> None:
        headers = self._headers()
        expected = json.loads((self.backend.cwd / "questions-read-python-oracle.json").read_text())
        missing_agent = self._read("GET", "/api/questions", headers)
        self.assertEqual(missing_agent.status, 400, missing_agent.body[:1000])
        self.assertEqual(missing_agent.json()["error"], "A conversation is required")
        deleted_questions = self._read("GET", "/api/questions?agent=questions-read-deleted", headers)
        self.assertEqual(deleted_questions.status, 400)
        self.assertEqual(deleted_questions.json()["error"], "This agent was deleted")
        questions = self._read("GET", "/api/questions?agent=questions-read-lead", headers)
        self.assertEqual(questions.status, 200, questions.body[:1000])
        self.assertEqual(questions.json(), expected["questions"])
        items = questions.json()["items"]
        self.assertEqual([item["id"] for item in items], [
            "questions-read-tie-first", "questions-read-tie-second", "questions-read-zero-answer", "questions-read-new",
            "questions-read-old", "questions-read-answered-early",
        ])
        by_id = {item["id"]: item for item in items}
        fields = {field["id"]: field for field in by_id["questions-read-new"]["questions"]}
        self.assertEqual(
            [field["id"] for field in by_id["questions-read-new"]["questions"]],
            ["password", "color"],
        )
        self.assertTrue(fields["password"]["isSecret"])
        self.assertFalse(fields["color"]["isSecret"])
        self.assertEqual(by_id["questions-read-old"]["answerHistory"][0]["answer"], 9007199254740993)

        complaint = self._read("GET", "/api/complaint?id=questions-read-complaint", headers)
        self.assertEqual(complaint.status, 200, complaint.body[:1000])
        self.assertEqual(complaint.json(), expected["complaint"])
        missing_complaint = self._read("GET", "/api/complaint?id=questions-read-missing", headers)
        self.assertEqual(missing_complaint.status, 400, missing_complaint.body[:1000])
        self.assertEqual(missing_complaint.json()["error"], "Unknown complaint")

        deleted_checkpoints = self._read("GET", "/api/checkpoints?agent=questions-read-deleted", headers)
        self.assertEqual(deleted_checkpoints.status, 400)
        self.assertEqual(deleted_checkpoints.json()["error"], "This agent was deleted")
        checkpoint = self._read("GET", "/api/checkpoints?agent=questions-read-lead", headers)
        self.assertEqual(checkpoint.status, 200, checkpoint.body[:1000])
        self.assertEqual(checkpoint.json(), expected["checkpoints"])
        value = checkpoint.json()["checkpoints"][0]
        self.assertEqual(value["label"], "Python-written checkpoint")
        self.assertEqual(value["extension"], 9007199254740993)
        for field in ("items", "historyDelta"):
            self.assertNotIn(field, value)
        repeated = self._read(
            "GET", "/api/checkpoints?agent=questions-read-lead",
            {**headers, "If-None-Match": checkpoint.headers.get("etag", "")},
        )
        self.assertEqual(repeated.status, 304, repeated.body[:1000])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_records_are_refused_without_writes(self) -> None:
        backend = self.backend
        database_path = backend.state / "canvas.sqlite3"
        cases = (
            ("runtime_requests", "questions-read-new", "status", 123),
            ("runtime_complaints", "questions-read-complaint", "text", 17),
            ("runtime_checkpoints", "questions-read-checkpoint", "created", "invalid"),
        )
        for table, record_id, field, invalid in cases:
            with sqlite3.connect(database_path) as database:
                original = database.execute(
                    f"SELECT record FROM {table} WHERE id=?", (record_id,),
                ).fetchone()[0]
                value = json.loads(original)
                value[field] = invalid
                database.execute(
                    f"UPDATE {table} SET record=? WHERE id=?",
                    (json.dumps(value), record_id),
                )
            before = state_snapshot(backend.state)
            path = {
                "runtime_requests": "/api/questions?agent=questions-read-lead",
                "runtime_complaints": "/api/complaint?id=questions-read-complaint",
                "runtime_checkpoints": "/api/checkpoints?agent=questions-read-lead",
            }[table]
            try:
                response = backend.request("GET", path, headers=self._headers())
                self.assertEqual(response.status, 500, response.body[:1000])
                self.assertEqual(response.json()["error"], f"Invalid stored object {table.removeprefix('runtime_')}/{record_id}")
                self.assertEqual(state_snapshot(backend.state), before)
            finally:
                with sqlite3.connect(database_path) as database:
                    database.execute(
                        f"UPDATE {table} SET record=? WHERE id=?",
                        (original, record_id),
                    )


if __name__ == "__main__":
    unittest.main()
