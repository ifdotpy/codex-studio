#!/usr/bin/env python3
"""Full terminal output stays on disk while the live view remains bounded."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_terminals import TerminalManager, HISTORY_LIMIT, ARCHIVE_CHUNK


class TerminalHistory(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.manager = TerminalManager(self.temp.name)
        with self.manager.db() as db:
            db.execute('INSERT INTO user_terminals VALUES (?,?,?,?)',
                       ('terminal', json.dumps({'id': 'terminal', 'status': 'exited', 'created': 1}), '', 0))

    def tearDown(self):
        self.manager.close()
        self.temp.cleanup()

    def test_unicode_full_history_paging_after_restart_and_bounded_live_view(self):
        content = 'first\n' + 'Ж🙂abcd' * (HISTORY_LIMIT // 3) + '\nlast'
        for start in range(0, len(content), 37001):
            self.manager.append('terminal', content[start:start + 37001])
        self.assertTrue(self.manager.output('terminal')['truncated'])
        self.assertEqual(len(self.manager.output('terminal')['text']), HISTORY_LIMIT)
        self.manager.close()
        self.manager = TerminalManager(self.temp.name)
        cursor, pieces = 0, []
        while True:
            page = self.manager.history_output('terminal', cursor, 130003)
            self.assertFalse(page['truncated'])
            self.assertLessEqual(len(page['text']), 130003)
            self.assertEqual(page['offset'], cursor + len(page['text']))
            pieces.append(page['text'])
            cursor = page['offset']
            if not page['hasMore']:
                break
        self.assertEqual(''.join(pieces), content)
        self.assertEqual(cursor, len(content))
        self.assertEqual(self.manager.processes, {})

    def test_legacy_suffix_does_not_invent_discarded_prefix(self):
        with self.manager.db() as db:
            db.execute('UPDATE user_terminals SET output=?,offset=? WHERE id=?', ('saved', 91, 'terminal'))
        self.manager.append('terminal', '-new')
        value = self.manager.history_output('terminal')
        self.assertEqual(value['historyStart'], 91)
        self.assertTrue(value['truncated'])
        self.assertEqual(value['text'], 'saved-new')
        self.assertEqual(value['offset'], 100)

    def test_database_failure_cannot_commit_archive_without_live_cursor(self):
        self.manager.append('terminal', 'before')
        original = self.manager.archive_text
        def fail(*args):
            original(*args)
            raise sqlite3.OperationalError('injected database failure')
        with patch.object(self.manager, 'archive_text', side_effect=fail):
            with self.assertRaises(sqlite3.OperationalError):
                self.manager.append('terminal', 'lost-transaction')
        self.assertEqual(self.manager.history_output('terminal')['text'], 'before')
        self.assertEqual(self.manager.output('terminal')['text'], 'before')
        self.manager.append('terminal', 'after')
        self.assertEqual(self.manager.history_output('terminal')['text'], 'beforeafter')

    def test_small_deltas_use_bounded_archive_chunks(self):
        for _ in range(100):
            self.manager.append('terminal', 'xy')
        self.manager.append('terminal', 'a' * ARCHIVE_CHUNK)
        with self.manager.db() as db:
            rows = list(db.execute('SELECT start,end FROM user_terminal_output ORDER BY start'))
        self.assertEqual([tuple(row) for row in rows], [(0, ARCHIVE_CHUNK), (ARCHIVE_CHUNK, ARCHIVE_CHUNK + 200)])
        self.assertEqual(self.manager.history_output('terminal', ARCHIVE_CHUNK - 3, 9)['text'], 'a' * 9)

    def test_invalid_ranges_and_unknown_terminal_fail(self):
        for offset, limit in [(-1, 1), (0, 0), (0, HISTORY_LIMIT + 1), ('invalid', 3)]:
            with self.assertRaises(ValueError):
                self.manager.history_output('terminal', offset, limit)
        with self.assertRaisesRegex(ValueError, 'Unknown terminal'):
            self.manager.history_output('missing')
        self.assertEqual(self.manager.history_output('terminal', 999)['offset'], 0)


if __name__ == '__main__':
    unittest.main(verbosity=2)
