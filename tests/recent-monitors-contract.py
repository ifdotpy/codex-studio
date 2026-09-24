#!/usr/bin/env python3
"""recent_monitors returns the same rows as the full scan, through the status index."""
import json
from pathlib import Path
import random
import sqlite3
import sys
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_workspace import MONITOR_TERMINAL_STATUSES, WorkspaceMixin

OLD = """SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('running','starting','approval') {scope}
  UNION ALL SELECT record FROM (SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') NOT IN ('running','starting','approval') {scope}
  ORDER BY json_extract(record,'$.created') DESC LIMIT 100)"""
SCOPE = """ AND json_extract(record,'$.agent') IN (
            SELECT id FROM runtime_agents WHERE json_extract(record,'$.rootId')=?
            AND json_extract(record,'$.deletedAt') IS NULL)"""


class RecentMonitors(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.db.execute("CREATE TABLE runtime_monitors (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.db.execute("CREATE INDEX runtime_monitor_status ON runtime_monitors(json_extract(record,'$.status'),json_extract(record,'$.created'))")
        self.db.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        for agent, root in [("a", "r1"), ("b", "r1"), ("c", "r2")]:
            self.db.execute("INSERT INTO runtime_agents VALUES (?,?)", (agent, json.dumps({"id": agent, "rootId": root})))
        rng = random.Random(7)
        statuses = list(MONITOR_TERMINAL_STATUSES) + ["running", "starting", "approval"]
        for i in range(3000):
            status = rng.choice(statuses if i % 50 == 0 else MONITOR_TERMINAL_STATUSES)
            self.db.execute("INSERT INTO runtime_monitors VALUES (?,?)", (f"m{i}", json.dumps(
                {"id": f"m{i}", "agent": rng.choice("abc"), "status": status, "created": rng.random() * 1e6})))

    def ids(self, rows):
        return [json.loads(row[0])["id"] for row in rows]

    def test_same_rows_as_the_full_scan_scoped_and_unscoped(self):
        mixin = WorkspaceMixin.__new__(WorkspaceMixin)
        self.assertEqual([m["id"] for m in mixin.recent_monitors(self.db)],
                         self.ids(self.db.execute(OLD.format(scope="")).fetchall()))
        for root in ("r1", "r2", "missing"):
            self.assertEqual([m["id"] for m in mixin.recent_monitors(self.db, root)],
                             self.ids(self.db.execute(OLD.format(scope=SCOPE), (root, root)).fetchall()))

    def test_terminal_rows_use_the_status_index(self):
        mixin = WorkspaceMixin.__new__(WorkspaceMixin)
        queries = []
        self.db.set_trace_callback(queries.append)
        mixin.recent_monitors(self.db)
        self.db.set_trace_callback(None)
        plan = " ".join(row[3] for row in self.db.execute("EXPLAIN QUERY PLAN " + queries[-1]))
        self.assertNotIn("SCAN runtime_monitors", plan)
        self.assertIn("USING INDEX runtime_monitor_status", plan)


class ClaudeAccountRefresh(unittest.TestCase):
    def test_claude_cli_runs_without_the_account_lock(self):
        from codex_accounts import AccountStore as Accounts
        accounts = Accounts.__new__(Accounts)
        accounts.lock = threading.RLock()
        accounts.data = {"accounts": {"claude-x": {"provider": "claude", "claudeOptions": {"configDir": "/tmp/x"},
                                                   "status": "ready"}}}
        seen = []

        def auth(options):
            # Another thread must be able to take the account lock meanwhile.
            got = []

            def probe():
                acquired = accounts.lock.acquire(timeout=1)
                got.append(acquired)
                if acquired:
                    accounts.lock.release()
            thread = threading.Thread(target=probe)
            thread.start(); thread.join()
            seen.append((options, got[0]))
            return {"status": "ready", "accountId": "claude:me", "email": "me", "plan": "max"}

        with patch("codex_claude.auth_metadata", auth):
            row = accounts.refresh("claude-x")
        self.assertEqual(seen, [({"configDir": "/tmp/x"}, True)])
        self.assertEqual(row["status"], "ready")


if __name__ == "__main__":
    unittest.main(verbosity=2)
