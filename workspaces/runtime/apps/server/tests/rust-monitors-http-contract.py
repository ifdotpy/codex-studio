#!/usr/bin/env python3
"""Monitor log and state-only rules HTTP contract for Python and Rust backends."""

from __future__ import annotations

import json
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
    with (logs / "large-monitor.log").open("wb") as large:
        large.truncate(64 * 1024 * 1024 + 8)
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "monitors", monitor)
        runtime.put(db, "monitors", {{
            "id": "monitor-é", "agent": agent["id"], "epoch": 0,
            "command": "fixture", "status": "completed", "created": 1.0,
            "tail": "unicode", "bytes": 7, "log": str(logs / "contract-monitor.log"),
        }})
        runtime.put(db, "monitors", {{
            "id": "large-monitor", "agent": agent["id"], "epoch": 0,
            "command": "fixture", "status": "completed", "created": 1.0,
            "tail": "", "bytes": 64 * 1024 * 1024 + 8,
            "log": str(logs / "large-monitor.log"),
        }})
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
        runtime.put(db, "rules", {{
            "id": "flight-no-monitor", "agent": agent["id"], "rootId": agent["id"],
            "epoch": 0, "name": "No monitor", "kind": "interval", "status": "active",
            "intervalSeconds": 60, "nextAt": 1.0, "at": 1.0, "command": "echo",
            "text": "check", "created": 1.0, "inFlight": True, "checks": 1, "wakes": 0,
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


def update_rule(backend, key: str, **fields) -> None:
    with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
        row = database.execute("SELECT record FROM runtime_rules WHERE id=?", (key,)).fetchone()
        record = json.loads(row[0])
        record.update(fields)
        database.execute("UPDATE runtime_rules SET record=? WHERE id=?", (json.dumps(record), key))


def stored_low_rule(database: sqlite3.Connection) -> dict[str, object]:
    """Return the stored rule without the worker count the rules tick owns."""
    record = json.loads(database.execute("SELECT record FROM runtime_rules WHERE id='low-rule'").fetchone()[0])
    record.pop("activeWorkers", None)
    return record


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

    def test_monitor_log_large_ranges_and_headers(self) -> None:
        response = self.backend.request(
            "GET", "/api/monitor/log?id=contract-monitor", headers=self.headers(),
        )
        self.assertEqual(response.status, 200, response.body[:100])
        self.assertEqual(response.headers["content-type"], "text/plain; charset=utf-8")
        self.assertEqual(response.headers["content-disposition"], 'attachment; filename="contract-monitor.log"')
        large = self.backend.request(
            "GET", "/api/monitor/log?id=large-monitor",
            headers={**self.headers(), "Range": "bytes=0-0"},
        )
        self.assertEqual(large.status, 206, large.body[:100])
        self.assertEqual(large.body, b"\x00")
        complete = self.backend.request("GET", "/api/monitor/log?id=large-monitor", headers=self.headers())
        self.assertEqual(complete.status, 200, complete.body[:100])
        self.assertEqual(len(complete.body), 64 * 1024 * 1024 + 8)
        unicode_name = self.backend.request(
            "GET", "/api/monitor/log?id=monitor-%C3%A9", headers=self.headers()
        )
        self.assertEqual(unicode_name.status, 200, unicode_name.body[:100])
        self.assertEqual(unicode_name.headers["content-disposition"], 'attachment; filename="monitor-_.log"')
        malformed = self.backend.request(
            "GET", "/api/monitor/log?id=contract-monitor",
            headers={**self.headers(), "Range": "bytes=+2-4"},
        )
        self.assertEqual(malformed.status, 416)

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust backend only")
    def test_monitor_log_refuses_invalid_file_and_deleted_agent(self) -> None:
        backend = self.backend
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            database.execute(
                "UPDATE runtime_monitors SET record=json_set(record,'$.log',?) WHERE id=?",
                (str(backend.state / "monitor-logs"), "contract-monitor"),
            )
        invalid = backend.request("GET", "/api/monitor/log?id=contract-monitor", headers=self.headers())
        self.assertEqual(invalid.status, 500)
        self.assertIn("monitors/contract-monitor", invalid.json()["error"])
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            database.execute(
                "UPDATE runtime_monitors SET record=json_set(record,'$.log',?) WHERE id=?",
                (str(backend.state / "monitor-logs" / "contract-monitor.log"), "contract-monitor"),
            )
            database.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id=?",
                ("monitor-contract-agent",),
            )
        deleted = backend.request("GET", "/api/monitor/log?id=contract-monitor", headers=self.headers())
        self.assertEqual(deleted.status, 400)
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            database.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',NULL) WHERE id=?",
                ("monitor-contract-agent",),
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

    def test_file_rule_save_and_resume_match_python_path_fingerprint(self) -> None:
        backend = self.backend
        watched = backend.cwd / "watched-file.log"
        watched.write_text("before", encoding="utf-8")
        body = {
            "action": "save", "id": "file-rule", "agent": "monitor-contract-agent",
            "name": "File watch", "kind": "file", "path": "watched-file.log",
        }
        saved = backend.request("POST", "/api/rules", headers=self.headers(), body=body)
        self.assertEqual(saved.status, 200, saved.body[:1000])
        self.assertEqual(saved.json()["path"], str(watched.resolve()))
        fingerprint = saved.json()["fingerprint"]
        self.assertIsNotNone(fingerprint)
        generation = saved.json()["fileGeneration"]
        paused = backend.request("POST", "/api/rules", headers=self.headers(), body={**body, "action": "pause"})
        self.assertEqual(paused.status, 200, paused.body[:1000])
        watched.write_text("after a change", encoding="utf-8")
        resumed = backend.request("POST", "/api/rules", headers=self.headers(), body={**body, "action": "resume"})
        self.assertEqual(resumed.status, 200, resumed.body[:1000])
        self.assertNotEqual(resumed.json()["fingerprint"], fingerprint)
        self.assertEqual(resumed.json()["fileGeneration"], generation + 1)

    def test_low_worker_idempotence_and_fresh_save_field_set(self) -> None:
        backend = self.backend
        blank_liveness = backend.request("POST", "/api/rules", headers=self.headers(), body={
            "action": "save", "id": "blank-live", "agent": "monitor-contract-agent",
            "name": "Blank liveness", "kind": "interval", "livenessCommand": "   ",
        })
        self.assertEqual(blank_liveness.status, 200, blank_liveness.body[:1000])
        self.assertEqual(blank_liveness.json()["livenessCommand"], "")
        low = {
            "action": "save", "id": "low-rule", "agent": "monitor-contract-agent",
            "name": "Low workers", "kind": "low_workers", "minimumWorkers": 8,
            "durationMinutes": 30, "text": "notify",
        }
        saved = backend.request("POST", "/api/rules", headers=self.headers(), body=low)
        self.assertEqual(saved.status, 200, saved.body[:1000])
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            update_rule(backend, "low-rule", lowSince=123.5, alerted=True)
            before = stored_low_rule(database)
        repeated = backend.request("POST", "/api/rules", headers=self.headers(), body=low)
        self.assertEqual(repeated.status, 200, repeated.body[:1000])
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            self.assertEqual(stored_low_rule(database), before)
            database.execute("UPDATE runtime_rules SET record=json_set(record,'$.restartCheck',true,'$.error','old','$.previousOutcome','old','$.lowSince',123,'$.alerted',true,'$.minimumWorkers',4) WHERE id='low-rule'")
        interval = {
            "action": "save", "id": "low-rule", "agent": "monitor-contract-agent",
            "name": "Replacement", "kind": "interval", "text": "fresh",
        }
        replaced = backend.request("POST", "/api/rules", headers=self.headers(), body=interval)
        self.assertEqual(replaced.status, 200, replaced.body[:1000])
        record = replaced.json()
        for field in ("restartCheck", "error", "previousOutcome", "lowSince", "alerted", "minimumWorkers", "durationMinutes"):
            self.assertNotIn(field, record)
        for field in ("fileActivityAt", "fileGeneration", "stallWakeGeneration"):
            self.assertIn(field, record)

    def test_active_check_dependency_only_applies_to_cancel_actions(self) -> None:
        backend = self.backend
        headers = self.headers()
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            update_rule(backend, "flight-no-monitor", inFlight=True, status="paused")
            self.assertTrue(json.loads(database.execute("SELECT record FROM runtime_rules WHERE id='flight-no-monitor'").fetchone()[0])["inFlight"])
        save = backend.request("POST", "/api/rules", headers=headers, body={
            "action": "save", "id": "flight-no-monitor", "agent": "monitor-contract-agent",
            "name": "Edited", "kind": "interval", "text": "edit",
        })
        self.assertEqual(save.status, 400)
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            update_rule(backend, "active-rule", inFlight=False)
        edited = backend.request("POST", "/api/rules", headers=headers, body={
            "action": "save", "id": "active-rule", "agent": "monitor-contract-agent",
            "name": "Edited while monitor active", "kind": "interval", "text": "edit",
        })
        self.assertEqual(edited.status, 200, edited.body[:1000])
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            update_rule(backend, "active-rule", inFlight=True)
        busy_resume = backend.request("POST", "/api/rules", headers=headers, body={
            "action": "resume", "id": "flight-no-monitor", "agent": "monitor-contract-agent",
        })
        self.assertEqual(busy_resume.status, 400)
        paused = backend.request("POST", "/api/rules", headers=headers, body={
            "action": "pause", "id": "flight-no-monitor", "agent": "monitor-contract-agent",
        })
        self.assertEqual(paused.status, 200, paused.body[:1000])
        deleted = backend.request("POST", "/api/rules", headers=headers, body={
            "action": "delete", "id": "flight-no-monitor", "agent": "monitor-contract-agent",
        })
        self.assertEqual(deleted.status, 200, deleted.body[:1000])

    def test_rule_failure_rolls_back_runtime_and_sync_entity(self) -> None:
        backend = self.backend
        with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
            database.execute("CREATE TRIGGER fail_rule_insert BEFORE INSERT ON runtime_rules BEGIN SELECT RAISE(ABORT,'injected'); END")
        response = None
        try:
            response = backend.request("POST", "/api/rules", headers=self.headers(), body={
            "action": "save", "id": "rollback-rule", "agent": "monitor-contract-agent", "name": "Rollback",
            })
            expected_status = 500 if os.environ.get("STUDIO_HTTP_BACKEND_COMMAND") else 400
            self.assertEqual(response.status, expected_status, response.body[:1000])
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                self.assertIsNone(database.execute("SELECT 1 FROM runtime_rules WHERE id='rollback-rule'").fetchone())
                self.assertIsNone(database.execute("SELECT 1 FROM sync_entities WHERE collection='rule' AND id='rollback-rule'").fetchone())
        finally:
            with sqlite3.connect(backend.state / "canvas.sqlite3") as database:
                database.execute("DROP TRIGGER IF EXISTS fail_rule_insert")

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
