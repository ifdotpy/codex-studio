#!/usr/bin/env python3
"""Keep the owner of a real slow writer after commit and process restart."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import gc
import importlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch
import weakref

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_sqlite
import codex_sqlite_traces as traces


class OwnerHistoryContract(unittest.TestCase):
    def setUp(self):
        importlib.reload(traces)
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.path = self.root / "canvas.sqlite3"
        with sqlite3.connect(self.path) as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("CREATE TABLE events(value TEXT)")
        db.close()
        self.journal = self.root / "diagnostics" / "sqlite-transactions.json"

    def test_real_blocked_writer_retains_origin_and_wait_stack_after_commit(self):
        ready, release = threading.Event(), threading.Event()
        failures = []
        secret = "PRIVATE_MESSAGE_AND_SQL_PARAMETER_987654321"

        def hold_writer(db):
            ready.set()
            assert release.wait(6)

        def owner():
            db = codex_sqlite.connect(self.path, site="contract.notification")
            try:
                with codex_sqlite.scope(db, "contract.notification"):
                    db.execute("INSERT INTO events VALUES (?)", (secret,))
                    hold_writer(db)
            except BaseException as error:
                failures.append(error)
            finally:
                db.close()

        thread = threading.Thread(target=owner, name="contract-notification-owner")
        thread.start()
        try:
            self.assertTrue(ready.wait(1))
            self.assertFalse(failures)
            contender = sqlite3.connect(self.path, timeout=0.02)
            try:
                with self.assertRaisesRegex(sqlite3.OperationalError, "locked"):
                    contender.execute("BEGIN IMMEDIATE")
            finally:
                contender.close()
            time.sleep(1.02)
            with patch.object(codex_sqlite.InstrumentedConnection, "execute",
                              side_effect=AssertionError("The watchdog must not execute SQL")):
                for _ in range(6):
                    traces.transaction_watchdog(self.root)
            active = json.loads(self.journal.read_text())["active"]
            self.assertEqual(len(active), 1)
            entry = active[0]
            self.assertEqual(entry["site"], "contract.notification")
            self.assertEqual(entry["database"], "canvas.sqlite3")
            self.assertEqual(entry["threadId"], thread.ident)
            self.assertIsInstance(entry["nativeThreadId"], int)
            self.assertEqual(entry["threadName"], thread.name)
            self.assertEqual(entry["firstStatement"], "INSERT")
            self.assertGreaterEqual(entry["durationMs"], 1000)
            self.assertIn("owner", {row["function"] for row in entry["originFrames"]})
            self.assertEqual(len(entry["observations"]), 4)
            self.assertIn("hold_writer", {row["function"] for row in entry["observations"][-1]["frames"]})
            self.assertNotIn(secret, self.journal.read_text())
            self.assertNotIn("INSERT INTO", self.journal.read_text())
            self.assertNotIn(str(self.root), self.journal.read_text())
            self.assertEqual(self.journal.stat().st_mode & 0o777, 0o600)
            self.assertEqual(self.journal.parent.stat().st_mode & 0o777, 0o700)
        finally:
            release.set()
            thread.join(2)
        self.assertFalse(thread.is_alive())
        self.assertFalse(failures)
        traces.transaction_watchdog(self.root)
        saved = json.loads(self.journal.read_text())
        self.assertEqual(saved["active"], [])
        self.assertEqual(saved["recent"][-1]["state"], "committed")
        self.assertEqual(saved["recent"][-1]["id"], entry["id"])
        self.assertEqual(traces.history()["longest"][0]["id"], entry["id"])
        self.assertEqual(codex_sqlite.diagnostics()["slowTransactions"]["recent"][-1]["id"], entry["id"])
        self.restart_watchdog()
        archived = self.journal.with_name("sqlite-transactions.previous.json")
        self.assertEqual(json.loads(archived.read_text())["recent"][-1]["id"], entry["id"])
        self.restart_watchdog()
        self.assertEqual(json.loads(archived.read_text())["recent"][-1]["id"], entry["id"])

    def restart_watchdog(self):
        code = "from pathlib import Path; import sys; from codex_sqlite_traces import transaction_watchdog; transaction_watchdog(Path(sys.argv[1]))"
        subprocess.run([sys.executable, "-B", "-c", code, str(self.root)], check=True,
                       env={**os.environ, "PYTHONPATH": str(ROOT / "scripts")}, timeout=5, capture_output=True)

    def test_bounded_history_rollbacks_and_no_retained_connection(self):
        now = [1000.0]
        with patch.object(codex_sqlite, "_clock", lambda: now[0]), \
                patch.object(traces, "_clock", lambda: now[0]):
            db = codex_sqlite.connect(self.path, site="contract.bounded")
            reference = weakref.ref(db)
            for index in range(80):
                db.execute("INSERT INTO events VALUES (?)", (str(index),))
                now[0] += 1.1 + index / 100
                if index % 2:
                    db.rollback()
                else:
                    db.commit()
            db.close()
            del db
            gc.collect()
            self.assertIsNone(reference(), "The recorder retains a connection")
            snapshot = traces.history()
            self.assertEqual(len(snapshot["recent"]), traces.HISTORY_LIMIT)
            self.assertEqual(len(snapshot["longest"]), traces.LONGEST_LIMIT)
            self.assertEqual({row["state"] for row in snapshot["recent"]}, {"committed", "rolledBack"})
            self.assertEqual(traces._ACTIVE, {})
            self.assertGreaterEqual(snapshot["longest"][0]["durationMs"], snapshot["longest"][-1]["durationMs"])
            traces.transaction_watchdog(self.root)
            saved = self.journal.read_bytes()
            traces.transaction_watchdog(self.root)
            self.assertEqual(self.journal.read_bytes(), saved)
        with sqlite3.connect(self.path) as db:
            self.assertEqual(db.execute("SELECT count(*) FROM events").fetchone()[0], 40)
        db.close()

    def test_io_error_retries_without_changing_transaction(self):
        db = codex_sqlite.connect(self.path, site="contract.io")
        try:
            db.execute("INSERT INTO events VALUES ('one')")
            with patch.object(traces.os, "replace", side_effect=PermissionError("PRIVATE_ERROR")):
                traces.transaction_watchdog(self.root)
            self.assertTrue(db.in_transaction)
            self.assertEqual(traces.history()["journal"], {"status": "error", "errorType": "PermissionError"})
            self.assertEqual(list(self.root.glob("diagnostics/.sqlite-transactions-*")), [])
            db.commit()
            traces.transaction_watchdog(self.root)
            self.assertEqual(traces.history()["journal"]["status"], "written")
        finally:
            db.close()

    def test_recorder_failure_does_not_change_commit_or_rollback(self):
        db = codex_sqlite.connect(self.path, site="contract.failed_recorder")
        try:
            with patch.object(traces, "begin_transaction", side_effect=RuntimeError("PRIVATE_ERROR")), \
                    patch.object(traces, "end_transaction", side_effect=RuntimeError("PRIVATE_ERROR")):
                with self.assertLogs("codex.sqlite", level="WARNING") as logs:
                    with codex_sqlite.scope(db, "contract.failed_recorder"):
                        db.execute("INSERT INTO events VALUES ('committed')")
                    db.executemany("INSERT INTO events VALUES (?)", [("rolledBack",)])
                    db.rollback()
                self.assertNotIn("PRIVATE_ERROR", " ".join(logs.output))
            self.assertFalse(db.in_transaction)
            self.assertEqual(db.execute("SELECT value FROM events").fetchall(), [("committed",)])
        finally:
            db.close()

    def test_active_registry_is_bounded_and_weak(self):
        connections = [codex_sqlite.connect(":memory:", site="contract.active_cap")
                       for _ in range(traces.ACTIVE_LIMIT + 4)]
        try:
            for db in connections:
                db.execute("BEGIN")
            self.assertEqual(len(traces._ACTIVE), traces.ACTIVE_LIMIT)
            self.assertEqual(traces.history()["activeTracking"]["untrackedStarts"], 4)
            for db in connections:
                db.rollback()
            self.assertEqual(traces._ACTIVE, {})
        finally:
            for db in connections:
                db.close()

    def test_failed_context_commit_records_rollback(self):
        now = [1000.0]
        with patch.object(codex_sqlite, "_clock", lambda: now[0]):
            db = codex_sqlite.connect(":memory:", site="contract.deferred_constraint")
            try:
                db.execute("PRAGMA foreign_keys=ON")
                db.execute("CREATE TABLE parent(id PRIMARY KEY)")
                db.execute("CREATE TABLE child(parent REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED)")
                with self.assertRaises(sqlite3.IntegrityError):
                    with codex_sqlite.scope(db, "contract.deferred_constraint"):
                        db.execute("INSERT INTO child VALUES (1)")
                        now[0] += 2
                self.assertFalse(db.in_transaction)
                self.assertEqual(traces.history()["recent"][-1]["state"], "rolledBack")
                self.assertEqual(db.execute("SELECT count(*) FROM child").fetchone()[0], 0)
            finally:
                db.close()

    def test_restart_accepts_full_bounded_frame_snapshot(self):
        frames = [{"file": "x" * 160, "function": "y" * 120, "line": 1}] * 8
        entry = {"id": "old:1", "originFrames": frames,
                 "observations": [{"frames": frames, "durationMs": 2000, "at": 1}] * 4}
        saved = {"version": 1, "pid": -1, "coverageStartedAt": 1,
                 "active": [entry] * traces.ACTIVE_LIMIT,
                 "recent": [entry] * traces.HISTORY_LIMIT,
                 "longest": [entry] * traces.LONGEST_LIMIT}
        self.journal.parent.mkdir()
        raw = json.dumps(saved).encode()
        self.assertGreater(len(raw), 2_000_000)
        self.journal.write_bytes(raw)
        traces.transaction_watchdog(self.root)
        self.assertEqual(traces.history()["journal"]["status"], "written")
        self.assertEqual(self.journal.with_name("sqlite-transactions.previous.json").read_bytes(), raw)

    def test_live_reused_runtime_connection_keeps_its_known_database_kind(self):
        db = codex_sqlite.connect(self.path, site="Runtime.db")
        del db._codex_database_kind
        try:
            with codex_sqlite.scope(db, "Runtime.db"):
                db.execute("INSERT INTO events VALUES ('old reusable connection')")
                self.assertEqual(db._codex_transaction_trace["database"], "canvas.sqlite3")
        finally:
            db.close()


if __name__ == "__main__":
    unittest.main()
