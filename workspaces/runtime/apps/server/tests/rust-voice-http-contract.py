#!/usr/bin/env python3
"""Voice record, history, approval and durable-delivery HTTP contracts."""

from __future__ import annotations

import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import unittest

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

SERVER_SOURCE = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SERVER_SOURCE))

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def seed_python_voice_rows(backend) -> None:
    script = f"""
import importlib.util, json, sqlite3
from pathlib import Path
import sys
tests = Path({str(Path(__file__).resolve().parent)!r})
sys.path.insert(0, str(tests))
source = tests / "runtime-contract.py"
spec = importlib.util.spec_from_file_location("voice_runtime_fixture", source)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
class QuietRuntime(fixture.Runtime):
    def schedule(self):
        pass
runtime = QuietRuntime(Path({str(backend.state)!r}), fixture.FakeServer)
agent = runtime.create({{
    "name": "Voice projection contract agent", "cwd": {str(backend.cwd)!r},
    "prompt": "Seed an idle orchestrator for voice HTTP contracts.", "autoWake": True,
}}, defer=True)
agent.update(autoWake=True, status="idle")
with runtime.db() as connection:
    runtime.put(connection, "agents", agent)
agent_id = agent["id"]
sys.path.insert(0, {str(SERVER_SOURCE)!r})
from codex_voice import VoiceStore
voice = VoiceStore(runtime)
voice._publish_voice = lambda agent: None
with runtime.db() as connection:
    connection.execute(
        "INSERT INTO voice_sessions(id,agent,created,ended,answer,state,error) "
        "VALUES('python-session',?,1,2,NULL,'lost','transcript retained')",
        (agent_id,),
    )
voice.record(
    agent_id, "python-session", "python-user-record", "user",
    "Read the stored Python transcript.", payload={{"utterance_order": 1}},
)
voice.record(
    agent_id, "python-session", "python-courier-record", "courier",
    "Courier reply.", payload={{"utterance_order": 2}},
)
runtime.close()
"""
    backend.stop()
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", script],
        cwd=backend.cwd,
        env=backend._environment(),
        check=True,
        timeout=30,
    )
    backend.start()
    backend.ready_samples.append(backend.ready_seconds or 0)
    with sqlite3.connect(backend.state / "canvas.sqlite3") as connection:
        agent_id = connection.execute(
            "SELECT id FROM runtime_agents WHERE json_extract(record,'$.isLead')=1 "
            "ORDER BY rowid LIMIT 1"
        ).fetchone()[0]
        request = {
            "id": "python-approval",
            "method": "item/commandExecution/requestApproval",
            "agent": agent_id,
            "status": "pending",
            "params": {"command": "echo safe"},
        }
        connection.execute(
            "INSERT INTO runtime_requests(id,record) VALUES(?,?)",
            (request["id"], json.dumps(request)),
        )


def voice_snapshot(state: Path):
    with sqlite3.connect(f"file:{state / 'canvas.sqlite3'}?mode=ro", uri=True) as connection:
        tables = {
            name: connection.execute(f"SELECT * FROM {name} ORDER BY rowid").fetchall()
            for name in (
                "voice_sessions", "voice_records", "voice_deliveries",
                "voice_approvals", "runtime_events",
            )
        }
    audio = state / "voice-audio"
    return {"tables": tables, "audio": sorted(path.name for path in audio.iterdir()) if audio.exists() else []}


class VoiceHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)
        with sqlite3.connect(cls.backend.state / "canvas.sqlite3") as connection:
            cls.cold_voice_catalog = connection.execute(
                "SELECT type,name,tbl_name,sql FROM sqlite_master "
                "WHERE name LIKE 'voice_%' ORDER BY type,name"
            ).fetchall()
        seed_python_voice_rows(cls.backend)
        with sqlite3.connect(cls.backend.state / "canvas.sqlite3") as connection:
            cls.lead_id = connection.execute(
                "SELECT json_extract(record,'$.id') FROM runtime_agents "
                "WHERE json_extract(record,'$.isLead')=1 ORDER BY rowid LIMIT 1"
            ).fetchone()[0]

    @classmethod
    def headers(cls):
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    def request(self, path: str, body: dict):
        return self.backend.request("POST", path, body=body, headers=self.headers())

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_rust_bootstraps_voice_tables_before_python_voice_store_runs(self):
        self.assertEqual(
            {row[1] for row in self.cold_voice_catalog},
            {
                "voice_approvals", "voice_deliveries", "voice_records",
                "voice_records_agent", "voice_sessions", "voice_speech_requests",
            },
        )

    def test_reads_python_written_voice_records_and_pending_approvals(self):
        status = self.request("/api/voice/status", {"agent": self.lead_id})
        self.assertEqual(status.status, 200, status.body[:1000])
        self.assertEqual(status.json(), {
            "configured": True, "transport": "native", "auth": "chatgpt",
        })

        history = self.request("/api/voice/records", {"agent": self.lead_id, "after": 0})
        self.assertEqual(history.status, 200, history.body[:1000])
        self.assertEqual(
            [record["id"] for record in history.json()["records"]],
            ["python-user-record", "python-courier-record"],
        )
        session = history.json()["session"]
        self.assertEqual(session["session_id"], "python-session")
        self.assertEqual(session["state"], "lost")
        self.assertIsNone(session["sdp"])
        self.assertIsInstance(session["error"], str)
        self.assertIsInstance(session["ended"], (int, float))

        approvals = self.request("/api/voice/approvals", {"agent": self.lead_id})
        self.assertEqual(approvals.status, 200, approvals.body[:1000])
        self.assertEqual([item["id"] for item in approvals.json()["requests"]], ["python-approval"])

    def test_record_retry_is_idempotent_and_changed_identity_is_refused(self):
        body = {
            "agent": self.lead_id, "session_id": "python-session", "event_id": "new-event",
            "kind": "event", "text": "Stored event", "item_id": "", "previous_item_id": "",
        }
        first = self.request("/api/voice/record", body)
        self.assertEqual(first.status, 200, first.body[:1000])
        retry = self.request("/api/voice/record", body)
        self.assertEqual(retry.status, 200, retry.body[:1000])
        self.assertEqual(first.json(), retry.json())

        before = voice_snapshot(self.backend.state)
        conflict = self.request("/api/voice/record", {**body, "text": "Changed"})
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(voice_snapshot(self.backend.state), before)

    def test_submit_commits_delivery_and_event_once_and_refusals_do_not_write(self):
        body = {
            "agent": self.lead_id, "message_id": "voice-message",
            "record_ids": ["python-user-record", "python-courier-record"],
        }
        first = self.request("/api/voice/submit", body)
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(first.json()["status"], "queued")
        self.assertEqual(first.json()["id"], "voice-message")
        retry = self.request("/api/voice/submit", body)
        self.assertEqual(retry.status, 200, retry.body[:1000])
        self.assertEqual(retry.json()["id"], first.json()["id"])
        self.assertIn(retry.json()["status"], {
            "pending", "reserved", "dispatching", "uncertain", "failed",
            "cancelled", "queued", "delivered", "stored_only",
        })
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            delivery_count = connection.execute(
                "SELECT COUNT(*) FROM voice_deliveries WHERE id='voice-message'"
            ).fetchone()[0]
            event_count = connection.execute(
                "SELECT COUNT(*) FROM runtime_events WHERE id='voice-message'"
            ).fetchone()[0]
            event_text = connection.execute(
                "SELECT text FROM runtime_events WHERE id='voice-message'"
            ).fetchone()[0]
        self.assertEqual(delivery_count, 1)
        self.assertEqual(event_count, 1)
        self.assertIn("Read the stored Python transcript.", event_text)
        self.assertIn("Courier reply.", event_text)

        before = voice_snapshot(self.backend.state)
        refused = self.request("/api/voice/submit", {
            "agent": self.lead_id, "message_id": "missing-message", "record_ids": ["missing-record"],
        })
        self.assertEqual(refused.status, 400, refused.body[:1000])
        self.assertEqual(voice_snapshot(self.backend.state), before)

    def test_unknown_voice_action_keeps_the_python_error_contract(self):
        before = voice_snapshot(self.backend.state)
        response = self.request("/api/voice/not-a-route", {"anything": True})
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json()["error"], "Unknown voice action")
        self.assertEqual(voice_snapshot(self.backend.state), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_provider_and_audio_operations_refuse_without_writes(self):
        before = voice_snapshot(self.backend.state)
        requests = (
            ("start", {"agent": self.lead_id, "session_id": "unused", "sdp": "offer"}),
            ("end", {"agent": self.lead_id, "session_id": "python-session"}),
            ("speech", {
                "agent": self.lead_id, "session_id": "python-session", "record_id": "python-courier-record",
            }),
            ("audio", {
                "agent": self.lead_id, "session_id": "python-session", "chunk_id": "unused",
                "audio": "", "mime": "audio/webm",
            }),
            ("approval_speech", {"agent": self.lead_id, "request_id": "python-approval"}),
            ("approve", {
                "agent": self.lead_id, "session_id": "python-session", "speech_id": "unused",
                "transcript_id": "unused",
            }),
        )
        for action, body in requests:
            with self.subTest(action=action):
                response = self.request(f"/api/voice/{action}", body)
                self.assertEqual(response.status, 501, response.body[:1000])
                self.assertIn("Not implemented: requires ", response.json()["error"])
                self.assertEqual(voice_snapshot(self.backend.state), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_voice_record_is_refused_without_writes(self):
        self.backend.stop()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            connection.execute(
                "UPDATE voice_records SET payload='not-json' WHERE id='python-user-record'"
            )
        before = voice_snapshot(self.backend.state)
        self.backend.start()
        self.backend.ready_samples.append(self.backend.ready_seconds or 0)
        response = self.request("/api/voice/records", {"agent": self.lead_id, "after": 0})
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn("voice_records", response.json()["error"])
        self.assertEqual(voice_snapshot(self.backend.state), before)
        self.backend.stop()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            connection.execute(
                "UPDATE voice_records SET payload=? WHERE id='python-user-record'",
                (json.dumps({"utterance_order": 1}),),
            )
        self.backend.start()
        self.backend.ready_samples.append(self.backend.ready_seconds or 0)


if __name__ == "__main__":
    unittest.main(verbosity=2)
