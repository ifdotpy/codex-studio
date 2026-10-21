#!/usr/bin/env python3
"""Message acceptance/replay and read visibility through both HTTP backends."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import sys
import unittest
from urllib.parse import urlencode

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from http_backend_harness import (
    TimedHTTPContractTest, assert_stored_data_unavailable, no_write_snapshot,
    start_contract_backend,
)

_spec = importlib.util.spec_from_file_location("p03_message_fixture", Path(__file__).with_name("rust-messages-http-contract.py"))
_fixture = importlib.util.module_from_spec(_spec)
assert _spec.loader is not None
_spec.loader.exec_module(_fixture)
from codex_shared_values import MAX_TURN_ATTACHMENTS


class MessageIngressHTTP(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def assert_rust_recipient_held(cls):
        with sqlite3.connect(cls.backend.state / "canvas.sqlite3") as db:
            record = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (cls.agent,)).fetchone()[0])
        state = (record.get("status"), record.get("autoWake"), record.get("inFlight"))
        expected = ("approval", True, False)
        if state != expected:
            raise AssertionError(f"Rust P03 recipient state must stay held: {state!r}")

    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls, hold_provider_method="thread/start")
        cls.agent = _fixture.seed_agent(cls.backend)
        cls.foreign = _fixture.seed_agent(cls.backend)
        class QuietRuntime(_fixture._fixture.Runtime):
            def schedule(self):
                pass
        runtime = _fixture.make_runtime(QuietRuntime, cls.backend, _fixture._fixture.FakeServer)
        try:
            with runtime.db() as db:
                runtime.put(db, "rooms", {"id": "p03-private", "kind": "private", "members": [cls.agent], "name": "Message room", "updated": 100.0})
                db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES(?,?,?,?,?,?)",
                           ("p03-chat-message", "p03-private", cls.agent, "Persist this history", 100.0, "{}"))
        finally:
            runtime.close()
        cls.backend.start()
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            # Apply the Rust-only hold after startup recovery and before any
            # HTTP contract call. Approval keeps accepted messages unqueued.
            _fixture.update_agent(
                cls.backend, cls.agent,
                {"status": "approval", "autoWake": True, "inFlight": False},
            )
            cls.assert_rust_recipient_held()
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            response = cls.backend.request("POST", "/api/messages", body={
                "id": "06060606-0606-0606-0606-060606060606", "room": cls.agent, "text": "Hold provider preparation",
            }, headers={"Origin": cls.backend.origin or "", "X-Canvas-Token": cls.backend.session["token"],
                        "X-Studio-API-Schema": cls.backend.schema_hash})
            if response.status != 200:
                raise AssertionError(response.body)
            cls.backend.wait_until(cls.backend.provider_gate_started, "provider preparation gate")

    @classmethod
    def tearDownClass(cls):
        cls.backend.stop()

    def request(self, method, path, body=None):
        return self.backend.request(method, path, body=body, headers={
            "Origin": self.backend.origin or "", "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        })

    def test_send_replays_current_receipt_and_changed_content_writes_nothing(self):
        body = {"id": "03030303-0303-0303-0303-030303030303", "room": self.agent, "text": " hello "}
        result = self.request("POST", "/api/messages", body)
        self.assertEqual(result.status, 200, result.body)
        self.assertEqual(result.json()["id"], body["id"])
        self.assertEqual(result.json()["status"], "queued")
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            from codex_sync_entities import register_functions
            register_functions(db)
            db.execute("UPDATE runtime_events SET status='uncertain',error='Outcome unknown' WHERE id=?", (body["id"],))
        before = no_write_snapshot(self.backend.state)
        replay = self.request("POST", "/api/messages", body)
        self.assertEqual(replay.status, 200, replay.body)
        self.assertEqual({key: value for key, value in replay.json().items() if key != "_syncEntities"},
                         {"id": body["id"], "status": "uncertain", "error": "Outcome unknown"})
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        receipts = self.request("GET", "/api/messages/receipts?" + urlencode({"agent": self.agent, "ids": json.dumps([body["id"], body["id"], "missing"])}))
        self.assertEqual(receipts.status, 200, receipts.body)
        self.assertEqual(receipts.json()["items"], [{"id": body["id"], "status": "uncertain", "error": "Outcome unknown", "materialized": False}])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        for change in ({"text": "changed"}, {"assets": ["changed"]}, {"delivery": "next_turn"}):
            refused = self.request("POST", "/api/messages", {**body, **change})
            self.assertEqual(refused.status, 400, refused.body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_bounded_attachment_ids_are_accepted_before_preparation_validation(self):
        for index, assets in enumerate((["unknown-upload"], [str(n) for n in range(MAX_TURN_ATTACHMENTS)])):
            body = {"id": f"04040404-0404-0404-0404-{index:012d}", "room": self.agent, "text": "", "assets": assets}
            response = self.request("POST", "/api/messages", body)
            self.assertEqual(response.status, 200, response.body)
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                metadata = json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (body["id"],)).fetchone()[0])
            self.assertEqual(metadata["assets"], assets)
            before = no_write_snapshot(self.backend.state)
            replay = self.request("POST", "/api/messages", body)
            self.assertEqual(replay.status, 200, replay.body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_attachment_count_over_limit_is_refused_without_writes(self):
        before = no_write_snapshot(self.backend.state)
        body = {"id": "04040404-0404-0404-0404-999999999999", "room": self.agent, "text": "",
                "assets": [str(n) for n in range(MAX_TURN_ATTACHMENTS + 1)]}
        refused = self.request("POST", "/api/messages", body)
        self.assertEqual(refused.status, 400, refused.body)
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_uploaded_attachment_replay_and_foreign_owner_keep_python_timing(self):
        for index, owner in enumerate((self.agent, self.foreign)):
            asset_id = f"07070707-0707-0707-0707-{index:012d}"
            upload = {"id": asset_id, "agent": owner, "name": "note.txt", "base64": "bm90ZQ=="}
            saved = self.request("POST", "/api/assets", upload)
            self.assertEqual(saved.status, 200, saved.body)
            before = no_write_snapshot(self.backend.state)
            replay = self.request("POST", "/api/assets", upload)
            self.assertEqual(replay.status, 200, replay.body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)
            body = {"id": f"08080808-0808-0808-0808-{index:012d}", "room": self.agent, "text": "", "assets": [asset_id]}
            accepted = self.request("POST", "/api/messages", body)
            self.assertEqual(accepted.status, 200, accepted.body)
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                metadata = json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?", (body["id"],)).fetchone()[0])
            self.assertEqual(metadata["assets"], [asset_id])

    def test_missing_visibility_targets_and_invalid_receipts_are_read_only(self):
        for method, path, body in (
            ("GET", "/api/agent-chat?room=missing", None),
            ("POST", "/api/room/delete", {"id": "missing"}),
            ("GET", "/api/messages/receipts?" + urlencode({"agent": self.agent, "ids": json.dumps([""])}), None),
        ):
            before = no_write_snapshot(self.backend.state)
            result = self.request(method, path, body)
            self.assertEqual(result.status, 400, result.body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_room_hide_retains_history_and_replay_is_read_only(self):
        history = self.request("GET", "/api/agent-chat?room=p03-private")
        self.assertEqual(history.status, 200, history.body)
        self.assertEqual([message["text"] for message in history.json()["messages"]], ["Persist this history"])
        hidden = self.request("POST", "/api/room/delete", {"id": "p03-private"})
        self.assertEqual(hidden.status, 200, hidden.body)
        self.assertEqual(hidden.json()["deleted"], ["p03-private"])
        before = _fixture.parsed_coordination_snapshot(self.backend.state / "canvas.sqlite3")
        replay = self.request("POST", "/api/room/delete", {"id": "p03-private"})
        self.assertEqual(replay.status, 200, replay.body)
        self.assertEqual(
            _fixture.parsed_coordination_snapshot(self.backend.state / "canvas.sqlite3"), before,
        )
        retained = self.request("GET", "/api/agent-chat?room=p03-private")
        self.assertEqual(retained.status, 200, retained.body)
        self.assertEqual(retained.json()["messages"], history.json()["messages"])
        self.assertEqual(
            _fixture.parsed_coordination_snapshot(self.backend.state / "canvas.sqlite3"), before,
        )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "located Rust storage errors")
    def test_corrupt_replay_and_operation_report_the_stored_row(self):
        body = {"id": "09090909-0909-0909-0909-090909090909", "room": self.agent, "text": "replay"}
        self.assertEqual(self.request("POST", "/api/messages", body).status, 200)
        self.assert_rust_recipient_held()
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            from codex_sync_entities import register_functions
            register_functions(db)
            db.execute("UPDATE runtime_events SET status='corrupt' WHERE id=?", (body["id"],))
        before = _fixture.message_write_snapshot(
            self.backend.state / "canvas.sqlite3", body["id"], self.agent,
        )
        response = self.request("POST", "/api/messages", body)
        assert_stored_data_unavailable(self, response, body["id"])
        self.assertEqual(_fixture.message_write_snapshot(
            self.backend.state / "canvas.sqlite3", body["id"], self.agent,
        ), before)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            register_functions(db)
            db.execute("UPDATE runtime_events SET status='pending' WHERE id=?", (body["id"],))
            db.execute("DROP INDEX IF EXISTS runtime_workspace_operation_phase")
            db.execute("DROP INDEX IF EXISTS runtime_workspace_operation_agent_phase")
            db.execute("INSERT INTO runtime_workspace_operations VALUES('broken-operation','{broken')")
        _fixture.update_agent(self.backend, self.agent, {"workspaceOperation": "capture", "workspaceReservationId": "broken-operation"})
        try:
            operation_message_id = "10101010-1010-1010-1010-101010101010"
            before = _fixture.message_write_snapshot(
                self.backend.state / "canvas.sqlite3", operation_message_id, self.agent,
            )
            response = self.request("POST", "/api/messages", {**body, "id": operation_message_id})
            assert_stored_data_unavailable(self, response, "broken-operation")
            self.assertEqual(_fixture.message_write_snapshot(
                self.backend.state / "canvas.sqlite3", operation_message_id, self.agent,
            ), before)
        finally:
            _fixture.update_agent(self.backend, self.agent, {}, remove=("workspaceOperation", "workspaceReservationId"))
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                db.execute("DELETE FROM runtime_workspace_operations WHERE id='broken-operation'")

    def test_budget_and_workspace_admission_never_write_an_event(self):
        for fields in ({"tokenBudget": 0}, {"workspaceOperation": "restore"}):
            _fixture.update_agent(self.backend, self.agent, fields)
            try:
                before = no_write_snapshot(self.backend.state)
                response = self.request("POST", "/api/messages", {
                    "id": "05050505-0505-0505-0505-050505050505", "room": self.agent, "text": "blocked",
                })
                if "tokenBudget" in fields:
                    self.assertEqual(response.status, 400, response.body)
                    self.assertTrue(response.json()["error"].startswith("Team token budget"))
                else:
                    self.assertEqual(response.status, 400, response.body)
                    self.assertTrue(response.json()["error"].startswith(
                        "A workspace operation is active in this directory:"))
                after = no_write_snapshot(self.backend.state, baseline_snapshot=before)
                self.assertEqual(after, before)
                with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                    self.assertIsNone(db.execute(
                        "SELECT 1 FROM runtime_events WHERE id=?",
                        ("05050505-0505-0505-0505-050505050505",),
                    ).fetchone())
            finally:
                _fixture.update_agent(self.backend, self.agent, {}, remove=tuple(fields))


if __name__ == "__main__":
    unittest.main()
