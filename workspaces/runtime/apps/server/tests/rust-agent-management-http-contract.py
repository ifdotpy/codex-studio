#!/usr/bin/env python3
"""Persisted lifecycle HTTP contracts; no native requests or live state."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
import sqlite3
import sys
import unittest
import uuid
from contextlib import closing
from pathlib import Path

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class AgentManagementHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = start_contract_backend(cls)

    @classmethod
    def tearDownClass(cls):
        cls.backend.stop()

    def headers(self):
        return {"Origin": self.backend.origin or "", "X-Canvas-Token": self.backend.session["token"], "X-Studio-API-Schema": self.backend.schema_hash}

    def post(self, path, body):
        return self.backend.request("POST", path, body=body, headers=self.headers())

    def draft(self):
        identity = str(uuid.uuid4())
        response = self.post("/api/leads", {"id": identity, "cwd": str(self.backend.cwd), "workspaceMode": "worktree"})
        self.assertEqual(response.status, 200, response.body)
        return identity

    def record(self, identity):
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (identity,)).fetchone()[0])

    def projection(self, identity):
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            return db.execute("SELECT deleted,payload FROM sync_entities WHERE collection='agent' AND id=?", (identity,)).fetchone()

    def test_stop_idle_draft_and_repeat_preserve_pause_and_advance_epoch(self):
        identity = self.draft()
        epoch = self.record(identity)["epoch"]
        for offset in (1, 2):
            response = self.post("/api/stop", {"id": identity})
            self.assertEqual(response.status, 200, response.body)
            self.assertEqual(response.json()["stopped"], [identity])
            record = self.record(identity)
            self.assertEqual(record["epoch"], epoch+offset)
            self.assertFalse(record["autoWake"])
            self.assertEqual(record["status"], "paused")
            self.assertEqual(record["error"], "Stopped by user")
            self.assertEqual(self.projection(identity)[0], 0)

    def test_delete_idle_draft_keeps_tombstone_and_replays_without_recreation(self):
        identity = self.draft()
        first = self.post("/api/conversation/delete", {"id": identity})
        self.assertEqual(first.status, 200, first.body)
        self.assertEqual(first.json()["deleted"], [identity])
        before = self.record(identity)
        self.assertTrue(before["deletedAt"])
        self.assertEqual(self.projection(identity)[0], 1)
        replay = self.post("/api/conversation/delete", {"id": identity})
        self.assertEqual(replay.status, 200, replay.body)
        self.assertEqual(self.record(identity)["deletedAt"], before["deletedAt"])
        self.assertFalse(self.record(identity)["autoWake"])

    def test_native_action_refusal_does_not_reserve_an_attempt(self):
        identity = self.draft()
        self.assertEqual(self.post("/api/stop", {"id": identity}).status, 200)
        before = self.record(identity)
        for action in ("review", "compact"):
            response = self.post("/api/action", {"id": identity, "action": action, "request_id": "native-"+action})
            if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
                self.assertEqual(response.status, 501, response.body)
                self.assertEqual(response.json(), {"error": "Not implemented: requires account-worker/native-action"})
            else:
                self.assertEqual(response.status, 400, response.body)
            self.assertEqual(self.record(identity), before)

    def test_missing_request_id_and_unknown_agent_have_no_writes(self):
        identity = self.draft()
        self.assertEqual(self.post("/api/stop", {"id": identity}).status, 200)
        before = self.record(identity)
        self.assertEqual(self.post("/api/action", {"id": identity, "action": "review"}).status, 400)
        self.assertEqual(self.post("/api/stop", {"id": "not-an-agent"}).status, 400)
        self.assertEqual(self.record(identity), before)


if __name__ == "__main__":
    unittest.main()
