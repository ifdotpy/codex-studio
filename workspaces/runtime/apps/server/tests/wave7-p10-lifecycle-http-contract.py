#!/usr/bin/env python3
"""P10 HTTP boundary: idle lifecycle and exact quiescent refusal snapshots."""
from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
import uuid
from contextlib import closing

from http_backend_harness import BackendProcess, TimedHTTPContractTest, no_write_snapshot

isolate_supervisor_environment()


class LifecycleHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = BackendProcess(Path(os.environ["TMPDIR"]))
        # The host may require provider homes/cache on disk while state is tmpfs.
        if os.environ.get("STUDIO_TEST_HOME_ROOT"):
            cls.homes = tempfile.TemporaryDirectory(dir=os.environ["STUDIO_TEST_HOME_ROOT"], prefix="p10-http-")
            cls.addClassCleanup(cls.homes.cleanup)
            for field in ("home", "codex_home", "claude_home", "xdg_cache"):
                path = Path(cls.homes.name) / field
                path.mkdir()
                setattr(cls.backend, field, path)
        cls.addClassCleanup(cls.backend.close)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        cls.backend.hold_provider("thread/start")
        cls.backend.start()

    def post(self, path, body):
        return self.backend.request("POST", path, body=body, headers={
            "Origin": self.backend.origin or "", "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash})

    def draft(self):
        identity = str(uuid.uuid4())
        response = self.post("/api/leads", {"id": identity, "cwd": str(self.backend.cwd), "workspaceMode": "worktree"})
        self.assertEqual(response.status, 200, response.body)
        return identity

    def record(self, identity):
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (identity,)).fetchone()[0])

    def test_stop_delete_repeated_tombstone(self):
        identity = self.draft()
        epoch = self.record(identity)["epoch"]
        stopped = self.post("/api/stop", {"id": identity, "descendants": False})
        self.assertEqual(stopped.status, 200, stopped.body)
        self.assertEqual(stopped.json()["stopped"], [identity])
        self.assertEqual(self.record(identity)["epoch"], epoch + 1)
        first = self.post("/api/conversation/delete", {"id": identity})
        self.assertEqual(first.status, 200, first.body)
        self.assertEqual(first.json()["deleted"], [identity])
        deleted_at = self.record(identity)["deletedAt"]
        again = self.post("/api/conversation/delete", {"id": identity})
        self.assertEqual(again.status, 200, again.body)
        saved = self.record(identity)
        self.assertEqual(saved["deletedAt"], deleted_at)
        self.assertFalse(saved["autoWake"])
        self.assertEqual(saved["status"], "paused")

    def test_invalid_stop_delete_have_exact_no_writes(self):
        # No draft/provider preparation is started by this test: a quiescent
        # snapshot covers all request-owned rows via the existing helper.
        before = no_write_snapshot(self.backend.state)
        for path in ("/api/stop", "/api/conversation/delete"):
            response = self.post(path, {"id": "missing-p10-agent"})
            self.assertEqual(response.status, 400, response.body)
            self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_missing_native_consumer_preserves_quiescent_state(self):
        before = no_write_snapshot(self.backend.state)
        response = self.post("/api/action", {"id": "missing-p10-agent", "action": "compact", "request_id": "p10-native"})
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(response.status, 501, response.body)
            self.assertEqual(response.json(), {"error": "Not implemented: requires account-worker/native-action"})
        else:
            self.assertEqual(response.status, 400, response.body)
        self.assertEqual(no_write_snapshot(self.backend.state), before)


if __name__ == "__main__":
    unittest.main()
