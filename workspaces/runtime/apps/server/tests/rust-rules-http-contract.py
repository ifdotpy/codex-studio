#!/usr/bin/env python3
"""Rules HTTP read contract against Python-written persisted state."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
import sqlite3
import subprocess
import sys
import time
import uuid
import unittest
from contextlib import closing
from pathlib import Path


isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from runtime_test_environment import runtime_environment
from http_backend_harness import TimedHTTPContractTest, start_contract_backend, no_write_snapshot
from studio_api.models import MAX_SAFE_INTEGER


def seed_python_rules(backend) -> None:
    """Use Runtime's real record and rules writers while the backend is stopped."""
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
sys.path.insert(0, {str(source.parent / "tests")!r})
from runtime_test_environment import make_runtime
from codex_runtime import Runtime

runtime = make_runtime(Runtime, Path(os.environ["CODEX_AGENTS_STATE_DIR"]), environment=dict(os.environ))
try:
    live = {{
        "id": "rules-contract-live", "rootId": "rules-contract-live", "epoch": 0,
        "status": "idle", "name": "Rules contract live owner", "role": "orchestrator",
        "cwd": str(Path(os.environ["CODEX_CANVAS_CWD"])), "isLead": True,
        "autoWake": True, "approvalPolicy": "never",
        "sandbox": {{"type": "workspaceWrite", "writableRoots": [str(Path(os.environ["CODEX_CANVAS_CWD"]))],
                    "networkAccess": False}},
    }}
    deleted = {{**live, "id": "rules-contract-deleted", "rootId": "rules-contract-deleted",
               "name": "Rules contract deleted owner"}}
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", live, sync_rooms=False)
        runtime.put(db, "agents", deleted, sync_rooms=False)
    visible = runtime.rules({{"agent": live["id"], "action": "save", "name": "visible",
                             "id": "visible", "kind": "interval", "intervalSeconds": 60}})
    ticked = runtime.rules({{"agent": live["id"], "action": "save", "name": "ticked",
                            "id": "ticked", "kind": "low_workers", "minimumWorkers": 1,
                            "durationMinutes": 60}})
    runtime.rules_tick()
    ticked = next(rule for rule in runtime.rules({{"agent": live["id"], "action": "list"}})["rules"]
                  if rule["id"] == "ticked")
    hidden = runtime.rules({{"agent": deleted["id"], "action": "save", "id": "hidden",
                            "name": "hidden",
                            "kind": "interval", "intervalSeconds": 60}})
    visible.update(stallWakeGeneration=-1, lastExitCode=-9, lastStallExitCode=-11)
    deleted["deletedAt"] = 1.0
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", deleted, sync_rooms=False)
        # These are persisted read fixtures, not live timers that can wake an
        # agent between the read/refusal snapshots (or invalidate its ETag).
        for rule in (visible, hidden):
            rule["status"] = "paused"
        runtime.put(db, "rules", visible, sync_rooms=False)
        runtime.put(db, "rules", ticked, sync_rooms=False)
        runtime.put(db, "rules", hidden, sync_rooms=False)
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=runtime_environment(backend),
        check=True,
        timeout=30,
    )


class RulesHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        seed_python_rules(cls.backend)
        cls.backend.start()
        def pause_ticked_rule():
            with closing(sqlite3.connect(cls.backend.state / "canvas.sqlite3")) as database, database:
                record = json.loads(database.execute(
                    "SELECT record FROM runtime_rules WHERE id='ticked'"
                ).fetchone()[0])
                # Python clears lowSince on restart. Preserve coverage of a real
                # tick, then hold the read fixture before any contract snapshot.
                if record.get("lowSince") is None:
                    return False
                record["status"] = "paused"
                database.execute(
                    "UPDATE runtime_rules SET record=? WHERE id='ticked'", (json.dumps(record),),
                )
                return True
        cls.backend.wait_until(pause_ticked_rule, "low-worker read fixture ticked and paused")
        cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)

    def _headers(self):
        backend = self.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def test_out_of_window_rule_time_is_refused_without_writes(self) -> None:
        values = (
            0, 1.0, 2**64, 2**100, 1.5, -1, 1,
            MAX_SAFE_INTEGER - 1, MAX_SAFE_INTEGER + 1,
            MAX_SAFE_INTEGER,
        )
        for index, at in enumerate(values):
            with self.subTest(at=at):
                before = no_write_snapshot(self.backend.state)
                response = self.backend.request(
                    "POST", "/api/rules",
                    body={
                        "action": "save", "agent": "rules-contract-live",
                        "id": f"must-not-save-{index}", "kind": "once", "at": at,
                    },
                    headers=self._headers(),
                )
                self.assertEqual(response.status, 400, response.body[:1000])
                self.assertEqual(no_write_snapshot(self.backend.state), before)

    def test_get_rules_reads_python_written_state_and_filters_deleted_agents(self) -> None:
        response = self.backend.request("GET", "/api/rules", headers=self._headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        etag = response.headers.get("etag", "")
        self.assertRegex(etag, r'^"[0-9a-f]{64}"$')
        payload = response.json()
        self.assertEqual(len(payload["rules"]), 2)
        rule = next(rule for rule in payload["rules"] if rule["id"] == "visible")
        self.assertEqual(rule["id"], "visible")
        self.assertEqual(rule["agent"], "rules-contract-live")
        self.assertEqual(rule["stallWakeGeneration"], -1)
        self.assertEqual(rule["lastExitCode"], -9)
        self.assertEqual(rule["lastStallExitCode"], -11)
        ticked = next(rule for rule in payload["rules"] if rule["id"] == "ticked")
        self.assertEqual(ticked["activeWorkers"], 0)
        self.assertIsNotNone(ticked["lowSince"])
        self.assertFalse(ticked["alerted"])

        conditional = self.backend.request(
            "GET",
            "/api/rules",
            headers={**self._headers(), "If-None-Match": f"W/{etag}"},
        )
        self.assertEqual(conditional.status, 304, conditional.body[:1000])
        self.assertEqual(conditional.body, b"")
        self.assertEqual(conditional.headers.get("etag"), etag)

    def test_active_check_pause_and_delete_cancel_the_check(self) -> None:
        backend = self.backend
        path = backend.state / "canvas.sqlite3"
        rule_id = "active-check-fixture"
        monitor_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"rule:{rule_id}:1"))
        create_response = backend.request(
            "POST",
            "/api/rules",
            headers=self._headers(),
            body={
                "action": "save",
                "agent": "rules-contract-live",
                "id": rule_id,
                "name": "Active check fixture",
                "kind": "interval",
                "intervalSeconds": 60,
            },
        )
        self.assertEqual(create_response.status, 200, create_response.body[:1000])
        for action in ("pause", "delete"):
            with self.subTest(action=action):
                with sqlite3.connect(path) as db:
                    rule_row = db.execute(
                        "SELECT record FROM runtime_rules WHERE id=?", (rule_id,)
                    ).fetchone()
                    rule = json.loads(rule_row[0])
                    rule.update(
                        status="active",
                        inFlight=True,
                        checks=1,
                        command="echo fixture",
                        nextAt=time.time() + 60,
                    )
                    db.execute(
                        "UPDATE runtime_rules SET record=? WHERE id=?",
                        (json.dumps(rule), rule_id),
                    )
                    monitor = {
                        "id": monitor_id,
                        "agent": "rules-contract-live",
                        "epoch": 0,
                        "command": "echo fixture",
                        "status": "approval",
                        "created": time.time(),
                        "tail": "",
                        "bytes": 0,
                        "ruleId": rule_id,
                    }
                    db.execute(
                        "INSERT OR REPLACE INTO runtime_monitors(id,record) VALUES(?,?)",
                        (monitor_id, json.dumps(monitor)),
                    )
                if action == "pause":
                    before = no_write_snapshot(backend.state)
                    update_response = backend.request(
                        "POST",
                        "/api/rules",
                        headers=self._headers(),
                        body={
                            "action": "save",
                            "agent": "rules-contract-live",
                            "id": rule_id,
                            "name": "edited during active check",
                            "kind": "interval",
                        },
                    )
                    self.assertEqual(update_response.status, 400, update_response.body[:1000])
                    self.assertEqual(no_write_snapshot(backend.state), before)
                    create_response = backend.request(
                        "POST",
                        "/api/rules",
                        headers=self._headers(),
                        body={
                            "action": "save",
                            "agent": "rules-contract-live",
                            "id": "created-with-active-check",
                            "name": "Independent check rule",
                            "kind": "once",
                        },
                    )
                    self.assertEqual(create_response.status, 200, create_response.body[:1000])
                    cleanup_response = backend.request(
                        "POST",
                        "/api/rules",
                        headers=self._headers(),
                        body={
                            "action": "delete",
                            "agent": "rules-contract-live",
                            "id": "created-with-active-check",
                        },
                    )
                    self.assertEqual(cleanup_response.status, 200, cleanup_response.body[:1000])
                response = backend.request(
                    "POST",
                    "/api/rules",
                    headers=self._headers(),
                    body={"action": action, "agent": "rules-contract-live", "id": rule_id},
                )
                self.assertEqual(response.status, 200, response.body[:1000])
                with sqlite3.connect(path) as db:
                    monitor = json.loads(db.execute(
                        "SELECT record FROM runtime_monitors WHERE id=?", (monitor_id,),
                    ).fetchone()[0])
                    self.assertEqual(monitor["status"], "cancelled")
                    row = db.execute(
                        "SELECT record FROM runtime_rules WHERE id=?", (rule_id,)
                    ).fetchone()
                if action == "pause":
                    self.assertEqual(json.loads(row[0])["status"], "paused")
                    self.assertFalse(json.loads(row[0])["inFlight"])
                else:
                    self.assertIsNone(row)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_rule_is_refused_without_writes(self) -> None:
        backend = self.backend
        database_path = backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            original = database.execute(
                "SELECT record FROM runtime_rules WHERE id='visible'"
            ).fetchone()[0]
            invalid = json.loads(original)
            invalid["lowSince"] = "invalid"
            database.execute(
                "UPDATE runtime_rules SET record=? WHERE id='visible'",
                (json.dumps(invalid),),
            )
        before = no_write_snapshot(backend.state)
        try:
            response = backend.request("GET", "/api/rules", headers=self._headers())
            self.assertEqual(response.status, 503, response.body[:1000])
            self.assertEqual(response.json()["error"], "Invalid stored object rules/visible")
            self.assertEqual(no_write_snapshot(backend.state), before)
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute(
                    "UPDATE runtime_rules SET record=? WHERE id='visible'",
                    (original,),
                )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_agent_is_refused_without_writes(self) -> None:
        backend = self.backend
        database_path = backend.state / "canvas.sqlite3"
        agent_id = "rules-contract-live"
        with sqlite3.connect(database_path) as database:
            original = database.execute(
                "SELECT record FROM runtime_agents WHERE id=?", (agent_id,)
            ).fetchone()[0]
            invalid = json.loads(original)
            invalid["sandbox"] = "workspace-write"
            database.execute(
                "UPDATE runtime_agents SET record=? WHERE id=?",
                (json.dumps(invalid), agent_id),
            )
        before = no_write_snapshot(backend.state)
        try:
            response = backend.request("GET", "/api/rules", headers=self._headers())
            self.assertEqual(response.status, 503, response.body[:1000])
            self.assertEqual(
                response.json()["error"],
                f"Invalid stored object agents/{agent_id}",
            )
            self.assertEqual(no_write_snapshot(backend.state), before)
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute(
                    "UPDATE runtime_agents SET record=? WHERE id=?",
                    (original, agent_id),
                )


if __name__ == "__main__":
    unittest.main()
