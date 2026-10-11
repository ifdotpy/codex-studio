#!/usr/bin/env python3
"""Monitor output preserves its log, byte count and bounded tail."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import json
import stat
import tempfile
import unittest

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime
from codex_entity_contracts import MONITOR_FINAL_WINDOW
from codex_sync_entities import sync_monitor_window


class MonitorOutputContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-monitor-output-")
        self.runtime = Runtime(Path(self.temp.name), server_factory=lambda *_args: None)
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(self.runtime.close)
        agent = self.runtime.create(
            {"name": "Output owner", "cwd": self.temp.name, "prompt": ""},
            draft=True,
            defer=True,
        )
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(agent["id"], db)
            agent.update(autoWake=True, status="waiting")
            self.runtime.put(db, "agents", agent)
            self.agent_id = agent["id"]

    def add_monitor(self, key, status="running"):
        log = Path(self.temp.name) / "monitor-logs" / (key + ".log")
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "monitors", {
                "id": key,
                "agent": self.agent_id,
                "status": status,
                "created": 1,
                "log": str(log),
                "bytes": 0,
                "tail": "",
                "activityAt": 1,
                "activityGeneration": 0,
            })
        return log

    def record(self, key):
        with self.runtime.read_db() as db:
            row = db.execute("SELECT record FROM runtime_monitors WHERE id=?", (key,)).fetchone()
        return json.loads(row[0])

    def send(self, key, payload):
        self.runtime.output({
            "processId": key,
            "deltaBase64": base64.b64encode(payload).decode("ascii"),
        })

    def test_chunks_keep_complete_log_exact_bytes_bounded_tail_and_private_mode(self):
        key = "bounded-output"
        log_path = self.add_monitor(key)
        chunks = (b"first chunk", b"b" * 12005, b"last chunk")
        for chunk in chunks:
            self.send(key, chunk)
        expected_log = b"".join(chunks)
        record = self.record(key)
        self.assertEqual(log_path.read_bytes(), expected_log)
        self.assertEqual(record["bytes"], len(expected_log))
        self.assertEqual(record["tail"], expected_log.decode("utf-8")[-12000:])
        self.assertEqual(stat.S_IMODE(log_path.stat().st_mode), 0o600)
        with self.runtime.read_db() as db:
            row = db.execute(
                "SELECT payload FROM sync_entities WHERE collection='monitor' AND id=?", (key,)
            ).fetchone()
        synced = json.loads(row[0])["value"]
        self.assertEqual((synced["bytes"], synced["tail"]),
                         (len(expected_log), record["tail"][:2000]))

    def test_output_after_monitor_is_not_running_is_ignored(self):
        key = "finished-output"
        log_path = self.add_monitor(key, status="completed")
        before = self.record(key)
        self.send(key, b"late output")
        self.assertEqual(self.record(key), before)
        self.assertFalse(log_path.exists())

    def test_non_output_activation_recalculates_the_bounded_monitor_window(self):
        with self.runtime.lock, self.runtime.db() as db:
            for index in range(MONITOR_FINAL_WINDOW + 1):
                monitor = {
                    "id": f"terminal-{index}", "agent": self.agent_id,
                    "status": "completed", "created": index + 1,
                    "tail": "", "bytes": 0,
                }
                db.execute("INSERT INTO runtime_monitors(id,record) VALUES (?,?)",
                           (monitor["id"], json.dumps(monitor)))
            sync_monitor_window(db)
            monitor = {
                "id": "newly-running", "agent": self.agent_id, "status": "running",
                "created": 1000, "log": str(Path(self.temp.name) / "monitor-logs/new.log"),
                "tail": "", "bytes": 0,
            }
            self.runtime.put(db, "monitors", monitor)
            before_recalculation = db.execute(
                "SELECT id,deleted,payload FROM sync_entities WHERE collection='monitor' ORDER BY id"
            ).fetchall()
            sync_monitor_window(db)
            after_recalculation = db.execute(
                "SELECT id,deleted,payload FROM sync_entities WHERE collection='monitor' ORDER BY id"
            ).fetchall()
            self.assertEqual(before_recalculation, after_recalculation)

    def test_existing_monitor_log_is_narrowed_before_output_resumes(self):
        key = "recovered-output"
        log_path = self.add_monitor(key)
        log_path.parent.mkdir(mode=0o700)
        log_path.write_bytes(b"previous output")
        log_path.chmod(0o644)
        self.runtime._ensure_private_monitor_log(self.record(key))
        self.assertEqual(stat.S_IMODE(log_path.stat().st_mode), 0o600)


if __name__ == "__main__":
    unittest.main()
