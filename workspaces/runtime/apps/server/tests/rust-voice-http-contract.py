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

from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot


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
voice.record(
    agent_id, "python-session", "python-stopped-agent-record", "user",
    "Wake the stopped agent.", payload={{"utterance_order": 3}},
)
with runtime.db() as connection:
    connection.execute(
        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) "
        "VALUES(?,?,?,?,?,?,?,?,?)",
        ("conflicting-message", agent_id, "user", "Different transcript", "pending", 1, 0, None, None),
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
    if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
        backend.hold_provider("thread/start")
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
    def tearDownClass(cls) -> None:
        if not cls.rust_backend:
            cls.backend.release_provider_gate()
        cls.backend.stop()

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
        record_ids = [record["id"] for record in history.json()["records"]]
        self.assertEqual(record_ids[:3], [
            "python-user-record", "python-courier-record", "python-stopped-agent-record",
        ])
        self.assertTrue(all(record_id.startswith("approval-speech:") for record_id in record_ids[3:]))
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

        before = no_write_snapshot(self.backend.state)
        conflict = self.request("/api/voice/record", {**body, "text": "Changed"})
        self.assertEqual(conflict.status, 400, conflict.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_submit_commits_delivery_and_event_once_and_refusals_do_not_write(self):
        body = {
            "agent": self.lead_id, "message_id": "voice-message",
            "record_ids": ["python-user-record", "python-courier-record"],
        }
        first = self.request("/api/voice/submit", body)
        self.assertEqual(first.status, 200, first.body[:1000])
        self.assertEqual(first.json()["status"], "queued")
        self.assertEqual(first.json()["id"], "voice-message")
        self.backend.wait_for_agent_preparation(self.lead_id)
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
        self.assertEqual(
            event_text,
            "Voice conversation, full transcript (speech recognition can contain errors):\n\n"
            "User: Read the stored Python transcript.\n\nVoice courier: Courier reply.",
        )
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            delivery = connection.execute(
                "SELECT id,agent,records,text,edited_text FROM voice_deliveries WHERE id='voice-message'"
            ).fetchone()
            metadata = json.loads(connection.execute(
                "SELECT record FROM runtime_event_meta WHERE id='voice-message'"
            ).fetchone()[0])
            receipt = connection.execute(
                "SELECT id,signature,result FROM runtime_operation_receipts WHERE id='voice-message'"
            ).fetchone()
            event = connection.execute(
                "SELECT id,agent,kind,text,status,error FROM runtime_events WHERE id='voice-message'"
            ).fetchone()
        self.assertEqual(delivery, (
            "voice-message", self.lead_id,
            json.dumps(["python-user-record", "python-courier-record"])
            if not self.rust_backend else '["python-user-record","python-courier-record"]',
            event_text, None,
        ))
        self.assertEqual(metadata.get("assets"), [])
        self.assertEqual(metadata.get("delivery"), "queue")
        self.assertIsInstance(metadata.get("acceptedAt"), (int, float))
        self.assertEqual(event[:4], ("voice-message", self.lead_id, "user", event_text))
        self.assertIn(event[4], {
            "pending", "reserved", "dispatching", "uncertain", "cancelled",
        })
        self.assertIsNone(event[5])
        if self.rust_backend:
            signature_source = json.dumps(
                {"agent": self.lead_id, "text": event_text, "assets": []},
                sort_keys=True, ensure_ascii=False,
            )
            self.assertEqual(receipt[0], "voice-message")
            self.assertEqual(receipt[1], signature_source)
            self.assertEqual(json.loads(receipt[2]), {"id": "voice-message", "status": "queued"})

        before = no_write_snapshot(self.backend.state)
        refused = self.request("/api/voice/submit", {
            "agent": self.lead_id, "message_id": "missing-message", "record_ids": ["missing-record"],
        })
        self.assertEqual(refused.status, 400, refused.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_request_and_ownership_refusals_do_not_write(self):
        before = no_write_snapshot(self.backend.state)
        cases = (
            ("/api/voice/status", {"agent": "voice-nonlead-owner"}),
            ("/api/voice/record", {
                "agent": self.lead_id, "session_id": "", "event_id": "", "kind": "event",
            }),
            ("/api/voice/record", {
                "agent": self.lead_id, "session_id": "other-session", "event_id": "wrong-session",
                "kind": "event",
            }),
            ("/api/voice/submit", {
                "agent": self.lead_id, "message_id": "wrong-owner", "record_ids": ["missing-owner-record"],
            }),
        )
        for path, body in cases:
            with self.subTest(path=path, body=body):
                response = self.request(path, body)
                self.assertEqual(response.status, 400, response.body[:1000])
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_submit_manually_wakes_a_stopped_agent(self):
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            record = json.loads(connection.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.lead_id,),
            ).fetchone()[0])
            record["autoWake"] = False
            record["error"] = "previous failure"
            record["nativeFailureHold"] = True
            record["complaintMisses"] = 3
            connection.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(record), self.lead_id),
            )
        before = no_write_snapshot(self.backend.state)
        response = self.request("/api/voice/submit", {
            "agent": self.lead_id, "message_id": "wake-voice-agent",
            "record_ids": ["python-stopped-agent-record"],
        })
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.json()["status"], "queued")
        self.backend.wait_for_agent_preparation(self.lead_id)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            agent = json.loads(connection.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (self.lead_id,),
            ).fetchone()[0])
        self.assertTrue(agent["autoWake"])
        self.assertIsNone(agent.get("error"))
        self.assertNotIn("nativeFailureHold", agent)
        self.assertEqual(agent["complaintMisses"], 0)
        after = no_write_snapshot(self.backend.state)
        self.assertNotEqual(after, before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_existing_event_conflict_and_post_delivery_failure_roll_back(self):
        before = no_write_snapshot(self.backend.state)
        response = self.request("/api/voice/submit", {
            "agent": self.lead_id, "message_id": "conflicting-message",
            "record_ids": ["python-user-record"],
        })
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            connection.execute(
                "CREATE TRIGGER fail_voice_event BEFORE INSERT ON runtime_events "
                "WHEN NEW.id='fail-after-delivery' BEGIN SELECT RAISE(ABORT, 'injected'); END"
            )
        before = no_write_snapshot(self.backend.state)
        failed = self.request("/api/voice/submit", {
            "agent": self.lead_id, "message_id": "fail-after-delivery",
            "record_ids": ["python-user-record"],
        })
        self.assertEqual(failed.status, 500, failed.body[:1000])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_unknown_voice_action_keeps_the_python_error_contract(self):
        before = no_write_snapshot(self.backend.state)
        audio_before = self._audio_snapshot()
        response = self.request("/api/voice/not-a-route", {"anything": True})
        self.assertEqual(response.status, 400, response.body[:1000])
        self.assertEqual(response.json()["error"], "Unknown voice action")
        after = no_write_snapshot(self.backend.state)
        self.assertEqual(after, before)
        non_object = self.backend.request(
            "POST", "/api/voice/not-a-route", body=[1], headers=self.headers(),
        )
        self.assertEqual(non_object.status, 400, non_object.body[:1000])
        missing = self.backend.request(
            "POST", "/api/voice/not-a-route", headers=self.headers(),
        )
        self.assertEqual(missing.status, 415, missing.body[:1000])
        after = no_write_snapshot(self.backend.state)
        self.assertEqual(after, before)
        self.assertEqual(self._audio_snapshot(), audio_before)

    def _audio_snapshot(self) -> list[str]:
        audio = self.backend.state / "voice-audio"
        return sorted(path.name for path in audio.iterdir()) if audio.exists() else []

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_provider_and_audio_operations_refuse_without_writes(self):
        before = no_write_snapshot(self.backend.state)
        requests = (
            ("start", {"agent": self.lead_id, "session_id": "unused", "sdp": "offer"}),
            ("audio", {
                "agent": self.lead_id, "session_id": "python-session", "chunk_id": "unused",
                "audio": "", "mime": "audio/webm",
            }),
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
                self.assertEqual(no_write_snapshot(self.backend.state), before)
        ended = self.request("/api/voice/end", {
            "agent": self.lead_id, "session_id": "python-session",
        })
        self.assertEqual(ended.status, 200, ended.body[:1000])
        self.assertEqual(ended.json()["state"], "lost")

        self.backend.stop()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            connection.execute(
                "INSERT INTO voice_speech_requests(id,session,record_id,state,error) "
                "VALUES(?,?,?,?,?)",
                ("python-session:python-courier-record", "python-session", "python-courier-record", "failed", "saved failure"),
            )
        self.backend.start()
        self.backend.ready_samples.append(self.backend.ready_seconds or 0)
        receipt = self.request("/api/voice/speech", {
            "agent": self.lead_id, "session_id": "python-session",
            "record_id": "python-courier-record",
        })
        self.assertEqual(receipt.status, 200, receipt.body[:1000])
        self.assertEqual(receipt.json(), {
            "id": "python-session:python-courier-record", "session": "python-session",
            "record_id": "python-courier-record", "state": "failed", "error": "saved failure",
        })
        speech = self.request("/api/voice/approval_speech", {
            "agent": self.lead_id, "request_id": "python-approval",
        })
        self.assertEqual(speech.status, 200, speech.body[:1000])
        self.assertEqual(speech.json()["kind"], "orchestrator")
        approval_payload = json.loads(speech.json()["payload"])
        self.assertEqual(approval_payload["request_id"], "python-approval")
        self.assertRegex(approval_payload["fingerprint"], r"^[0-9a-f]{64}$")

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_voice_record_is_refused_without_writes(self):
        self.backend.stop()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as connection:
            connection.execute(
                "UPDATE voice_records SET payload='not-json' WHERE id='python-user-record'"
            )
        before = no_write_snapshot(self.backend.state)
        self.backend.start()
        self.backend.ready_samples.append(self.backend.ready_seconds or 0)
        response = self.request("/api/voice/records", {"agent": self.lead_id, "after": 0})
        self.assertEqual(response.status, 500, response.body[:1000])
        self.assertIn("voice_records", response.json()["error"])
        self.assertIn("python-user-record", response.json()["error"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
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
