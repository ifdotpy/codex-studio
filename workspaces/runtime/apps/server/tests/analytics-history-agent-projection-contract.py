#!/usr/bin/env python3
"""History roster projection retains JSON values, row order, and exact receipts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'history_projection_fixture', Path(__file__).with_name('analytics-history-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_analytics_history as history


class AgentProjection(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.runtime = fixture.Fixture(self.temp.name)

    def rows(self, records):
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_agents')
            db.executemany('INSERT INTO runtime_agents VALUES (?,?)',
                           [(key, json.dumps(record)) for key, record in records])

    def refresh(self):
        self.runtime._analytics_history_cursor = 0
        with patch.object(self.runtime, 'agent', side_effect=ValueError('Private absent actor')):
            self.assertFalse(self.runtime.analytics_history_step())
        return self.runtime._analytics_history_ids

    @unittest.skipUnless(sqlite3.sqlite_version_info >= (3, 38, 0), 'SQLite JSON projection needs 3.38')
    def test_refresh_does_not_decode_full_actors(self):
        self.rows([('database-key', {'id': 'record-key', 'threadId': 'thread',
                                    'prompt': 'large-secret-' + 'x' * 262144})])
        decoded = []
        original = history.json.loads

        def loads(raw, *args, **options):
            decoded.append(raw)
            return original(raw, *args, **options)

        with patch.object(self.runtime, 'records', side_effect=AssertionError('Full actor decode')), \
                patch.object(history.json, 'loads', side_effect=loads):
            self.assertEqual(self.refresh(), ['record-key'])
        self.assertTrue(decoded)
        self.assertFalse(any('large-secret-' in raw for raw in decoded if isinstance(raw, str)))

    def test_json_truthiness_preserves_original_row_order(self):
        values = [None, False, True, 0, 1, -1, 0.0, 1.5, '', '0', 'false', [], [0], {}, {'value': 0}]
        records = [('last-sql-key', {'id': 'missing-thread'})]
        for index, value in enumerate(values):
            records.append((str(99 - index), {'id': 'record-' + str(index), 'threadId': value}))
        self.rows(records)
        expected = [record['id'] for _, record in records if record.get('threadId')]
        self.assertEqual(self.refresh(), expected)

    def test_record_id_json_types_and_sql_key_mismatch_are_preserved(self):
        identities = [True, False, None, 0, 1, 2 ** 80, 1.25, 'record-key', [], {'nested': [1]}]
        self.rows([(str(index), {'id': identity, 'threadId': 'thread'})
                   for index, identity in enumerate(identities)])
        actual = self.refresh()
        self.assertEqual(actual, identities)
        self.assertEqual([type(value) for value in actual], [type(value) for value in identities])

    def test_missing_id_errors_only_for_a_truthy_thread(self):
        self.rows([('empty', {'threadId': ''}), ('null', {'threadId': None}),
                   ('missing', {}), ('valid', {'id': 'record-id', 'threadId': 'thread'})])
        self.assertEqual(self.refresh(), ['record-id'])
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_agents VALUES (?,?)',
                       ('bad', json.dumps({'threadId': True})))
        with self.assertRaisesRegex(KeyError, 'id'):
            self.refresh()
        self.assertEqual(self.runtime._analytics_history_ids, ['record-id'])

    def test_old_sqlite_retains_the_records_path(self):
        self.rows([('sql-key', {'id': 'record-key', 'threadId': 'thread'})])
        original = self.runtime.records
        with patch.object(sqlite3, 'sqlite_version_info', (3, 37, 0)), \
                patch.object(self.runtime, 'records', wraps=original) as records:
            self.assertEqual(self.refresh(), ['record-key'])
        records.assert_called_once()

    def test_each_step_loads_the_fresh_actor_scope(self):
        self.rows([('agent', {'id': 'agent', 'accountKey': 'first', 'threadId': fixture.THREAD}),
                   ('second', {'id': 'second', 'accountKey': 'first', 'threadId': 'old-thread'})])
        self.runtime.path.write_bytes(fixture.line('session_meta', {'id': fixture.THREAD}))
        self.assertTrue(self.runtime.analytics_history_step())
        with self.runtime.db() as db:
            db.execute('UPDATE runtime_agents SET record=? WHERE id=?', (json.dumps(
                {'id': 'second', 'accountKey': 'second', 'threadId': fixture.OTHER_THREAD}), 'second'))
        path = self.runtime.homes['second'] / 'sessions' / '2026' / (
            'rollout-' + fixture.OTHER_THREAD + '.jsonl')
        path.write_bytes(fixture.line('session_meta', {'id': fixture.OTHER_THREAD}) +
                         fixture.line('response_item', {'type': 'message', 'role': 'user', 'content': []}))
        self.assertTrue(self.runtime.analytics_history_step())
        captured = self.runtime.captured()
        self.assertEqual(len(captured), 1)
        self.assertEqual(captured[0][2]['a']['id'], 'second')
        self.assertEqual(captured[0][2]['a']['accountKey'], 'second')
        self.assertEqual(captured[0][2]['a']['threadId'], fixture.OTHER_THREAD)
        with self.runtime.db() as db:
            ids = {row[0] for row in db.execute('SELECT id FROM analytics_history')}
        self.assertIn('second:second:' + fixture.OTHER_THREAD, ids)
        self.assertNotIn('second:first:old-thread', ids)

    def test_real_runtime_keeps_budget_and_checkpoint_after_projection(self):
        spec = importlib.util.spec_from_file_location(
            'projection_budget_fixture', Path(__file__).with_name('analytics-history-lock-order-contract.py'))
        budget_fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(budget_fixture)
        case = budget_fixture.HistoryLockOrder()
        case.setUp()
        self.addCleanup(case.doCleanups)
        runtime = case.runtime
        self.assertTrue(runtime.analytics_history_step())
        self.assertTrue(runtime.analytics_history_step())
        self.assertFalse(runtime.analytics_history_step())
        before = case.usage()
        self.assertEqual(before[0]['spent'], 100)
        self.assertEqual(before[1], [('response', 0), ('response', 100)])
        self.assertEqual(before[3]['offset'], case.path.stat().st_size)
        self.assertEqual(before[3]['status'], 'current')
        self.assertEqual([runtime.analytics_history_step() for _ in range(3)], [False] * 3)
        self.assertEqual(case.usage(), before)


if __name__ == '__main__':
    unittest.main()
