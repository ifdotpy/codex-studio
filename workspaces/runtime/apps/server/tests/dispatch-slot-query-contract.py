#!/usr/bin/env python3
"""Slot reads preserve exact receipts and skip completed agent history."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
from pathlib import Path
import sqlite3
import sys
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime


class DispatchSlotQuery(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.addCleanup(self.db.close)
        self.db.executescript("""
            CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX runtime_agent_global_active ON runtime_agents(
                json_extract(record,'$.status')) WHERE json_extract(record,'$.status')
                IN ('running','starting','approval');
            CREATE INDEX runtime_agent_inflight ON runtime_agents(
                json_extract(record,'$.inFlight')) WHERE json_extract(record,'$.inFlight')=1;
            CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT NOT NULL,
                status TEXT NOT NULL, created REAL NOT NULL, epoch INTEGER NOT NULL);
            CREATE INDEX runtime_event_queue ON runtime_events(status,agent,created);
        """)
        self.runtime = Runtime.__new__(Runtime)
        self.runtime.ensure_dispatch_indexes(self.db)

    def actor(self, key, **fields):
        record = {"id": key, "rootId": "root", "epoch": 8,
                  "status": "completed", "inFlight": False, "autoWake": False,
                  **fields}
        self.db.execute("INSERT INTO runtime_agents VALUES (?,?)", (key, json.dumps(record)))

    def event(self, key, agent, status="uncertain", epoch=3):
        self.db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?)",
                        (key, agent, status, 1, epoch))

    def busy(self, key, **fields):
        self.actor(key, startAttempt={"activeAtReservation": True,
                   "epoch": 3, "events": ["input:" + key]}, **fields)
        self.event("input:" + key, key)

    def slots(self):
        changes, transaction = self.db.total_changes, self.db.in_transaction
        result = self.runtime.dispatch_active_slots(self.db)
        self.assertEqual(self.db.total_changes, changes)
        self.assertEqual(self.db.in_transaction, transaction)
        self.assertEqual(len(result), len({slot["id"] for slot in result}))
        return {slot["id"]: slot["rootId"] for slot in result}

    def history(self):
        self.db.executemany("INSERT INTO runtime_agents VALUES (?,?)", (
            ("history:" + str(i), json.dumps({"id": "history:" + str(i),
                "rootId": "old-root", "status": "completed", "inFlight": False,
                "autoWake": False, "deletedAt": i + 1, "tail": "history" * 100}))
            for i in range(4000)))

    def bounded_slots(self):
        instructions = [0]
        def progress():
            instructions[0] += 100
            return instructions[0] > 2000
        self.db.set_progress_handler(progress, 100)
        try:
            try:
                result = self.slots()
            except sqlite3.OperationalError as error:
                self.fail("Slot reads exceeded 2000 SQL instructions: " + str(error))
        finally:
            self.db.set_progress_handler(None, 0)
        self.assertLessEqual(instructions[0], 2000)
        return result

    def test_active_status_and_inflight_preserve_global_and_archived_slots(self):
        for status in ("running", "starting", "approval"):
            self.actor(status, status=status)
        self.actor("inflight", inFlight=True, status="paused", deletedAt=1)
        self.actor("archived", status="running", deletedAt=1, rootId="other-root")
        self.actor("queued", status="queued", autoWake=True)
        self.event("queued-input", "queued", status="pending", epoch=8)
        for status in ("completed", "failed", "paused", "waiting", "parked"):
            self.actor("idle:" + status, status=status)
        self.assertEqual(self.slots(), {"running": "root", "starting": "root",
            "approval": "root", "inflight": "root", "archived": "other-root"})

    def test_only_exact_outstanding_input_holds_an_inactive_slot(self):
        self.busy("stopped", status="paused", deletedAt=1)
        for status in ("reserved", "dispatching", "uncertain"):
            with self.subTest(status=status):
                self.db.execute("UPDATE runtime_events SET status=? WHERE id='input:stopped'", (status,))
                self.assertEqual(self.slots(), {"stopped": "root"})
        for status in ("pending", "delivered", "failed", "cancelled"):
            with self.subTest(status=status):
                self.db.execute("UPDATE runtime_events SET status=? WHERE id='input:stopped'", (status,))
                self.assertEqual(self.slots(), {})
        for field, value in (("epoch", 8), ("agent", "other"), ("id", "other-input")):
            with self.subTest(field=field):
                self.db.execute("DELETE FROM runtime_events")
                self.event("input:stopped", "stopped")
                self.db.execute("UPDATE runtime_events SET " + field + "=?", (value,))
                self.assertEqual(self.slots(), {})

    def test_overlapping_active_and_busy_slots_count_once(self):
        self.busy("overlap", status="running", inFlight=True)
        self.busy("busy", status="queued", rootId="other-root")
        self.assertEqual(self.slots(), {"overlap": "root", "busy": "other-root"})

    def test_slot_read_remains_compatible_before_busy_index_setup(self):
        self.busy("busy")
        self.actor("active", status="starting")
        self.db.execute("DROP INDEX runtime_agent_dispatch_busy_input")
        self.assertEqual(self.slots(), {"busy": "root", "active": "root"})

    def test_no_busy_input_does_not_scan_history(self):
        self.history()
        self.actor("active", status="running")
        self.assertEqual(self.bounded_slots(), {"active": "root"})

    def test_exact_busy_input_does_not_scan_history(self):
        self.history()
        self.busy("busy")
        self.actor("active", inFlight=True)
        self.assertEqual(self.bounded_slots(), {"active": "root", "busy": "root"})


if __name__ == "__main__":
    unittest.main()
