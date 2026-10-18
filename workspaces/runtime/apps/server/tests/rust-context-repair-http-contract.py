#!/usr/bin/env python3
"""Context repair's no-op success and held-receipt no-write contract."""
from __future__ import annotations

import importlib.util
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
from http_backend_harness import BackendProcess, TimedHTTPContractTest, no_write_snapshot
from runtime_test_environment import make_runtime
from studio_api.models import MAX_SAFE_INTEGER

_fixture_spec = importlib.util.spec_from_file_location(
    "context_repair_runtime_fixture", Path(__file__).with_name("runtime-contract.py")
)
_fixture = importlib.util.module_from_spec(_fixture_spec)
assert _fixture_spec.loader is not None
_fixture_spec.loader.exec_module(_fixture)


class ContextRepairHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.backend = BackendProcess(Path(os.environ["TMPDIR"]))
        cls.addClassCleanup(cls.backend.close)
        cls.backend.create_codex_replay("restart-mid-turn-supervisor-replay.json")

        class QuietRuntime(_fixture.Runtime):
            def schedule(self):
                pass

        runtime = make_runtime(QuietRuntime, cls.backend, _fixture.FakeServer)
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

    def no_op_agent_snapshot(self, agent_id):
        # The mutation-sync envelope can carry unrelated concurrent workspace
        # projection changes. Keep this proof scoped to the record and projection
        # that context repair owns for the requested agent.
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            record = db.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,)
            ).fetchone()
            projection = db.execute(
                "SELECT seq,hash,payload,deleted FROM sync_entities "
                "WHERE collection='agent' AND id=?",
                (agent_id,),
            ).fetchone()
            return record, projection

    def test_noop_reads_python_written_agent(self):
        for kind in ("empty", "claude"):
            agent_id = self.ids[kind]
            before = self.no_op_agent_snapshot(agent_id)
            response = self.post({"id": agent_id})
            self.assertEqual(response.status, 200, response.body)
            body = response.json()
            sync_fields = {"_syncEntities", "_syncEntitiesAfter"}
            self.assertEqual(set(body) - sync_fields, {"id", "repair"})
            self.assertEqual(body["id"], agent_id)
            self.assertIsNone(body["repair"])
            if "_syncEntities" in body:
                self.assertIsInstance(body["_syncEntities"], list)
            if "_syncEntitiesAfter" in body:
                self.assertIn("_syncEntities", body)
                sync_after = body["_syncEntitiesAfter"]
                self.assertIs(type(sync_after), int)
                self.assertGreaterEqual(sync_after, 0)
                self.assertLessEqual(sync_after, MAX_SAFE_INTEGER)
            self.assertEqual(self.no_op_agent_snapshot(agent_id), before)

    def test_unresolved_native_receipt_is_not_replayed(self):
        response = self.post({"id": self.ids["held"]})
        self.assertEqual(response.status, 400, response.body)

    def test_uncertain_native_receipt_refuses_without_writes(self):
        # Both backends implement repair_idle; this exact uncertain input is a
        # hold, not a missing native-history-rewrite consumer.
        before = (self.snapshot(), no_write_snapshot(self.backend.state))
        response = self.post({"id": self.ids["rewrite"]})
        self.assertEqual(response.status, 400, response.body)
        self.assertEqual(
            (
                self.snapshot(),
                no_write_snapshot(
                    self.backend.state, baseline_snapshot=before[1]
                ),
            ),
            before,
        )

    def test_validation_and_missing_agent(self):
        self.assertEqual(self.post({}).status, 400)
        self.assertEqual(self.post({"id": "missing-agent"}).status, 400)


if __name__ == "__main__":
    unittest.main()
