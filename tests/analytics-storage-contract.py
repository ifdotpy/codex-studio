#!/usr/bin/env python3
"""Online analytics-file copy fixture. All databases are temporary."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch
from contextlib import contextmanager

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import codex_analytics_storage as storage


@contextmanager
def database(path):
    db = sqlite3.connect(path)
    try:
        with db:
            yield db
    finally:
        db.close()


class AnalyticsStorageContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.canvas = root / "canvas.sqlite3"
        self.analytics = root / "analytics.sqlite3"
        with database(self.canvas) as db:
            db.execute("CREATE TABLE analytics_items (id TEXT PRIMARY KEY,agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,type TEXT,name TEXT,is_tool INTEGER,record TEXT)")
            db.execute("CREATE TABLE analytics_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            for index in range(400):
                db.execute("INSERT INTO analytics_items VALUES (?,?,?,?,?,?,?,?,?,?)",
                           (f"item:{index}", "agent", "agent", "thread", "turn", index,
                            "reasoning", None, 0, json.dumps({"id": f"item:{index}", "payload": "x" * 1024})))
            db.execute("INSERT INTO analytics_meta VALUES ('trackingSince','1')")
        with database(self.analytics) as db:
            db.execute("CREATE TABLE analytics_items (id TEXT PRIMARY KEY,agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,type TEXT,name TEXT,is_tool INTEGER,record TEXT)")
            db.execute("CREATE TABLE analytics_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL)")

    def tearDown(self):
        self.temp.cleanup()

    def test_copy_is_resumable_preserves_target_updates_and_retires_source(self):
        advanced, status, _ = storage.copy_step(self.analytics, self.canvas)
        self.assertTrue(advanced)
        self.assertEqual(status, "copy")
        with database(self.analytics) as db:
            db.execute("INSERT INTO analytics_items VALUES (?,?,?,?,?,?,?,?,?,?)",
                       ("item:0", "agent", "agent", "thread", "turn", 999,
                        "reasoning", None, 0, json.dumps({"id": "item:0", "payload": "newer"})))
        advanced_once = False
        for _ in range(1000):
            advanced, status, _ = storage.copy_step(self.analytics, self.canvas)
            advanced_once |= advanced
            if status == "retire":
                break
        self.assertTrue(advanced_once)
        db = sqlite3.connect(f"file:{self.analytics}?mode=ro", uri=True)
        try:
            db.execute("ATTACH DATABASE ? AS canvas", (self.canvas.as_uri(),))
            storage.install_legacy_read_views(db)
            self.assertEqual(db.execute("SELECT COUNT(*) FROM analytics_items").fetchone()[0], 400)
            self.assertEqual(json.loads(db.execute("SELECT record FROM analytics_items WHERE id='item:0'").fetchone()[0])["payload"], "newer")
        finally:
            db.close()
        for _ in range(1000):
            advanced, status, _ = storage.copy_step(self.analytics, self.canvas)
            if status == "complete":
                break
        self.assertEqual(status, "complete")
        with database(self.canvas) as db:
            self.assertIsNone(db.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_items'").fetchone())
        with database(self.analytics) as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM analytics_items").fetchone()[0], 400)
            migrated = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='fileMigrationV1'").fetchone()[0])
            self.assertGreater(migrated["batches"], 1)
            self.assertTrue(migrated["writesMs"])

    def test_space_check_refuses_before_copy(self):
        class LowUsage:
            free = storage.COPY_SPACE_BYTES - 1
        with patch.object(storage.shutil, "disk_usage", return_value=LowUsage()):
            advanced, status, _ = storage.copy_step(self.analytics, self.canvas)
        self.assertFalse(advanced)
        self.assertEqual(status, "insufficientSpace")
        with database(self.analytics) as db:
            state = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='fileMigrationV1'").fetchone()[0])
            self.assertEqual(state["copied"], 0)
        with database(self.canvas) as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM analytics_items").fetchone()[0], 400)
        class RecoveredUsage:
            free = storage.COPY_SPACE_BYTES + storage.MIN_FREE_BYTES
        with patch.object(storage.shutil, "disk_usage", return_value=RecoveredUsage()):
            advanced, status, _ = storage.copy_step(self.analytics, self.canvas)
        self.assertTrue(advanced)
        self.assertEqual(status, "checkSpace")
        with patch.object(storage.shutil, "disk_usage", return_value=RecoveredUsage()):
            advanced, status, _ = storage.copy_step(self.analytics, self.canvas)
        self.assertTrue(advanced)
        self.assertEqual(status, "copy")


if __name__ == "__main__":
    unittest.main(verbosity=2)
