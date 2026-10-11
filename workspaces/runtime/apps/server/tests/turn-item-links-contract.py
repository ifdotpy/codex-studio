#!/usr/bin/env python3
"""Turn-item link startup guard and complete-index contract."""
from __future__ import annotations

import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import sqlite3
import tempfile
import unittest

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_turn_item_links as links


class TurnItemLinksContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / "canvas.sqlite3"
        self.db = sqlite3.connect(self.path)
        self.db.executescript(
            "CREATE TABLE runtime_items_padding(value TEXT);"
            "CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL,created REAL NOT NULL);"
            "CREATE INDEX runtime_item_agent ON runtime_items(agent,created);"
        )

    def tearDown(self):
        self.db.close()
        self.temp.cleanup()

    def insert(self, identity, agent, turn, created):
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                        (identity, agent, json.dumps({"turnId": turn}), created))

    def status(self, identity):
        raw = self.db.execute("SELECT record FROM runtime_items WHERE id=?", (identity,)).fetchone()[0]
        return json.loads(raw).get("turnStatus")

    def test_fresh_database_installs_complete_index_and_tracks_new_rows(self):
        self.assertTrue(links.ensure_tables(self.db))
        state = self.db.execute(f"SELECT cursor,target,complete FROM {links.STATE}").fetchone()
        self.assertEqual(state, (None, None, 1))
        self.insert("one", "agent", "turn", 1)
        self.insert("two", "agent", "turn", 3)
        self.insert("other", "agent", "turn", 10)
        self.db.commit()
        self.assertEqual(self.db.execute(f"SELECT COUNT(*) FROM {links.LINKS}").fetchone()[0], 3)
        links.update_turn_status(self.db, "agent", "turn", "failed", since=2)
        self.db.commit()
        self.assertIsNone(self.status("one"))
        self.assertEqual(self.status("two"), "failed")
        self.assertEqual(self.status("other"), "failed")

    def test_completed_new_format_database_reopens_without_scanning(self):
        links.ensure_tables(self.db)
        self.insert("new", "agent", "turn", 5)
        self.db.commit()
        self.assertTrue(links.ensure_tables(self.db))
        links.update_turn_status(self.db, "agent", "turn", "completed")
        self.db.commit()
        self.assertEqual(self.status("new"), "completed")

    def test_completed_coverage_survives_runtime_items_rootpage_change(self):
        links.ensure_tables(self.db)
        old_rootpage = self.db.execute(
            "SELECT rootpage FROM sqlite_master WHERE type='table' AND name='runtime_items'").fetchone()[0]
        self.db.execute("DROP TABLE runtime_items_padding")
        self.db.commit()
        self.db.execute("VACUUM")
        new_rootpage = self.db.execute(
            "SELECT rootpage FROM sqlite_master WHERE type='table' AND name='runtime_items'").fetchone()[0]
        self.assertNotEqual(old_rootpage, new_rootpage)
        self.assertTrue(links.ensure_tables(self.db))
        self.insert("after-vacuum", "agent", "turn", 7)
        self.db.commit()
        self.assertEqual(self.db.execute(
            f"SELECT COUNT(*) FROM {links.LINKS} WHERE id='after-vacuum'").fetchone()[0], 1)

    def test_old_rows_without_coverage_are_refused(self):
        self.insert("legacy", "agent", "turn", 5)
        self.db.commit()
        with self.assertRaisesRegex(RuntimeError, "legacy runtime_items.*Studio 1.0.0"):
            links.ensure_tables(self.db)
        self.assertIsNone(self.db.execute(
            "SELECT 1 FROM sqlite_master WHERE name=?", (links.LINKS,)).fetchone())

    def test_unfinished_coverage_marker_is_refused(self):
        links.ensure_tables(self.db)
        self.db.execute(f"UPDATE {links.STATE} SET complete=0")
        self.db.commit()
        with self.assertRaisesRegex(RuntimeError, "turn-item-links v1.*Studio 1.0.0"):
            links.ensure_tables(self.db)


if __name__ == "__main__":
    unittest.main(verbosity=2)
