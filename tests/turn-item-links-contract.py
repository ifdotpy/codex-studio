#!/usr/bin/env python3
"""Private SQLite fixtures for bounded terminal item links. No native requests."""
from __future__ import annotations

import hashlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_turn_item_links as links


class HookConnection(sqlite3.Connection):
    before_writer = None
    fail_cursor = False
    payload_read_transactions = None

    def execute(self, sql, parameters=()):
        if sql == 'BEGIN IMMEDIATE' and self.before_writer:
            callback, self.before_writer = self.before_writer, None
            callback()
        if 'json_extract(record' in sql and sql.startswith('SELECT') and self.payload_read_transactions is not None:
            self.payload_read_transactions.append(self.in_transaction)
        if self.fail_cursor and sql.startswith('UPDATE ' + links.STATE + ' SET cursor='):
            raise RuntimeError('simulated crash before cursor commit')
        return super().execute(sql, parameters)


class TurnItemLinksContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / 'canvas.sqlite3'
        self.db = self.connect()
        self.db.executescript('CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL,created REAL NOT NULL);'
                              'CREATE INDEX runtime_item_agent ON runtime_items(agent,created);')

    def tearDown(self):
        self.db.close()
        self.temp.cleanup()

    def connect(self):
        db = sqlite3.connect(self.path, factory=HookConnection)
        db.execute('PRAGMA journal_mode=WAL')
        return db

    def insert(self, key, turn='current', *, agent='agent', created=20, text='text', db=None, rowid=None):
        db = self.db if db is None else db
        record = json.dumps({'id': key, 'turnId': turn, 'text': text, 'requestId': 'request:' + key})
        if rowid is None:
            db.execute('INSERT INTO runtime_items VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record,agent=excluded.agent,created=excluded.created',
                       (key, agent, record, created))
        else:
            db.execute('INSERT INTO runtime_items(rowid,id,agent,record,created) VALUES(?,?,?,?,?)',
                       (rowid, key, agent, record, created))
        db.commit()

    def finish(self, **options):
        for _ in range(100):
            if links.backfill_batch(self.db, **options).complete:
                return
        self.fail('backfill did not complete')

    def snapshot(self):
        return self.db.execute('SELECT id,agent,record,created FROM runtime_items ORDER BY id').fetchall()

    def status(self, key):
        return json.loads(self.db.execute('SELECT record FROM runtime_items WHERE id=?', (key,)).fetchone()[0]).get('turnStatus')

    def test_exact_old_query_before_coverage_and_after_missing_trigger(self):
        self.insert('current')
        self.assertTrue(links.ensure_tables(self.db))
        self.assertFalse(links._coverage(self.db).complete)
        links.update_turn_status(self.db, 'agent', 'current', 'failed', since=0)
        self.db.commit()
        self.assertEqual(self.status('current'), 'failed')
        self.finish()
        self.db.execute('DROP TRIGGER runtime_turn_item_link_insert_v1')
        self.insert('unlinked')
        self.assertIsNone(links._coverage(self.db))
        links.update_turn_status(self.db, 'agent', 'current', 'interrupted')
        self.db.commit()
        self.assertEqual(self.status('unlinked'), 'interrupted')
        self.assertTrue(links.ensure_tables(self.db))
        self.assertFalse(links._coverage(self.db).complete)
        self.finish()
        self.assertEqual(self.db.execute(f'SELECT turn_id FROM {links.LINKS} WHERE id=?', ('unlinked',)).fetchone()[0], 'current')

    def test_same_records_and_identities_for_all_terminal_statuses_and_windows(self):
        for turn in ('current', 'other', None, 1, True, '1', {'nested': 1}, ['current']):
            self.insert('row:' + json.dumps(turn), turn)
        self.insert('old-same-turn', created=-10)
        self.insert('other-agent', agent='other')
        original = self.snapshot()
        self.finish(row_limit=3)
        for status in ('completed', 'failed', 'interrupted', 'ended'):
            for turn_id in ('current', '1', None, '{"nested":1}', '["current"]'):
                for since in (None, 0, 21):
                    with self.subTest(status=status, turn_id=turn_id, since=since):
                        self.db.executemany('UPDATE runtime_items SET agent=?,record=?,created=? WHERE id=?',
                                            [(agent, raw, created, key) for key, agent, raw, created in original])
                        self.db.commit()
                        query = "UPDATE runtime_items SET record=json_set(record,'$.turnStatus',?) WHERE agent=? AND json_extract(record,'$.turnId')=?"
                        args = (status, 'agent', turn_id)
                        if since is not None:
                            query += ' AND created>=?'
                            args += (since,)
                        self.db.execute(query, args)
                        expected = self.snapshot()
                        self.db.executemany('UPDATE runtime_items SET agent=?,record=?,created=? WHERE id=?',
                                            [(agent, raw, created, key) for key, agent, raw, created in original])
                        links.update_turn_status(self.db, 'agent', turn_id, status, since=since)
                        self.assertEqual(self.snapshot(), expected)
                        self.db.commit()

    def test_racing_upsert_delete_and_low_rowid_insert_take_priority(self):
        self.insert('changed', 'old', rowid=-10)
        self.insert('deleted', rowid=20)
        self.insert('reused', 'old', rowid=30)
        self.assertTrue(links.ensure_tables(self.db))
        other = self.connect()
        try:
            def race():
                self.insert('changed', 'new', created=99, db=other)
                other.execute('DELETE FROM runtime_items WHERE id IN (?,?)', ('deleted', 'reused'))
                other.commit()
                self.insert('reused', 'new', db=other)
                self.insert('low-rowid', 'new', db=other, rowid=-20)
                self.insert('late', 'new', db=other)
            self.db.before_writer = race
            links.backfill_batch(self.db)
            self.finish()
            links.update_turn_status(self.db, 'agent', 'old', 'failed')
            self.db.commit()
            self.assertIsNone(self.status('changed'))
            self.assertIsNone(self.status('reused'))
            links.update_turn_status(self.db, 'agent', 'new', 'completed')
            self.db.commit()
            for key in ('changed', 'reused', 'low-rowid', 'late'):
                self.assertEqual(self.status(key), 'completed')
        finally:
            other.close()

    def test_rowid_only_move_behind_cursor_keeps_legacy_row_covered(self):
        self.insert('first', 'old')
        self.insert('moved', 'current')
        links.ensure_tables(self.db)
        links.backfill_batch(self.db, row_limit=1)
        self.assertEqual(links._coverage(self.db).cursor, 1)
        other = self.connect()
        try:
            other.execute("UPDATE runtime_items SET rowid=-20 WHERE id='moved'")
            other.commit()
        finally:
            other.close()
        self.finish()
        links.update_turn_status(self.db, 'agent', 'current', 'completed')
        self.db.commit()
        self.assertEqual(self.status('moved'), 'completed')

    def test_commit_failure_rolls_back_links_and_cursor_then_resumes_on_reopen(self):
        for i in range(7):
            self.insert(str(i))
        links.ensure_tables(self.db)
        links.backfill_batch(self.db, row_limit=2)
        before = links._coverage(self.db)
        before_rows = self.db.execute(f'SELECT * FROM {links.LINKS} ORDER BY id').fetchall()
        self.db.fail_cursor = True
        with self.assertRaisesRegex(RuntimeError, 'simulated crash'):
            links.backfill_batch(self.db, row_limit=2)
        self.assertEqual(links._coverage(self.db), before)
        self.assertEqual(self.db.execute(f'SELECT * FROM {links.LINKS} ORDER BY id').fetchall(), before_rows)
        self.assertFalse(self.db.in_transaction)
        self.db.close()
        self.db = self.connect()
        self.finish(row_limit=2)
        self.assertEqual(self.db.execute(f'SELECT COUNT(*) FROM {links.LINKS}').fetchone()[0], 7)

    def test_row_and_byte_budget_and_oversized_decode_outside_writer(self):
        for i in range(5):
            self.insert(str(i), text='x' * 100)
        self.insert('large', text='x' * 100_000)
        links.ensure_tables(self.db)
        self.db.payload_read_transactions = []
        first = links.backfill_batch(self.db, row_limit=2, byte_limit=500)
        self.assertLessEqual(first.rows, 2)
        self.assertLessEqual(first.source_bytes, 500)
        for _ in range(10):
            batch = links.backfill_batch(self.db, row_limit=2, byte_limit=500)
            if batch.oversized_row:
                self.assertEqual(batch.rows, 1)
                self.assertGreater(batch.source_bytes, 100_000)
                break
        else:
            self.fail('oversized row did not make progress')
        self.assertTrue(self.db.payload_read_transactions)
        self.assertFalse(any(self.db.payload_read_transactions))
        self.finish()

    def test_oversized_row_does_not_read_other_payload_sizes(self):
        record = json.dumps({'turnId': 'current', 'text': 'x' * 1024 * 1024})
        self.db.executemany('INSERT INTO runtime_items VALUES(?,?,?,?)',
                            [(str(i), 'agent', record, i) for i in range(128)])
        self.db.commit()
        links.ensure_tables(self.db)
        sizes = []
        self.db.create_function('length', 1, lambda value: sizes.append(len(value)) or len(value))
        first = links.backfill_batch(self.db, row_limit=128, byte_limit=512 * 1024)
        self.assertEqual(first.rows, 1)
        self.assertEqual(sizes, [len(record.encode())])
        self.assertEqual(links._coverage(self.db).cursor, 1)
        second = links.backfill_batch(self.db, row_limit=128, byte_limit=512 * 1024)
        self.assertEqual(second.rows, 1)
        self.assertEqual(sizes, [len(record.encode())] * 2)
        self.assertEqual(links._coverage(self.db).cursor, 2)

    def test_time_limit_stops_source_size_reads_between_rows(self):
        for i in range(12):
            self.insert(str(i), text='x' * 2048)
        links.ensure_tables(self.db)
        sizes = []
        self.db.create_function('length', 1, lambda value: sizes.append(len(value)) or len(value))
        clock = iter((0.0, .03))
        with patch.object(links.time, 'monotonic', side_effect=lambda: next(clock)):
            result = links.backfill_batch(self.db, row_limit=128, seconds_limit=.02)
        self.assertEqual(result.rows, 1)
        self.assertEqual(len(sizes), 1)

    def test_progress_counters_share_the_cursor_commit(self):
        for i in range(5):
            self.insert(str(i))
        expected_bytes = self.db.execute('SELECT SUM(length(CAST(record AS BLOB))) FROM runtime_items').fetchone()[0]
        links.ensure_tables(self.db)
        links.backfill_batch(self.db, row_limit=2)
        partial = self.db.execute(f'SELECT cursor,target,complete,scanned_rows,source_bytes,updated FROM {links.STATE}').fetchone()
        self.assertEqual(partial[:4], (2, 5, 0, 2))
        self.assertGreater(partial[4], 0)
        self.assertGreater(partial[5], 0)
        self.finish(row_limit=2)
        final = self.db.execute(f'SELECT cursor,target,complete,scanned_rows,source_bytes FROM {links.STATE}').fetchone()
        self.assertEqual(final, (5, 5, 1, 5, expected_bytes))

    def test_time_budget_yields_between_rows_and_restarts_resume_the_range(self):
        for i in range(4):
            self.insert(str(i))
        links.ensure_tables(self.db)
        clock = iter((0.0, .03))
        with patch.object(links.time, 'monotonic', side_effect=lambda: next(clock)):
            result = links.backfill_batch(self.db, seconds_limit=.02)
        self.assertEqual(result.rows, 1)
        self.assertFalse(result.complete)
        self.assertFalse(self.db.in_transaction)
        self.db.close()
        self.db = self.connect()
        self.finish()
        self.assertEqual(self.db.execute(f'SELECT COUNT(*) FROM {links.LINKS}').fetchone()[0], 4)

    def test_changed_coverage_discards_staged_values(self):
        self.insert('old')
        links.ensure_tables(self.db)
        other = self.connect()
        try:
            def race():
                other.execute('DROP TRIGGER runtime_turn_item_link_insert_v1')
                self.insert('late', db=other)
                self.assertTrue(links.ensure_tables(other))
            self.db.before_writer = race
            self.assertEqual(links.backfill_batch(self.db).rows, 0)
            self.assertFalse(links._coverage(self.db).complete)
            self.finish()
            links.update_turn_status(self.db, 'agent', 'current', 'completed')
            self.db.commit()
            self.assertEqual(self.status('old'), 'completed')
            self.assertEqual(self.status('late'), 'completed')
        finally:
            other.close()

    def test_busy_writer_yields_without_cursor_progress(self):
        self.insert('one')
        links.ensure_tables(self.db)
        other = self.connect()
        try:
            other.execute('BEGIN IMMEDIATE')
            before = links._coverage(self.db)
            result = links.backfill_batch(self.db)
            self.assertTrue(result.busy)
            self.assertEqual(links._coverage(self.db), before)
            self.assertFalse(self.db.in_transaction)
        finally:
            other.rollback()
            other.close()
        self.finish()

    def test_triggers_cover_external_writers_updates_and_replace(self):
        self.finish()
        other = self.connect()
        try:
            self.insert('one', db=other)
            other.execute("UPDATE runtime_items SET id='renamed',agent='other',created=77,record=json_set(record,'$.turnId','new') WHERE id='one'")
            other.commit()
            self.assertIsNone(self.db.execute(f'SELECT id FROM {links.LINKS} WHERE id=?', ('one',)).fetchone())
            links.update_turn_status(self.db, 'other', 'new', 'failed', since=76)
            self.db.commit()
            self.assertEqual(self.status('renamed'), 'failed')
            self.insert('renamed', 'replaced', db=other)
            other.execute('INSERT OR REPLACE INTO runtime_items VALUES(?,?,?,?)',
                          ('renamed', 'agent', json.dumps({'turnId': 'replaced'}), 20))
            other.commit()
            links.update_turn_status(self.db, 'agent', 'replaced', 'completed')
            self.db.commit()
            self.assertEqual(self.status('renamed'), 'completed')
            other.execute('DELETE FROM runtime_items')
            other.commit()
            self.assertEqual(self.db.execute(f'SELECT COUNT(*) FROM {links.LINKS}').fetchone()[0], 0)
        finally:
            other.close()

    def test_invalid_complete_marker_or_foreign_schema_fails_closed(self):
        self.insert('one')
        self.finish()
        self.db.execute(f'UPDATE {links.STATE} SET cursor=target+1')
        self.db.commit()
        self.assertIsNone(links._coverage(self.db))
        self.assertTrue(links.ensure_tables(self.db))
        self.assertFalse(links._coverage(self.db).complete)
        self.finish()
        self.db.execute('DROP TRIGGER runtime_turn_item_link_insert_v1')
        self.db.execute('CREATE TRIGGER runtime_turn_item_link_insert_v1 AFTER INSERT ON runtime_items BEGIN SELECT 1; END')
        self.db.commit()
        self.assertFalse(links.ensure_tables(self.db))
        links.update_turn_status(self.db, 'agent', 'current', 'completed')
        self.db.commit()
        self.assertEqual(self.status('one'), 'completed')

    def test_schema_install_does_not_decode_or_copy_history(self):
        for i in range(20):
            self.insert(str(i), text='x' * 10_000)
        queries = []
        self.db.set_trace_callback(queries.append)
        self.assertTrue(links.ensure_tables(self.db))
        self.db.set_trace_callback(None)
        self.assertFalse(any(q.startswith('SELECT') and 'json_extract' in q for q in queries))
        self.assertEqual(self.db.execute(f'SELECT COUNT(*) FROM {links.LINKS}').fetchone()[0], 0)
        self.assertTrue(any('SELECT MAX(rowid)' in q for q in queries))

    def test_fast_plan_uses_turn_scope_and_primary_key_with_same_checksum(self):
        self.db.executemany('INSERT INTO runtime_items VALUES(?,?,?,?)',
                            [(str(i), 'agent', json.dumps({'turnId': 'old', 'text': 'x' * 2048}), i)
                             for i in range(2000)])
        self.db.commit()
        for i in range(16):
            self.insert('current:' + str(i), created=3000 + i)
        original = self.snapshot()
        links.update_turn_status(self.db, 'agent', 'current', 'completed', since=0)
        expected = hashlib.sha256(repr(self.snapshot()).encode()).hexdigest()
        self.db.executemany('UPDATE runtime_items SET record=? WHERE id=?', [(raw, key) for key, _, raw, _ in original])
        self.db.commit()
        self.finish(row_limit=128)
        queries = []
        self.db.set_trace_callback(queries.append)
        links.update_turn_status(self.db, 'agent', 'current', 'completed', since=0)
        self.db.set_trace_callback(None)
        self.assertEqual(hashlib.sha256(repr(self.snapshot()).encode()).hexdigest(), expected)
        query = next(q for q in queries if q.startswith('UPDATE runtime_items SET record='))
        plan = '\n'.join(row[3] for row in self.db.execute('EXPLAIN QUERY PLAN ' + query))
        self.assertIn(links.INDEX, plan)
        self.assertIn('turn_id=?', plan)
        self.assertIn('sqlite_autoindex_runtime_items_1', plan)
        self.assertNotIn('runtime_item_agent', plan)


if __name__ == '__main__':
    unittest.main()
