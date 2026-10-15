#!/usr/bin/env python3
"""HTTP contracts for cached costs and persisted usage controls."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import base64
import hashlib
import json
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


class UsageHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(
            cls,
            hold_provider_method="thread/resume" if not cls.rust_backend else None,
        )
        cls.backend.stop()
        from codex_runtime import Runtime

        runtime = make_runtime(Runtime, cls.backend)
        try:
            now = time.time()
            records = []
            for name in ("cancel", "retry", "disable", "enable", "malformed"):
                agent_id = str(uuid.uuid4())
                agent = runtime.create(
                    {"id": agent_id, "name": f"Usage {name}", "cwd": str(cls.backend.cwd), "prompt": ""},
                    draft=True,
                    defer=True,
                )
                if name in {"retry", "enable"}:
                    agent.update(
                        autoWake=True,
                        status="failed",
                        threadId="thread",
                        lastCompletedTurn="turn",
                        lastCompletedTurnStatus="failed",
                        nativeFailureHold=True,
                        error={"codexErrorInfo": "usageLimitExceeded"},
                    )
                retry = {
                    "id": f"retry-{name}", "status": "scheduled", "dueAt": now + 600,
                    "threadId": "thread", "turnId": "turn", "accountKey": "default",
                    "epoch": agent["epoch"], "cause": "serverOverloaded", "attempt": 1,
                    "maxAttempts": 3, "cwd": str(cls.backend.cwd), "settings": runtime.preparation_settings(agent),
                    "taskClaims": [],
                }
                resume = {
                    "id": f"resume-{name}", "status": "scheduled", "dueAt": now + 600,
                    "threadId": "thread", "turnId": "turn", "accountKey": "default",
                    "epoch": agent["epoch"], "cause": "usage_limit", "failedAt": now,
                    "authAttempt": 0, "authRefreshMarker": None, "plannedAt": None,
                    "resetAt": None, "reason": None,
                }
                if name == "enable":
                    resume = runtime.usage_resume_record(agent, "turn", agent["error"])
                    resume.update(status="cancelled", dueAt=None, reason="Automatic resume is off for this chat.")
                    agent["usageResumeEnabled"] = False
                agent["capacityRetry"] = retry
                with runtime.lock, runtime.db() as db:
                    runtime.capacity_save(db, agent, retry)
                    runtime.usage_resume_save(db, agent, resume)
                    runtime.put(db, "agents", agent, sync_rooms=False)
                records.append((agent_id, retry["id"], resume["id"]))
            cls.cancel_id, cls.cancel_retry, _ = records[0]
            cls.retry_id, cls.retry_retry, _ = records[1]
            cls.disable_id, _, cls.disable_resume = records[2]
            cls.enable_id, _, cls.enable_resume = records[3]
            cls.malformed_id, _, cls.malformed_resume = records[4]
        finally:
            runtime.close()
        home = cls.backend.codex_home.resolve()
        account_id = "usage-contract-account"
        claims = base64.urlsafe_b64encode(json.dumps({
            "https://api.openai.com/auth": {"chatgpt_account_id": account_id},
        }).encode()).rstrip(b"=").decode()
        (home / "auth.json").write_text(json.dumps({
            "tokens": {
                "account_id": account_id,
                "access_token": f"e30.{claims}.fixture-signature",
            },
        }), encoding="utf-8")
        registry = cls.backend.state / "accounts" / "registry.json"
        registry.parent.mkdir(parents=True, exist_ok=True)
        registry.write_text(json.dumps({
            "version": 1,
            "defaultAccountKey": "default",
            "accounts": {"default": {
                "id": "default", "home": str(home), "label": "Contract account",
                "source": "Local profile", "accountId": account_id, "status": "ready",
            }},
            "logins": {}, "deleteReceipts": {},
        }), encoding="utf-8")
        account_root = cls.backend.state / "account-costs" / hashlib.sha256(
            json.dumps(["default", account_id, str(home)]).encode()
        ).hexdigest()
        cache_scope = hashlib.sha256(
            json.dumps([str(home), str(account_root)]).encode()
        ).hexdigest()
        account_root.mkdir(parents=True, exist_ok=True)
        now = time.time()
        (account_root / "local-costs.json").write_text(json.dumps({
            "scope": cache_scope,
            "state": {
                "at": now, "checkedAt": now, "error": None,
                "data": {
                    "todayUSD": 2.5, "last30DaysUSD": 12.5,
                    "todayTokens": 100, "last30DaysTokens": 500,
                    "sourceDay": time.strftime("%Y-%m-%d", time.localtime(now)),
                    "coverage": "reported", "unknownModels": [], "historyDays": 30,
                },
            },
        }), encoding="utf-8")
        cls.backend.start()

    @property
    def backend(self):
        return type(self).backend

    @classmethod
    def headers(cls):
        backend = cls.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    @classmethod
    def write_headers(cls):
        return {
            **cls.headers(),
            "X-Canvas-Workspace": cls.backend.identity["workspaceId"],
        }

    def test_local_cancel_and_disable_persist_through_http(self) -> None:
        cancelled = self.backend.request(
            "POST", "/api/capacity-retry",
            body={"id": self.cancel_id, "retry_id": self.cancel_retry, "action": "cancel"},
            headers=self.write_headers(),
        )
        self.assertEqual(cancelled.status, 200, (cancelled.body[:1000], self.backend.stderr))
        self.assertEqual(cancelled.json()["status"], "cancelled")

        disabled = self.backend.request(
            "POST", "/api/usage-resume",
            body={"id": self.disable_id, "resume_id": self.disable_resume, "enabled": False},
            headers=self.write_headers(),
        )
        self.assertEqual(disabled.status, 200, disabled.body[:1000])
        self.assertEqual(disabled.json()["status"], "cancelled")
        if self.rust_backend:
            with sqlite3.connect(f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
                applied_rows = (
                    db.execute("SELECT record FROM runtime_agents WHERE id=?", (self.disable_id,)).fetchone()[0],
                    db.execute("SELECT record FROM runtime_usage_resumes WHERE id=?", (self.disable_resume,)).fetchone()[0],
                )
            replay = self.backend.request(
                "POST", "/api/usage-resume",
                body={"id": self.disable_id, "resume_id": self.disable_resume, "enabled": False},
                headers=self.write_headers(),
            )
            self.assertEqual(replay.status, 200, replay.body[:1000])
            for field in ("id", "status", "accountKey", "dueAt", "reason"):
                self.assertEqual(replay.json()[field], disabled.json()[field])
            with sqlite3.connect(f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
                replayed_rows = (
                    db.execute("SELECT record FROM runtime_agents WHERE id=?", (self.disable_id,)).fetchone()[0],
                    db.execute("SELECT record FROM runtime_usage_resumes WHERE id=?", (self.disable_resume,)).fetchone()[0],
                )
                self.assertEqual(replayed_rows, applied_rows)
            self.assertEqual(db.execute(
                "SELECT json_extract(record,'$.status') FROM runtime_capacity_retries WHERE id=?",
                (self.cancel_retry,),
            ).fetchone()[0], "cancelled")
            self.assertEqual(db.execute(
                "SELECT json_extract(record,'$.status') FROM runtime_usage_resumes WHERE id=?",
                (self.disable_resume,),
            ).fetchone()[0], "cancelled")

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust cache-reader contract")
    def test_costs_reads_the_cached_local_estimate_without_refresh(self) -> None:
        result = self.backend.request("GET", "/api/costs", headers=self.headers())
        self.assertEqual(result.status, 200, result.body[:1000])
        body = result.json()
        self.assertEqual(body["data"]["todayUSD"], 2.5)
        self.assertEqual(body["data"]["last30DaysUSD"], 12.5)
        self.assertFalse(body["refreshing"])
        self.assertFalse(body["stale"])

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust invalid stored-object contract")
    def test_malformed_resume_names_its_stored_object_and_leaves_rows_unchanged(self) -> None:
        with sqlite3.connect(self.backend.state / "canvas.sqlite3") as db:
            raw = db.execute(
                "SELECT record FROM runtime_usage_resumes WHERE id=?", (self.malformed_resume,),
            ).fetchone()[0]
            malformed = json.loads(raw)
            malformed["dueAt"] = "tomorrow"
            db.execute("UPDATE runtime_usage_resumes SET record=? WHERE id=?", (json.dumps(malformed), self.malformed_resume))
        with sqlite3.connect(f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
            before = (
                db.execute("SELECT record FROM runtime_agents WHERE id=?", (self.malformed_id,)).fetchone()[0],
                db.execute("SELECT record FROM runtime_usage_resumes WHERE id=?", (self.malformed_resume,)).fetchone()[0],
            )
        result = self.backend.request(
            "POST", "/api/usage-resume",
            body={"id": self.malformed_id, "resume_id": self.malformed_resume, "enabled": False},
            headers=self.write_headers(),
        )
        self.assertEqual(result.status, 503, result.body[:1000])
        self.assertIn(f"runtime_usage_resumes/{self.malformed_resume}", result.json()["error"])
        with sqlite3.connect(f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
            after = (
                db.execute("SELECT record FROM runtime_agents WHERE id=?", (self.malformed_id,)).fetchone()[0],
                db.execute("SELECT record FROM runtime_usage_resumes WHERE id=?", (self.malformed_resume,)).fetchone()[0],
            )
        self.assertEqual(after, before)

    def test_retry_and_enable_use_the_durable_continuation_path(self) -> None:
        if not self.rust_backend:
            self.addCleanup(self.backend.release_provider_gate)
        with sqlite3.connect(f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
            request_count = db.execute("SELECT count(*) FROM runtime_requests").fetchone()[0]
            plan = db.execute(
                "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM runtime_requests "
                "WHERE json_extract(record,'$.agent')=?1 "
                "AND json_extract(record,'$.status') IN ('pending','answering','uncertain'))",
                (self.retry_id,),
            ).fetchall()
        self.assertTrue(
            any("runtime_request_agent_status" in str(row) for row in plan),
            f"capacity admission must use the indexed projection: {plan}",
        )
        print(json.dumps({"runtimeRequestsRows": request_count, "capacityAdmissionPlan": plan}))
        retry = self.backend.request(
            "POST", "/api/capacity-retry",
            body={"id": self.retry_id, "retry_id": self.retry_retry, "action": "retry"},
            headers=self.write_headers(),
        )
        self.assertEqual(retry.status, 200, (retry.body[:1000], self.backend.stderr))
        self.assertIn(retry.json()["status"], {"starting", "scheduled"})
        if not self.rust_backend:
            self.backend.wait_until(
                self.backend.provider_gate_started,
                "capacity retry reaches the fake provider thread/resume call",
            )
        resume = self.backend.request(
            "POST", "/api/usage-resume",
            body={"id": self.enable_id, "resume_id": self.enable_resume, "enabled": True},
            headers=self.write_headers(),
        )
        self.assertEqual(resume.status, 200, resume.body[:1000])
        self.assertEqual(resume.json()["status"], "scheduled")
        with sqlite3.connect(f"file:{self.backend.state / 'canvas.sqlite3'}?mode=ro", uri=True) as db:
            capacity = json.loads(db.execute("SELECT record FROM runtime_capacity_retries WHERE id=?", (self.retry_retry,)).fetchone()[0])
            usage = json.loads(db.execute("SELECT record FROM runtime_usage_resumes WHERE id=?", (self.enable_resume,)).fetchone()[0])
            agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (self.enable_id,)).fetchone()[0])
            retry_agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (self.retry_id,)).fetchone()[0])
        self.assertEqual(capacity["status"], "starting", capacity)
        self.assertEqual(retry_agent["startAttempt"]["action"], "capacity")
        self.assertEqual(retry_agent["startAttempt"]["capacityRetryId"], self.retry_retry)
        self.assertFalse(retry_agent["startAttempt"]["submitted"])
        self.assertEqual(usage["status"], "scheduled")
        self.assertTrue(agent["usageResumeEnabled"])
        if not self.rust_backend:
            self.assertIn(
                "thread/resume",
                [frame.get("method") for frame in self.backend.received_provider_frames()],
            )
            self.backend.release_provider_gate()


if __name__ == "__main__":
    unittest.main()
