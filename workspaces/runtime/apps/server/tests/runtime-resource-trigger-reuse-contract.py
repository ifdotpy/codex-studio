#!/usr/bin/env python3
"""Retained callback connections keep resource triggers and transaction isolation."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import gc
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
import weakref
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
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

    def test_nested_scope_uses_a_fresh_connection(self):
        with self.runtime.db() as outer:
            with self.runtime.db() as inner:
                self.assertIsNot(inner, outer)
            with self.runtime.db() as following:
                self.assertIsNot(following, outer)
        with self.runtime.db() as following_scope:
            self.assertIs(following_scope, outer)

    def test_short_lived_thread_closes_fresh_connection_without_retaining_callback_cycle(self):
        connection_ref = []
        finished = threading.Event()
        def use_database():
            with self.runtime.db() as db:
                connection_ref.append(weakref.ref(db))
            with self.assertRaises(sqlite3.ProgrammingError):
                db.execute("SELECT 1")
            finished.set()

        worker = threading.Thread(target=use_database)
        worker.start()
        self.assertTrue(finished.wait(5))
        worker.join(5)
        self.assertFalse(worker.is_alive())
        gc.collect()
        self.assertIsNone(connection_ref[0]())

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

    def test_reused_scope_resets_api_timeout_and_discards_failed_mutated_connection(self):
        with self.runtime.db() as db:
            baseline_timeout = db.execute("PRAGMA busy_timeout").fetchone()[0]
        with self.runtime.db(busy_timeout=321) as db:
            self.assertEqual(db.execute("PRAGMA busy_timeout").fetchone()[0], 321)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("PRAGMA busy_timeout").fetchone()[0], baseline_timeout)

        connection = None
        with self.assertRaisesRegex(RuntimeError, "discard mutated scope"):
            with self.runtime.db() as db:
                connection = db
                db.execute("CREATE TEMP TABLE scope_private(value TEXT)")
                db.execute("SAVEPOINT scope_private")
                db.execute("PRAGMA busy_timeout=321")
                db.execute("PRAGMA query_only=ON")
                db.row_factory = lambda _cursor, row: row
                raise RuntimeError("discard mutated scope")
        self.assertIsNone(self.runtime._callback_db.connection)
        with self.assertRaises(sqlite3.ProgrammingError):
            connection.execute("SELECT 1")

        with self.runtime.db() as db:
            self.assertFalse(db.in_transaction)
            self.assertEqual(db.execute("PRAGMA busy_timeout").fetchone()[0], baseline_timeout)
            self.assertEqual(db.execute("PRAGMA query_only").fetchone()[0], 0)
            self.assertIs(db.row_factory, sqlite3.Row)
            self.assertIsNone(db.execute(
                "SELECT 1 FROM temp.sqlite_master WHERE name='scope_private'").fetchone())
            with self.assertRaises(sqlite3.OperationalError):
                db.execute("RELEASE scope_private")

        with self.assertRaisesRegex(RuntimeError, "scope rollback"):
            with self.runtime.db() as db:
                db.execute("INSERT INTO runtime_events VALUES ('rollback','agent','user','x','pending',1,1,NULL,NULL)")
                db.execute("SAVEPOINT scope_failed")
                raise RuntimeError("scope rollback")
        with self.runtime.db() as db:
            self.assertFalse(db.in_transaction)
            self.assertIsNone(db.execute(
                "SELECT 1 FROM runtime_events WHERE id='rollback'").fetchone())
            with self.assertRaises(sqlite3.OperationalError):
                db.execute("RELEASE scope_failed")
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
