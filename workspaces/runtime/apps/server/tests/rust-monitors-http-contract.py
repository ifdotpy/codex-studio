#!/usr/bin/env python3
"""Monitor log and state-only rules HTTP contract for Python and Rust backends."""

from __future__ import annotations

import os
import sqlite3
import subprocess
import sys
import unittest
from pathlib import Path

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


def seed_state(backend) -> None:
    source = Path(__file__).resolve().parents[1] / "src"
    seed = f"""
import os, sys
from pathlib import Path
sys.path.insert(0, {str(source)!r})
from codex_runtime import Runtime

root = Path(os.environ["CODEX_AGENTS_STATE_DIR"])
runtime = Runtime(root)
try:
    agent = {{
        "id": "monitor-contract-agent", "rootId": "monitor-contract-agent", "epoch": 0,
        "status": "idle", "name": "Monitor contract agent", "role": "orchestrator",
        "cwd": str(Path(os.environ["CODEX_CANVAS_CWD"])), "isLead": True,
        "autoWake": True, "approvalPolicy": "never", "sandbox": "workspace-write",
    }}
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent)
    logs = root / "monitor-logs"
    logs.mkdir(parents=True, exist_ok=True)
    monitor = {{
        "id": "contract-monitor", "agent": agent["id"], "epoch": 0,
        "command": "fixture", "status": "completed", "created": 1.0,
        "tail": "retained tail", "bytes": 10,
        "log": str(logs / "contract-monitor.log"),
    }}
    (logs / "contract-monitor.log").write_bytes(b"abcdefghij")
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "monitors", monitor)
        runtime.put(db, "rules", {{
            "id": "active-rule", "agent": agent["id"], "rootId": agent["id"],
            "epoch": 0, "name": "Active", "kind": "interval", "status": "active",
            "intervalSeconds": 60, "nextAt": 1.0, "at": 1.0, "command": "echo",
            "text": "check", "created": 1.0, "inFlight": True, "checks": 1, "wakes": 0,
        }})
        runtime.put(db, "monitors", {{
            "id": "active-check", "agent": agent["id"], "epoch": 0,
            "command": "echo", "status": "running", "created": 1.0,
            "tail": "", "bytes": 0, "ruleId": "active-rule",
            "log": str(logs / "active-check.log"),
        }})
finally:
    runtime.close()
"""
    python = os.environ.get("STUDIO_HTTP_BACKEND_PYTHON", sys.executable)
    subprocess.run(
        [python, "-B", "-c", seed],
        cwd=backend.cwd,
        env=backend._environment(),
        check=True,
        timeout=30,
    )


def persisted_state(backend):
    with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
        return (
            database.execute("SELECT id,record FROM runtime_rules ORDER BY rowid").fetchall(),
            database.execute("SELECT id,record FROM runtime_monitors ORDER BY rowid").fetchall(),
        )


class MonitorsHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.backend = start_contract_backend(cls)
        cls.backend.stop()
        seed_state(cls.backend)
        cls.backend.start()
        cls.backend.ready_samples.append(cls.backend.ready_seconds or 0)

    def headers(self):
        backend = self.backend
        return {
            "Origin": backend.origin or "",
            "X-Canvas-Token": backend.session["token"],
            "X-Studio-API-Schema": backend.schema_hash,
        }

    def test_monitor_log_reads_python_written_file_and_honors_ranges(self) -> None:
        response = self.backend.request(
            "GET", "/api/monitor/log?id=contract-monitor", headers=self.headers()
        )
        self.assertEqual(response.status, 200, response.body[:1000])
        self.assertEqual(response.body, b"abcdefghij")
        self.assertEqual(response.headers["x-log-truncated"], "false")
        self.assertEqual(response.headers["accept-ranges"], "bytes")

        ranged = self.backend.request(
            "GET",
            "/api/monitor/log?id=contract-monitor",
            headers={**self.headers(), "Range": "bytes=2-4"},
        )
        self.assertEqual(ranged.status, 206, ranged.body[:1000])
        self.assertEqual(ranged.body, b"cde")
        self.assertEqual(ranged.headers["content-range"], "bytes 2-4/10")

        invalid = self.backend.request(
            "GET",
            "/api/monitor/log?id=contract-monitor",
            headers={**self.headers(), "Range": "bytes=10-"},
        )
        self.assertEqual(invalid.status, 416)
        self.assertEqual(invalid.headers["content-range"], "bytes */10")

    def test_monitor_log_falls_back_to_stored_tail_when_file_is_missing(self) -> None:
        backend = self.backend
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            database.execute(
                "UPDATE runtime_monitors SET record=json_set(record,'$.log',?) WHERE id=?",
                (str(backend.state / "monitor-logs" / "fallback.log"), "contract-monitor"),
            )
        try:
            response = backend.request(
                "GET", "/api/monitor/log?id=contract-monitor", headers=self.headers()
            )
            self.assertEqual(response.status, 200, response.body[:1000])
            self.assertEqual(response.body, b"retained tail")
            self.assertEqual(response.headers["x-log-truncated"], "false")
        finally:
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute(
                    "UPDATE runtime_monitors SET record=json_set(record,'$.log',?) WHERE id=?",
                    (str(backend.state / "monitor-logs" / "contract-monitor.log"), "contract-monitor"),
                )

    def test_rule_save_pause_resume_delete_are_persisted(self) -> None:
        backend = self.backend
        headers = self.headers()
        body = {
            "action": "save",
            "id": "state-only-rule",
            "agent": "monitor-contract-agent",
            "name": "State only",
            "kind": "interval",
            "intervalSeconds": 60,
            "command": "",
        }
        saved = backend.request("POST", "/api/rules", headers=headers, body=body)
        self.assertEqual(saved.status, 200, saved.body[:1000])
        self.assertEqual(saved.json()["id"], "state-only-rule")
        self.assertEqual(saved.json()["status"], "active")

        paused = backend.request(
            "POST", "/api/rules", headers=headers,
            body={**body, "action": "pause"},
        )
        self.assertEqual(paused.status, 200, paused.body[:1000])
        self.assertEqual(paused.json()["status"], "paused")

        resumed = backend.request(
            "POST", "/api/rules", headers=headers,
            body={**body, "action": "resume"},
        )
        self.assertEqual(resumed.status, 200, resumed.body[:1000])
        self.assertEqual(resumed.json()["status"], "active")

        deleted = backend.request(
            "POST", "/api/rules", headers=headers,
            body={**body, "action": "delete"},
        )
        self.assertEqual(deleted.status, 200, deleted.body[:1000])
        self.assertEqual(deleted.json()["deleted"], "state-only-rule")
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            self.assertIsNone(
                database.execute(
                    "SELECT 1 FROM runtime_rules WHERE id='state-only-rule'"
                ).fetchone()
            )

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_active_rule_check_refusal_is_named_and_writes_nothing(self) -> None:
        backend = self.backend
        before = persisted_state(backend)
        response = backend.request(
            "POST",
            "/api/rules",
            headers=self.headers(),
            body={
                "action": "pause",
                "id": "active-rule",
                "agent": "monitor-contract-agent",
            },
        )
        self.assertEqual(response.status, 501, response.body[:1000])
        self.assertEqual(
            response.json()["error"],
            "Not implemented: requires monitor-driver/active-checks",
        )
        self.assertEqual(persisted_state(backend), before)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_monitor_input_and_cancel_remain_unregistered(self) -> None:
        backend = self.backend
        input_response = backend.request(
            "POST",
            "/api/monitor/input",
            headers=self.headers(),
            body={"id": "contract-monitor", "text": ""},
        )
        cancel_response = backend.request(
            "POST",
            "/api/monitor/cancel",
            headers=self.headers(),
            body={"id": "contract-monitor"},
        )
        for response in (input_response, cancel_response):
            self.assertEqual(response.status, 501, response.body[:1000])
            self.assertEqual(response.json()["error"], "Not implemented")


if __name__ == "__main__":
    unittest.main()
