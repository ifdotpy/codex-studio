#!/usr/bin/env python3
"""Analytics samples share the runtime commit without weakening failure isolation."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_analytics import AnalyticsMixin


class AnalyticsTransactionContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        path = Path(self.temp.name) / 'analytics.sqlite3'
        self.db = sqlite3.connect(path)
        self.addCleanup(self.db.close)
        self.db.executescript('''
            PRAGMA journal_mode=WAL;
            CREATE TABLE samples (id INTEGER PRIMARY KEY);
            CREATE TABLE runtime_result (value TEXT);
            CREATE TABLE analytics_meta (key TEXT PRIMARY KEY, value TEXT);
        ''')
        self.reader = sqlite3.connect(path)
        self.addCleanup(self.reader.close)
        self.capture = AnalyticsMixin().analytics_safe

    @staticmethod
    def sample(db, number):
        db.execute('INSERT INTO samples VALUES (?)', (number,))

    def visible_samples(self):
        return self.reader.execute('SELECT count(*) FROM samples').fetchone()[0]

    def test_128_samples_commit_once_with_runtime_result(self):
        with self.db:
            for index in range(128):
                self.capture(self.db, self.sample, index)
                self.assertTrue(self.db.in_transaction)
                self.assertEqual(self.visible_samples(), 0)
            self.db.execute("INSERT INTO runtime_result VALUES ('completed')")
        self.assertEqual(self.visible_samples(), 128)
        self.assertEqual(self.reader.execute('SELECT value FROM runtime_result').fetchone()[0], 'completed')

    def test_failed_sample_rolls_back_only_its_write(self):
        def failed(db):
            self.sample(db, 2)
            raise ValueError('bad sample')
        with self.db:
            self.capture(self.db, self.sample, 1)
            self.capture(self.db, failed)
            self.capture(self.db, self.sample, 3)
            self.db.execute("INSERT INTO runtime_result VALUES ('completed')")
            self.assertEqual(self.visible_samples(), 0)
        self.assertEqual(self.reader.execute('SELECT id FROM samples ORDER BY id').fetchall(), [(1,), (3,)])
        error = json.loads(self.reader.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()[0])
        self.assertEqual(error['count'], 1)
        self.assertEqual(error['last']['error'], 'bad sample')
        self.assertEqual(self.reader.execute('SELECT value FROM runtime_result').fetchone()[0], 'completed')

    def test_outer_failure_rolls_back_analytics_and_runtime_together(self):
        with self.assertRaisesRegex(ValueError, 'native write failed'):
            with self.db:
                self.capture(self.db, self.sample, 1)
                self.db.execute("INSERT INTO runtime_result VALUES ('partial')")
                raise ValueError('native write failed')
        self.assertEqual(self.visible_samples(), 0)
        self.assertEqual(self.reader.execute('SELECT count(*) FROM runtime_result').fetchone()[0], 0)

    def test_existing_transaction_stays_open_after_capture_failure(self):
        with self.db:
            self.db.execute("INSERT INTO runtime_result VALUES ('completed')")
            self.capture(self.db, self.sample, 1)
            self.capture(self.db, self.sample, 1)
            self.assertTrue(self.db.in_transaction)
        self.assertEqual(self.visible_samples(), 1)
        self.assertEqual(self.reader.execute('SELECT value FROM runtime_result').fetchone()[0], 'completed')


if __name__ == '__main__':
    unittest.main()
