#!/usr/bin/env python3
"""Context repair's genuine no-op success and no-write native-rewrite refusal."""
from __future__ import annotations

import json
from contextlib import closing
import os
from pathlib import Path
import sqlite3
import sys
import unittest
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from http_backend_harness import BackendProcess, TimedHTTPContractTest
from runtime_test_environment import make_runtime


class ContextRepairHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = BackendProcess(Path(os.environ["TMPDIR"]))
        cls.addClassCleanup(cls.backend.close)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")
        from codex_runtime import Runtime
        runtime = make_runtime(Runtime, cls.backend)
        cls.ids = {}
        try:
            for name in ("empty", "claude", "held", "rewrite"):
                agent = runtime.create(dict(name=name, cwd=str(cls.backend.cwd), prompt=""), draft=True, defer=True)
                agent.update(autoWake=False, status="paused", inFlight=False)
                if name == "claude":
                    agent.update(provider="claude", threadId="synthetic-claude")
                elif name == "held":
                    agent["contextRepair"] = dict(id="repair", phase="unknown")
                elif name == "rewrite":
                    agent["threadId"] = "synthetic-thread"
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "agents", agent)
                    if name == "rewrite":
                        db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES(?,?,?,'synthetic','uncertain',1,?,NULL,NULL)", ("repair-uncertain", agent["id"], "user", agent["epoch"]))
                cls.ids[name] = agent["id"]
        finally:
            runtime.close()
        cls.backend.start()

    def headers(self):
        return {"Origin": self.backend.origin or "", "X-Canvas-Token": self.backend.session["token"], "X-Studio-API-Schema": self.backend.schema_hash}

    def post(self, body):
        return self.backend.request("POST", "/api/context-repair", body=body, headers=self.headers())

    def snapshot(self):
        # All fixture agents are paused; no dispatcher-owned running fields enter
        # this proof. The uncertain event and repair receipt must be immutable.
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (self.ids["rewrite"],)).fetchone()[0])
            event = db.execute("SELECT * FROM runtime_events WHERE id='repair-uncertain'").fetchone()
            return {key: agent.get(key) for key in ("threadId", "contextRepair", "contextRepairHistory", "contextRepairWait")}, event

    def test_noop_reads_python_written_agent(self):
        for kind in ("empty", "claude"):
            response = self.post({"id": self.ids[kind]})
            self.assertEqual(response.status, 200, response.body)
            self.assertEqual(response.json(), {"id": self.ids[kind], "repair": None})

    def test_unresolved_native_receipt_is_not_replayed(self):
        response = self.post({"id": self.ids["held"]})
        self.assertEqual(response.status, 400, response.body)

    def test_native_rewrite_refusal_writes_nothing(self):
        before = self.snapshot()
        response = self.post({"id": self.ids["rewrite"]})
        if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"):
            self.assertEqual(response.status, 501, response.body)
            self.assertEqual(response.json()["error"], "Not implemented: requires context-repair/native-history-rewrite")
        else:
            self.assertEqual(response.status, 400, response.body)
        self.assertEqual(self.snapshot(), before)

    def test_validation_and_missing_agent(self):
        self.assertEqual(self.post({}).status, 400)
        self.assertEqual(self.post({"id": "missing-agent"}).status, 400)


if __name__ == "__main__":
    unittest.main()
