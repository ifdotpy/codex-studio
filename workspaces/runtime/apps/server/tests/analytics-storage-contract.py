#!/usr/bin/env python3
"""Analytics file migration startup guard contract."""

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import sqlite3
import tempfile
import unittest

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_analytics import AnalyticsMixin


class AnalyticsStorageContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        root = Path(self.temp.name)
        self.canvas = root / "canvas.sqlite3"
        self.analytics = root / "analytics.sqlite3"

    def tearDown(self):
        self.temp.cleanup()

    def check_state(self):
        db = sqlite3.connect(self.canvas)
        try:
            db.execute("ATTACH DATABASE ? AS analytics", (str(self.analytics),))
            AnalyticsMixin.require_file_migration_complete(db)
        finally:
            db.close()

    def test_fresh_and_completed_states_are_accepted(self):
        self.check_state()
        with sqlite3.connect(self.analytics) as db:
            db.execute("CREATE TABLE analytics_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            db.execute("INSERT INTO analytics_meta VALUES ('fileMigrationV1','{\"phase\":\"complete\"}')")
        self.check_state()

    def test_unfinished_marker_and_legacy_canvas_tables_are_refused(self):
        with sqlite3.connect(self.analytics) as db:
            db.execute("CREATE TABLE analytics_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            db.execute("INSERT INTO analytics_meta VALUES ('fileMigrationV1','{\"phase\":\"copy\"}')")
        with self.assertRaisesRegex(RuntimeError, "fileMigrationV1.*Studio 1.0.0"):
            self.check_state()
        with sqlite3.connect(self.analytics) as db:
            db.execute("UPDATE analytics_meta SET value='{\"phase\":\"complete\"}'")
        with sqlite3.connect(self.canvas) as db:
            db.execute("CREATE TABLE analytics_usage (id TEXT PRIMARY KEY,record TEXT)")
        with self.assertRaisesRegex(RuntimeError, "fileMigrationV1.*Studio 1.0.0"):
            self.check_state()

    def test_malformed_marker_is_refused(self):
        with sqlite3.connect(self.analytics) as db:
            db.execute("CREATE TABLE analytics_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL)")
            db.execute("INSERT INTO analytics_meta VALUES ('fileMigrationV1','not-json')")
        with self.assertRaisesRegex(RuntimeError, "fileMigrationV1.*Studio 1.0.0"):
            self.check_state()


if __name__ == "__main__":
    unittest.main(verbosity=2)
