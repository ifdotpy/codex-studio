"""Exact FTS row updates, migration rollback, and operation-count regression."""
import sqlite3
import sys
import unittest
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from transcript_storage.storage import backfill_addresses, index_item, initialize

class SearchIndex(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.db.executescript("""
        CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body);
        """)
        initialize(self.db)

    def tearDown(self):
        self.db.close()

    def seed(self, count):
        self.db.executemany('INSERT INTO runtime_search VALUES (?,?,?,?)',
            ((str(n), 'agent', 'assistant', 'original evidence') for n in range(count)))
        self.db.commit()

    def test_bounded_address_migration_updates_restart_and_rollback(self):
        self.seed(30)
        self.assertEqual(backfill_addresses(self.db, limit=10), 10)
        self.assertEqual(backfill_addresses(self.db, limit=10), 10)
        self.assertEqual(backfill_addresses(self.db, limit=10), 10)
        with self.db:
            index_item(self.db, '4', 'agent', 'assistant', 'changed evidence')
            index_item(self.db, 'new', 'other', 'assistant', 'new evidence')
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search').fetchone()[0], 31)
        self.assertEqual(self.db.execute("SELECT id FROM runtime_search WHERE runtime_search MATCH 'changed'").fetchall(), [('4',)])
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search WHERE runtime_search MATCH 'original'").fetchone()[0], 29)
        self.db.execute('BEGIN')
        index_item(self.db, '4', 'agent', 'assistant', 'rolledback')
        self.db.rollback()
        with self.db:
            index_item(self.db, '4', 'agent', 'assistant', 'durable')
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search WHERE runtime_search MATCH 'rolledback'").fetchone()[0], 0)
        self.assertEqual(self.db.execute("SELECT id FROM runtime_search WHERE runtime_search MATCH 'durable'").fetchall(), [('4',)])
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search s JOIN runtime_search_rows r ON s.rowid=r.search_rowid AND s.id=r.id').fetchone()[0], 31)

    def test_duplicate_legacy_ids_are_cleaned_by_next_update(self):
        self.seed(1)
        self.db.execute("INSERT INTO runtime_search VALUES ('0','agent','assistant','duplicate')")
        self.db.execute("INSERT INTO runtime_search_indexed VALUES ('0')")
        self.db.commit()
        index_item(self.db, '0', 'agent', 'assistant', 'one current body')
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search WHERE id=?', ('0',)).fetchone()[0], 1)


    def steps(self, callback):
        count = [0]
        def tick():
            count[0] += 1
            return 0
        self.db.set_progress_handler(tick, 1)
        try:
            callback()
        finally:
            self.db.set_progress_handler(None, 0)
        return count[0]

    def test_updates_do_not_scan_history(self):
        self.seed(5000)
        while backfill_addresses(self.db, limit=128):
            pass
        old = self.steps(lambda: self.db.execute('SELECT rowid FROM runtime_search WHERE id=?', ('2500',)).fetchall())
        new = self.steps(lambda: index_item(self.db, '2500', 'agent', 'assistant', 'updated evidence'))
        self.assertLess(new * 10, old, (new, old))
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search WHERE id=?', ('2500',)).fetchone()[0], 1)
        print({'legacyScanSteps': old, 'indexedUpdateSteps': new})

if __name__ == '__main__':
    unittest.main()
