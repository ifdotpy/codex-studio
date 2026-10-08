#!/usr/bin/env python3
"""Retained callback connections keep resource triggers and transaction isolation."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import codex_runtime
from codex_runtime import Runtime


class RuntimeResourceTriggerReuseContract(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        runtime = self.runtime = Runtime.__new__(Runtime)
        runtime.root = Path(self.temporary.name)
        runtime.db_path = runtime.root / "canvas.sqlite3"
        runtime.analytics_db_path = runtime.root / "analytics.sqlite3"
        runtime.changed = threading.Event()
        runtime._committed_resource_changes = {}
        runtime._committed_resource_overflow = False
        runtime._committed_resource_lock = threading.Lock()
        runtime._callback_db = threading.local()
        runtime._callback_db.reuse = True
        runtime.schedule_analytics_captures = lambda *_args, **_kwargs: None
        runtime.mark_agent_records_changed = lambda *_args, **_kwargs: None
        self.statements = []
        original_connect = codex_runtime.sqlite_connect

        def connect(*args, **kwargs):
            db = original_connect(*args, **kwargs)
            db.set_trace_callback(self.statements.append)
            return db

        self.patch = patch("codex_runtime.sqlite_connect", side_effect=connect)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.addCleanup(self.close_retained_connection)
        with sqlite3.connect(runtime.db_path) as db:
            db.executescript("""
                CREATE TABLE runtime_events (
                    id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                    text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                    epoch INTEGER NOT NULL, turn_id TEXT, error TEXT);
                CREATE TABLE runtime_items (
                    id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
            """)

    def close_retained_connection(self):
        db = getattr(self.runtime._callback_db, "connection", None)
        if db is not None:
            db.close()

    def trigger_ddl(self):
        return [sql for sql in self.statements if "studio_resource_" in sql
                and sql.lstrip().upper().startswith(("CREATE TEMP TRIGGER", "DROP TRIGGER"))]

    def clear_changes(self):
        self.runtime._committed_resource_changes.clear()
        self.runtime._committed_resource_overflow = False

    def changes(self):
        return {json.loads(key)["kind"] for key in self.runtime._committed_resource_changes}

    def test_reused_connection_does_not_repeat_trigger_ddl(self):
        with self.runtime.db() as first:
            self.assertEqual(first.execute("SELECT count(*) FROM sqlite_temp_master WHERE type='trigger'").fetchone()[0], 6)
        first_ddl = self.trigger_ddl()
        self.assertEqual(sum(sql.lstrip().upper().startswith("CREATE") for sql in first_ddl), 6)
        self.assertEqual(sum(sql.lstrip().upper().startswith("DROP") for sql in first_ddl), 6)
        self.statements.clear()
        with self.runtime.db() as second:
            self.assertIs(second, first)
        self.assertEqual(self.trigger_ddl(), [])

    def test_reused_triggers_stage_each_transaction_and_discard_rollback(self):
        with self.runtime.db() as db:
            db.execute("INSERT INTO runtime_events VALUES ('event','agent','user','hello','pending',1,1,NULL,NULL)")
            db.execute("INSERT INTO runtime_items VALUES ('item','agent','{}')")
        self.assertEqual(self.changes(), {"queue", "receipts", "transcript"})
        self.clear_changes()

        with self.assertRaisesRegex(RuntimeError, "rollback"):
            with self.runtime.db() as db:
                db.execute("UPDATE runtime_events SET status='reserved' WHERE id='event'")
                db.execute("UPDATE runtime_items SET record='changed' WHERE id='item'")
                raise RuntimeError("rollback")
        self.assertEqual(self.changes(), set())

        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='event'").fetchone()[0], "pending")
            self.assertEqual(db.execute("SELECT record FROM runtime_items WHERE id='item'").fetchone()[0], "{}")
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id='event'")
            db.execute("UPDATE runtime_items SET record='changed' WHERE id='item'")
        self.assertEqual(self.changes(), {"queue", "receipts", "transcript"})
        self.clear_changes()

        with self.runtime.db() as db:
            db.execute("DELETE FROM runtime_events WHERE id='event'")
            db.execute("DELETE FROM runtime_items WHERE id='item'")
        self.assertEqual(self.changes(), {"queue", "receipts", "transcript"})

    def test_old_connection_trigger_version_is_replaced_once(self):
        with self.runtime.db() as db:
            db._studio_resource_event_trigger_version = 0
            db._studio_resource_item_trigger_version = 0
        self.statements.clear()
        with self.runtime.db():
            pass
        self.assertEqual(len(self.trigger_ddl()), 12)
        self.statements.clear()
        with self.runtime.db():
            pass
        self.assertEqual(self.trigger_ddl(), [])

    def test_table_added_after_first_entry_gets_triggers(self):
        with sqlite3.connect(self.runtime.db_path) as db:
            db.execute("DROP TABLE runtime_items")
        with self.runtime.db() as db:
            self.assertIsNone(getattr(db, "_studio_resource_item_trigger_version", None))
            db.execute("CREATE TABLE runtime_items (id TEXT PRIMARY KEY, agent TEXT, record TEXT)")
        self.statements.clear()
        with self.runtime.db() as db:
            db.execute("INSERT INTO runtime_items VALUES ('item','agent','{}')")
        self.assertEqual(self.changes(), {"transcript"})
        self.assertEqual(sum(sql.lstrip().upper().startswith("CREATE") for sql in self.trigger_ddl()), 3)


if __name__ == "__main__":
    unittest.main()
