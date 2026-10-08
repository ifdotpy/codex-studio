#!/usr/bin/env python3
"""Monitor windows read their owners without decoding unrelated agent history."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
from pathlib import Path
import sqlite3
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_entity_contracts import monitor_records


class MonitorOwnerReadContract(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript("""
            CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX monitor_status ON runtime_monitors(
                json_extract(record,'$.status'),json_extract(record,'$.created'));
            CREATE INDEX agent_root ON runtime_agents(json_extract(record,'$.rootId'));
        """)

    def agent(self, identity, root="root", **extra):
        self.db.execute("INSERT INTO runtime_agents VALUES (?,?)", (
            identity, json.dumps({"id": identity, "rootId": root, **extra})))

    def monitor(self, identity, owner, status="running", created=1):
        self.db.execute("INSERT INTO runtime_monitors VALUES (?,?)", (
            identity, json.dumps({"id": identity, "agent": owner,
                                  "status": status, "created": created})))

    def test_unrelated_agent_history_does_not_consume_the_read_budget(self):
        self.db.executemany("INSERT INTO runtime_agents VALUES (?,?)", (
            (f"old-{index}", json.dumps({"id": f"old-{index}", "deletedAt": 1,
                                       "rootId": "old-root"})) for index in range(15000)))
        self.agent("live")
        self.agent("deleted", deletedAt=1)
        self.agent("other", root="other-root")
        self.monitor("live-monitor", "live")
        self.monitor("deleted-monitor", "deleted")
        self.monitor("missing-monitor", "missing")
        self.monitor("other-monitor", "other")

        steps = 0

        def limit_read():
            nonlocal steps
            steps += 100
            return steps > 10000

        self.db.set_progress_handler(limit_read, 100)
        try:
            self.assertEqual({row["id"] for row in monitor_records(self.db)},
                             {"live-monitor", "other-monitor"})
            self.assertEqual([row["id"] for row in monitor_records(self.db, "root")],
                             ["live-monitor"])
        finally:
            self.db.set_progress_handler(None, 0)

    def test_many_active_owners_use_bounded_parameter_batches(self):
        for index in range(1200):
            self.agent(f"owner-{index}")
            self.monitor(f"monitor-{index}", f"owner-{index}")
        self.db.setlimit(sqlite3.SQLITE_LIMIT_VARIABLE_NUMBER, 512)
        self.assertEqual(len(monitor_records(self.db)), 1200)
        self.assertEqual(len(monitor_records(self.db, "root")), 1200)

    def test_terminal_window_is_selected_before_deleted_owners_are_filtered(self):
        self.agent("live")
        self.agent("deleted", deletedAt=1)
        for index in range(101):
            self.monitor(f"terminal-{index}", "deleted" if index == 100 else "live",
                         "completed", index)
        rows = monitor_records(self.db)
        self.assertEqual(len(rows), 99)
        self.assertNotIn("terminal-0", {row["id"] for row in rows})
        self.assertNotIn("terminal-100", {row["id"] for row in rows})


if __name__ == "__main__":
    unittest.main()
