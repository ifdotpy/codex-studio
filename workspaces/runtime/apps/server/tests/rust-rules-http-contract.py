#!/usr/bin/env python3
"""Rules HTTP read contract against Python-written persisted state."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
import os
import sqlite3
import subprocess
import sys
import unittest
from pathlib import Path


isolate_supervisor_environment()

from runtime_test_environment import runtime_environment
from http_backend_harness import TimedHTTPContractTest, start_contract_backend


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
        "autoWake": True, "approvalPolicy": "never", "sandbox": "workspace-write",
    }}
    deleted = {{**live, "id": "rules-contract-deleted", "rootId": "rules-contract-deleted",
               "name": "Rules contract deleted owner"}}
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", live, sync_rooms=False)
        runtime.put(db, "agents", deleted, sync_rooms=False)
    visible = runtime.rules({{"agent": live["id"], "action": "save", "name": "visible",
                             "id": "visible", "kind": "interval", "intervalSeconds": 60}})
    hidden = runtime.rules({{"agent": deleted["id"], "action": "save", "id": "hidden",
                            "name": "hidden",
                            "kind": "interval", "intervalSeconds": 60}})
    visible.update(stallWakeGeneration=-1, lastExitCode=-9, lastStallExitCode=-11)
    deleted["deletedAt"] = 1.0
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", deleted, sync_rooms=False)
        runtime.put(db, "rules", visible, sync_rooms=False)
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


def rules_state(backend):
    with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
        return {
            "rules": database.execute(
                "SELECT id,record FROM runtime_rules ORDER BY rowid"
            ).fetchall(),
            "agents": database.execute(
                "SELECT id,record FROM runtime_agents "
                "WHERE id LIKE 'rules-contract-%' ORDER BY rowid"
            ).fetchall(),
        }


class RulesHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        seed_python_rules(cls.backend)
        cls.backend.start()
        cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)

    def _headers(self):
        backend = self.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def test_get_rules_reads_python_written_state_and_filters_deleted_agents(self) -> None:
        response = self.backend.request("GET", "/api/rules", headers=self._headers())
        self.assertEqual(response.status, 200, response.body[:1000])
        etag = response.headers.get("etag", "")
        self.assertRegex(etag, r'^"[0-9a-f]{64}"$')
        payload = response.json()
        self.assertEqual(len(payload["rules"]), 1)
        rule = payload["rules"][0]
        self.assertEqual(rule["id"], "visible")
        self.assertEqual(rule["agent"], "rules-contract-live")
        self.assertEqual(rule["stallWakeGeneration"], -1)
        self.assertEqual(rule["lastExitCode"], -9)
        self.assertEqual(rule["lastStallExitCode"], -11)

        conditional = self.backend.request(
            "GET",
            "/api/rules",
            headers={**self._headers(), "If-None-Match": f"W/{etag}"},
        )
        self.assertEqual(conditional.status, 304, conditional.body[:1000])
        self.assertEqual(conditional.body, b"")
        self.assertEqual(conditional.headers.get("etag"), etag)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_invalid_stored_rule_is_refused_without_writes(self) -> None:
        backend = self.backend
        database_path = backend.state / "canvas.sqlite3"
        with sqlite3.connect(database_path) as database:
            original = database.execute(
                "SELECT record FROM runtime_rules WHERE id='visible'"
            ).fetchone()[0]
            invalid = json.loads(original)
            invalid["stallWakeGeneration"] = "-1"
            database.execute(
                "UPDATE runtime_rules SET record=? WHERE id='visible'",
                (json.dumps(invalid),),
            )
        before = rules_state(backend)
        try:
            response = backend.request("GET", "/api/rules", headers=self._headers())
            self.assertEqual(response.status, 500, response.body[:1000])
            self.assertEqual(response.json()["error"], "Invalid stored object rules/visible")
            self.assertEqual(rules_state(backend), before)
        finally:
            with sqlite3.connect(database_path) as database:
                database.execute(
                    "UPDATE runtime_rules SET record=? WHERE id='visible'",
                    (original,),
                )


if __name__ == "__main__":
    unittest.main()
