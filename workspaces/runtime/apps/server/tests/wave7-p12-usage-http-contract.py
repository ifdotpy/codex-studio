#!/usr/bin/env python3
"""P12 HTTP evidence for durable cancellation of continuation intents."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import os
import sqlite3
import sys
import time
import unittest
import uuid
from pathlib import Path

isolate_supervisor_environment()
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import make_runtime
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class ContinuationHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        from codex_runtime import Runtime

        runtime = make_runtime(Runtime, cls.backend)
        cls.agent_ids: list[str] = []
        try:
            now = time.time()
            for kind in ("capacity", "usage"):
                agent_id = str(uuid.uuid4())
                agent = runtime.create(
                    {"id": agent_id, "name": kind, "cwd": str(cls.backend.cwd), "prompt": ""},
                    draft=True,
                    defer=True,
                )
                if kind == "capacity":
                    retry = {
                        "id": f"retry-{agent_id}", "status": "scheduled", "dueAt": now + 300,
                        "threadId": "thread", "turnId": "turn", "accountKey": "default",
                        "epoch": agent["epoch"], "cause": "serverOverloaded", "attempt": 1,
                        "maxAttempts": 4, "cwd": str(cls.backend.cwd), "settings": {}, "taskClaims": [],
                    }
                    agent["capacityRetry"] = retry
                    with runtime.lock, runtime.db() as db:
                        runtime.capacity_save(db, agent, retry)
                        runtime.put(db, "agents", agent, sync_rooms=False)
                    cls.capacity_retry_id = retry["id"]
                else:
                    resume = {
                        "id": f"resume-{agent_id}", "status": "scheduled", "dueAt": now + 300,
                        "threadId": "thread", "turnId": "turn", "accountKey": "default",
                        "epoch": agent["epoch"], "cause": "usage_limit", "failedAt": now,
                        "authAttempt": 0, "authRefreshMarker": None, "plannedAt": None,
                        "resetAt": None, "reason": None,
                    }
                    with runtime.lock, runtime.db() as db:
                        runtime.usage_resume_save(db, agent, resume)
                        runtime.put(db, "agents", agent, sync_rooms=False)
                    cls.usage_resume_id = resume["id"]
                cls.agent_ids.append(agent_id)
        finally:
            runtime.close()
        cls.backend.start()

    @property
    def backend(self):
        return type(self).backend

    @classmethod
    def headers(cls):
        return {
            "Origin": cls.backend.origin or "",
            "X-Canvas-Token": cls.backend.session["token"],
            "X-Studio-API-Schema": cls.backend.schema_hash,
        }

    @classmethod
    def write_headers(cls):
        return {**cls.headers(), "X-Canvas-Workspace": cls.backend.identity["workspaceId"]}

    def test_cancelled_continuations_stay_cancelled_after_http_replay(self) -> None:
        capacity_id, usage_id = self.agent_ids
        capacity = self.backend.request(
            "POST", "/api/capacity-retry",
            body={"id": capacity_id, "retry_id": self.capacity_retry_id, "action": "cancel"},
            headers=self.write_headers(),
        )
        self.assertEqual(capacity.status, 200, capacity.body[:1000])
        usage = self.backend.request(
            "POST", "/api/usage-resume",
            body={"id": usage_id, "resume_id": self.usage_resume_id, "enabled": False},
            headers=self.write_headers(),
        )
        self.assertEqual(usage.status, 200, usage.body[:1000])
        for method, path, body in (
            ("POST", "/api/capacity-retry", {"id": capacity_id, "retry_id": self.capacity_retry_id, "action": "cancel"}),
            ("POST", "/api/usage-resume", {"id": usage_id, "resume_id": self.usage_resume_id, "enabled": False}),
        ):
            replay = self.backend.request(method, path, body=body, headers=self.write_headers())
            self.assertEqual(replay.status, 200, replay.body[:1000])
        with sqlite3.connect(f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
            self.assertEqual(db.execute(
                "SELECT json_extract(record,'$.status') FROM runtime_capacity_retries WHERE id=?",
                (self.capacity_retry_id,),
            ).fetchone()[0], "cancelled")
            self.assertEqual(db.execute(
                "SELECT json_extract(record,'$.status') FROM runtime_usage_resumes WHERE id=?",
                (self.usage_resume_id,),
            ).fetchone()[0], "cancelled")


if __name__ == "__main__":
    unittest.main()
