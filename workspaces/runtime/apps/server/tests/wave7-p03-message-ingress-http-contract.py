#!/usr/bin/env python3
"""Message acceptance/replay and read visibility through both HTTP backends."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import unittest
from urllib.parse import urlencode

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, no_write_snapshot, start_contract_backend

_spec = importlib.util.spec_from_file_location("p03_message_fixture", Path(__file__).with_name("rust-messages-http-contract.py"))
_fixture = importlib.util.module_from_spec(_spec)
assert _spec.loader is not None
_spec.loader.exec_module(_fixture)


class MessageIngressHTTP(TimedHTTPContractTest, unittest.TestCase):
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
        response = cls.backend.request("POST", "/api/messages", body={
            "id": "06060606-0606-0606-0606-060606060606", "room": cls.agent, "text": "Hold provider preparation",
        }, headers={"Origin": cls.backend.origin or "", "X-Canvas-Token": cls.backend.session["token"],
                    "X-Studio-API-Schema": cls.backend.schema_hash})
        if response.status != 200:
            raise AssertionError(response.body)
        if not os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
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
        replay = self.request("POST", "/api/messages", {**body, "delivery": "after_turn"})
        self.assertEqual({key: value for key, value in replay.json().items() if key != "_syncEntities"},
                         {"id": body["id"], "status": "uncertain", "error": "Outcome unknown"})
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        receipts = self.request("GET", "/api/messages/receipts?" + urlencode({"agent": self.agent, "ids": json.dumps([body["id"], body["id"], "missing"])}))
        self.assertEqual(receipts.status, 200, receipts.body)
        self.assertEqual(receipts.json()["items"], [{"id": body["id"], "status": "uncertain", "error": "Outcome unknown", "materialized": False}])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        for change in ({"text": "changed"}, {"assets": ["changed"]}):
            refused = self.request("POST", "/api/messages", {**body, **change})
            self.assertEqual(refused.status, 400, refused.body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_attachment_ids_are_accepted_before_preparation_validation(self):
        for index, assets in enumerate((["unknown-upload"], [str(n) for n in range(9)])):
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
        before = no_write_snapshot(self.backend.state)
        replay = self.request("POST", "/api/room/delete", {"id": "p03-private"})
        self.assertEqual(replay.status, 200, replay.body)
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        retained = self.request("GET", "/api/agent-chat?room=p03-private")
        self.assertEqual(retained.status, 200, retained.body)
        self.assertEqual(retained.json()["messages"], history.json()["messages"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "located Rust storage errors")
    def test_corrupt_replay_and_operation_report_the_stored_row(self):
        body = {"id": "09090909-0909-0909-0909-090909090909", "room": self.agent, "text": "replay"}
        self.assertEqual(self.request("POST", "/api/messages", body).status, 200)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            from codex_sync_entities import register_functions
            register_functions(db)
            db.execute("UPDATE runtime_events SET status='corrupt' WHERE id=?", (body["id"],))
        before = no_write_snapshot(self.backend.state)
        response = self.request("POST", "/api/messages", body)
        self.assertEqual(response.status, 500, response.body)
        self.assertEqual(response.json()["error"], "Invalid stored object runtime_events/" + body["id"])
        self.assertEqual(no_write_snapshot(self.backend.state), before)
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            register_functions(db)
            db.execute("UPDATE runtime_events SET status='pending' WHERE id=?", (body["id"],))
            db.execute("DROP INDEX IF EXISTS runtime_workspace_operation_phase")
            db.execute("DROP INDEX IF EXISTS runtime_workspace_operation_agent_phase")
            db.execute("INSERT INTO runtime_workspace_operations VALUES('broken-operation','{broken')")
        _fixture.update_agent(self.backend, self.agent, {"workspaceOperation": "capture", "workspaceReservationId": "broken-operation"})
        try:
            before = no_write_snapshot(self.backend.state)
            response = self.request("POST", "/api/messages", {**body, "id": "10101010-1010-1010-1010-101010101010"})
            self.assertEqual(response.status, 500, response.body)
            self.assertEqual(response.json()["error"], "Invalid stored object runtime_workspace_operations/broken-operation")
            self.assertEqual(no_write_snapshot(self.backend.state), before)
        finally:
            _fixture.update_agent(self.backend, self.agent, {}, remove=("workspaceOperation", "workspaceReservationId"))
            with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
                db.execute("DELETE FROM runtime_workspace_operations WHERE id='broken-operation'")

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "named Rust dependencies")
    def test_unavailable_admission_never_writes_an_event(self):
        for fields, dependency in (({"tokenBudget": 0}, "budget-policy/message-admission"),
                                   ({"workspaceOperation": "restore"}, "workspace-policy/message-admission")):
            _fixture.update_agent(self.backend, self.agent, fields)
            before = no_write_snapshot(self.backend.state)
            response = self.request("POST", "/api/messages", {
                "id": "05050505-0505-0505-0505-050505050505", "room": self.agent, "text": "blocked",
            })
            self.assertEqual(response.status, 501, response.body)
            self.assertEqual(response.json()["error"], "Not implemented: requires " + dependency)
            self.assertEqual(no_write_snapshot(self.backend.state), before)
            _fixture.update_agent(self.backend, self.agent, {}, remove=tuple(fields))


if __name__ == "__main__":
    unittest.main()
