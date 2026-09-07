"""Exact FTS row updates, migration rollback, and operation-count regression."""
import sqlite3
from contextlib import contextmanager
import threading
from types import MethodType, SimpleNamespace
import sys
import unittest
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_work import WorkMixin
from codex_search_index_update import apply, _LEGACY

class SearchIndex(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.db.executescript("""
        CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body);
        CREATE TABLE runtime_search_indexed (id TEXT PRIMARY KEY);
        """)
        self.work = WorkMixin()

    def tearDown(self):
        self.db.close()

    def seed(self, count):
        self.db.executemany('INSERT INTO runtime_search VALUES (?,?,?,?)',
            ((str(n), 'agent', 'assistant', 'original evidence') for n in range(count)))
        self.db.commit()

    def test_migration_updates_restart_and_rollback(self):
        self.seed(30)
        self.work.setup_search_rows(self.db)
        self.work.setup_search_rows(self.db)
        with self.db:
            self.work.index_item(self.db, '4', 'agent', 'assistant', 'changed evidence')
            self.work.index_item(self.db, 'new', 'other', 'assistant', 'new evidence')
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search').fetchone()[0], 31)
        self.assertEqual(self.db.execute("SELECT id FROM runtime_search WHERE runtime_search MATCH 'changed'").fetchall(), [('4',)])
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search WHERE runtime_search MATCH 'original'").fetchone()[0], 29)
        self.db.execute('BEGIN')
        self.work.index_item(self.db, '4', 'agent', 'assistant', 'rolledback')
        self.db.rollback()
        self.work.setup_search_rows(self.db)
        with self.db:
            self.work.index_item(self.db, '4', 'agent', 'assistant', 'durable')
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search WHERE runtime_search MATCH 'rolledback'").fetchone()[0], 0)
        self.assertEqual(self.db.execute("SELECT id FROM runtime_search WHERE runtime_search MATCH 'durable'").fetchall(), [('4',)])
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search s JOIN runtime_search_rows r ON s.rowid=r.search_rowid AND s.id=r.id').fetchone()[0], 31)

    def test_failed_migration_leaves_no_partial_table(self):
        self.seed(1)
        self.db.execute("INSERT INTO runtime_search VALUES ('0','agent','assistant','duplicate')")
        self.db.commit()
        with self.assertRaises(sqlite3.IntegrityError):
            self.work.setup_search_rows(self.db)
        self.assertIsNone(self.db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_search_rows'").fetchone())
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search').fetchone()[0], 2)

    def test_live_cutover_requires_known_code_and_preserves_rows(self):
        self.seed(15)
        @contextmanager
        def db():
            with self.db:
                yield self.db
        runtime = SimpleNamespace(lock=threading.RLock(), db=db)
        runtime.index_item = MethodType(lambda *args: None, runtime)
        with self.assertRaisesRegex(RuntimeError, "Unknown live search"):
            apply(runtime)
        self.assertIsNone(self.db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_search_rows'").fetchone())
        scope = {}
        exec(_LEGACY, scope)
        runtime.index_item = MethodType(scope['index_item'], runtime)
        self.assertEqual(apply(runtime), {'status': 'applied', 'mappedRows': 15})
        self.assertEqual(apply(runtime), {'status': 'already_applied'})
        with db() as connection:
            runtime.index_item(connection, '3', 'agent', 'assistant', 'aftercutover')
        self.assertEqual(self.db.execute("SELECT id FROM runtime_search WHERE runtime_search MATCH 'aftercutover'").fetchall(), [('3',)])
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search').fetchone()[0], 15)

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
        self.work.setup_search_rows(self.db)
        old = self.steps(lambda: self.db.execute('SELECT rowid FROM runtime_search WHERE id=?', ('2500',)).fetchall())
        new = self.steps(lambda: self.work.index_item(self.db, '2500', 'agent', 'assistant', 'updated evidence'))
        self.assertLess(new * 10, old, (new, old))
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_search WHERE id=?', ('2500',)).fetchone()[0], 1)
        print({'legacyScanSteps': old, 'indexedUpdateSteps': new})

if __name__ == '__main__':
    unittest.main()
