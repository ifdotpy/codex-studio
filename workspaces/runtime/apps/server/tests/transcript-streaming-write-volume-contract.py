#!/usr/bin/env python3
"""Transcript projection deltas keep unchanged history off the wire."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import contextlib
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_sync import SyncStore, TRANSCRIPT_MAX_TOMBSTONES
import codex_sync_entities


class TranscriptStreamingWriteVolumeContract(unittest.TestCase):
    @contextlib.contextmanager
    def projected_store(self, item_count=3, before_writer=None):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sync.sqlite3'
            writer = sqlite3.connect(path)
            writer.execute('PRAGMA journal_mode=WAL')
            sql = []

            class Connection(sqlite3.Connection):
                def execute(db, statement, parameters=()):
                    if statement == 'BEGIN IMMEDIATE' and before_writer is not None:
                        before_writer(writer)
                    return super().execute(statement, parameters)

            @contextlib.contextmanager
            def connect():
                db = sqlite3.connect(path, timeout=.05, factory=Connection)
                db.set_trace_callback(sql.append)
                try:
                    with db:
                        yield db
                finally:
                    db.close()

            items = [{'id': f'msg-{i}', 'text': 'body', 'role': 'assistant'}
                     for i in range(item_count)]
            payload = {'items': items, 'agent': {'id': 'fixture'}}
            store = SyncStore(connect, lambda _key: payload)
            try:
                yield store, writer, payload, sql
            finally:
                writer.rollback()
                writer.close()

    def test_projected_full_and_sparse_replays_do_not_claim_the_writer(self):
        # A held external writer must not delay a replay of existing hashes,
        # regardless of whether the client needs a full page or a sparse delta.
        with self.projected_store() as (store, writer, payload, sql):
            scope = 'transcript:fixture'
            first = store.transcript_pull(scope, 0, payload, False)
            cursor = first['checkpoint']['seq']
            other = store.transcript_pull('transcript:other', 0, {'items': []}, False)
            payload['items'][0]['text'] = 'changed body'
            changed = store.transcript_pull(scope, cursor, payload, False)
            high = changed['checkpoint']['seq']
            expected_full = store.transcript_pull(scope, 0, payload, False)
            writer.execute('BEGIN IMMEDIATE')
            sql.clear()
            with patch.object(codex_sync_entities, 'next_sequence', side_effect=AssertionError(
                    'A projected replay must not allocate a sequence')):
                self.assertEqual(store.transcript_pull(scope, 0, payload, False), expected_full)
                self.assertEqual(store.transcript_pull(scope, cursor, payload, False), changed)
                caught_up = store.transcript_pull(scope, high, payload, False)
                self.assertEqual(caught_up['documents'], [])
                self.assertEqual(caught_up['checkpoint']['seq'], high)
                self.assertEqual(store.transcript_pull(
                    'transcript:other', 0, {'items': []}, False), other)
            self.assertNotIn('BEGIN IMMEDIATE', sql)
            self.assertFalse(any(statement.split()[0].upper() in {'INSERT', 'UPDATE', 'DELETE'}
                                 for statement in sql))
            sparse = json.loads(changed['documents'][0]['payload'])
            self.assertTrue(sparse['delta'])
            self.assertEqual([item['id'] for item in sparse['items']], ['msg-0'])

    def test_projected_tombstone_floor_and_deleted_replays_are_reads(self):
        with self.projected_store(TRANSCRIPT_MAX_TOMBSTONES + 2) as (store, writer, payload, sql):
            scope = 'transcript:fixture'
            first = store.transcript_pull(scope, 0, payload, False)
            stale_cursor = first['checkpoint']['seq']
            payload['items'].clear()
            changed = store.transcript_pull(scope, stale_cursor, payload, False)
            full = store.transcript_pull(scope, stale_cursor, payload, False)
            self.assertNotIn('delta', json.loads(full['documents'][0]['payload']))
            writer.execute('BEGIN IMMEDIATE')
            sql.clear()
            with patch.object(codex_sync_entities, 'next_sequence', side_effect=AssertionError(
                    'A replay below the retained tombstone floor must remain a read')):
                self.assertEqual(store.transcript_pull(scope, stale_cursor, payload, False), full)
            self.assertNotIn('BEGIN IMMEDIATE', sql)
            writer.rollback()
            deleted = store.transcript_pull(scope, changed['checkpoint']['seq'], {}, True)
            writer.execute('BEGIN IMMEDIATE')
            sql.clear()
            with patch.object(codex_sync_entities, 'next_sequence', side_effect=AssertionError(
                    'An existing deleted page must not allocate a sequence')):
                replay = store.transcript_pull(scope, stale_cursor, {}, True)
                self.assertTrue(replay['documents'][0]['_deleted'])
                self.assertEqual(replay['checkpoint'], deleted['checkpoint'])
                replay = store.transcript_pull(scope, 0, {}, True)
                self.assertTrue(replay['documents'][0]['_deleted'])
                self.assertEqual(replay['checkpoint'], deleted['checkpoint'])
            self.assertNotIn('BEGIN IMMEDIATE', sql)

    def test_changed_projection_rereads_after_reserving_the_writer(self):
        competing_change = []

        def remove_prior_hash(writer):
            if competing_change:
                competing_change.pop()
                writer.execute("DELETE FROM sync_entities WHERE collection='transcript:fixture' AND id='item:msg-1'")
                writer.commit()

        with self.projected_store(before_writer=remove_prior_hash) as (store, writer, payload, sql):
            scope = 'transcript:fixture'
            first = store.transcript_pull(scope, 0, payload, False)
            payload['items'][0]['text'] = 'changed body'
            competing_change.append(True)
            original = codex_sync_entities.next_sequence
            with patch.object(codex_sync_entities, 'next_sequence', wraps=original) as allocate:
                sql.clear()
                changed = store.transcript_pull(scope, first['checkpoint']['seq'], payload, False)
            self.assertEqual(allocate.call_count, 1)
            self.assertEqual(sql.count('BEGIN IMMEDIATE'), 1)
            writer_position = sql.index('BEGIN IMMEDIATE')
            self.assertIn('ROLLBACK', sql[:writer_position])
            ledger_reads = [index for index, statement in enumerate(sql)
                            if statement.startswith('SELECT id,seq,hash,deleted FROM sync_entities')
                            and 'AND seq>' not in statement]
            self.assertEqual(len(ledger_reads), 2)
            self.assertLess(ledger_reads[0], writer_position)
            self.assertGreater(ledger_reads[1], writer_position)
            self.assertGreater(changed['checkpoint']['seq'], first['checkpoint']['seq'])
            projected = json.loads(changed['documents'][0]['payload'])
            self.assertEqual({item['id'] for item in projected['items']}, {'msg-0', 'msg-1'})
            self.assertIsNotNone(writer.execute("SELECT hash FROM sync_entities WHERE "
                                               "collection='transcript:fixture' AND id='item:msg-1'").fetchone())
            self.assertEqual(store.transcript_pull(scope, changed['checkpoint']['seq'], payload, False)['documents'], [])

    @staticmethod
    def prepare_revision(store, connect):
        store._ensure_versions()
        with connect() as db:
            db.execute("INSERT INTO runtime_agents VALUES ('fixture', '{}')")

    @staticmethod
    def bump_revision(connect):
        with connect() as db:
            db.execute("UPDATE runtime_agents SET record=record WHERE id='fixture'")

    def test_cursor_returns_full_then_sparse_delta_for_lagging_cursor(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sync.sqlite3'
            items = [{'id': f'msg-{i}', 'text': 'x' * 900, 'role': 'assistant'} for i in range(400)]
            anchor = sqlite3.connect(path)
            anchor.execute('PRAGMA journal_mode=WAL')
            anchor.execute('PRAGMA wal_autocheckpoint=0')
            anchor.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)')
            anchor.execute('CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, payload TEXT)')
            anchor.commit()
            self.addCleanup(anchor.close)

            @contextlib.contextmanager
            def connect():
                db = sqlite3.connect(path, timeout=10)
                db.execute('PRAGMA wal_autocheckpoint=0')
                try:
                    with db:
                        yield db
                finally:
                    db.close()

            store = SyncStore(connect, lambda _key: {'items': items, 'agent': {'id': 'fixture'}})
            self.prepare_revision(store, connect)
            first = store.pull('transcript:fixture', 0)
            self.assertEqual(len(first['documents']), 1)
            cursor = first['checkpoint']['seq']
            self.assertEqual(len(json.loads(first['documents'][0]['payload'])['items']), 400)
            self.assertEqual(store.transcript_revision('fixture'), 1)
            state = store.pull('state:entities:v1', 0)
            state_cursor = state['checkpoint']['seq']
            anchor.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            wal_path = Path(str(path) + '-wal')
            wal_before = wal_path.stat().st_size

            # The cursor intentionally lags several item revisions, rather
            # than being exactly one version behind.
            for i in range(5):
                items[i] = {**items[i], 'text': items[i]['text'] + f'delta-{i}'}
            self.bump_revision(connect)
            changed = store.pull('transcript:fixture', cursor)
            patch = json.loads(changed['documents'][0]['payload'])
            self.assertTrue(patch['delta'])
            self.assertEqual({item['id'] for item in patch['items']}, {f'msg-{i}' for i in range(5)})
            self.assertEqual(set(patch['itemRevisions']), {f'msg-{i}' for i in range(5)})
            self.assertNotIn('order', patch)
            next_cursor = changed['checkpoint']['seq']
            self.assertEqual(store.pull('transcript:fixture', next_cursor)['documents'], [])
            self.assertEqual(store.pull('state:entities:v1', state_cursor)['documents'], [])

            # An unrelated runtime commit does not rebuild or advance this page.
            calls = []
            store.transcript = lambda key: calls.append(key) or {'items': items, 'agent': {'id': 'fixture'}}
            with connect() as db:
                db.execute("CREATE TABLE unrelated(id INTEGER)")
                db.execute("INSERT INTO unrelated VALUES (1)")
            revision_cursor = changed['checkpoint']['seq']
            unchanged = store.pull('transcript:fixture', revision_cursor)
            self.assertEqual(unchanged['documents'], [])
            self.assertEqual(calls, [])

            # A write to one transcript advances its agent revision and pulls its delta.
            items[5] = {**items[5], 'text': items[5]['text'] + '-changed'}
            with connect() as db:
                db.execute("INSERT INTO runtime_items VALUES ('item-1', 'fixture', '{}')")
            self.assertEqual(store.transcript_revision('fixture'), 3)
            changed = store.pull('transcript:fixture', revision_cursor)
            self.assertEqual(calls, ['fixture'])
            self.assertEqual(json.loads(changed['documents'][0]['payload'])['items'][0]['id'], 'msg-5')
            next_cursor = changed['checkpoint']['seq']

            items.reverse()
            self.bump_revision(connect)
            reordered = store.pull('transcript:fixture', next_cursor)
            order_delta = json.loads(reordered['documents'][0]['payload'])
            self.assertEqual(order_delta['order'], [item['id'] for item in items])
            self.assertEqual(order_delta['items'], [])

            with connect() as db:
                rows = db.execute("SELECT id,payload FROM sync_entities WHERE collection='transcript:fixture'").fetchall()
                self.assertTrue(rows)
                self.assertTrue(all(row[1] is None for row in rows))
                self.assertEqual(db.execute("SELECT count(*) FROM sync_documents WHERE scope='transcript:fixture'").fetchone()[0], 0)

    def test_pruned_tombstones_bound_rows_and_stale_cursor_gets_full_replace(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sync.sqlite3'
            items = [{'id': f'msg-{i}', 'text': 'body', 'role': 'assistant'} for i in range(520)]
            anchor = sqlite3.connect(path)
            anchor.execute('PRAGMA journal_mode=WAL')
            anchor.execute('PRAGMA wal_autocheckpoint=0')
            anchor.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)')
            anchor.commit()
            self.addCleanup(anchor.close)

            @contextlib.contextmanager
            def connect():
                db = sqlite3.connect(path, timeout=10)
                db.execute('PRAGMA wal_autocheckpoint=0')
                try:
                    with db:
                        yield db
                finally:
                    db.close()

            store = SyncStore(connect, lambda _key: {'items': items, 'agent': {'id': 'fixture'}})
            self.prepare_revision(store, connect)
            first = store.pull('transcript:fixture', 0)
            stale_cursor = first['checkpoint']['seq']
            cursor = stale_cursor
            for _ in range(TRANSCRIPT_MAX_TOMBSTONES + 2):
                items.pop(0)
                self.bump_revision(connect)
                result = store.pull('transcript:fixture', cursor)
                cursor = result['checkpoint']['seq']

            stale = store.pull('transcript:fixture', stale_cursor)
            payload = json.loads(stale['documents'][0]['payload'])
            self.assertNotIn('delta', payload)
            self.assertEqual(payload['items'], items)
            with connect() as db:
                count, payload_count = db.execute("""SELECT count(*),sum(payload IS NOT NULL)
                    FROM sync_entities WHERE collection='transcript:fixture'""").fetchone()
                tombstones = db.execute("""SELECT count(*) FROM sync_entities
                    WHERE collection='transcript:fixture' AND deleted=1""").fetchone()[0]
                self.assertLessEqual(tombstones, TRANSCRIPT_MAX_TOMBSTONES)
                self.assertLessEqual(count, len(items) + TRANSCRIPT_MAX_TOMBSTONES + 4)
                self.assertEqual(payload_count or 0, 0)

    def test_five_400_item_pulls_send_only_changed_items(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sync.sqlite3'
            items = [{'id': f'msg-{i:03d}', 'role': 'assistant',
                      'text': f'message {i:03d} ' + ('x' * 1080)} for i in range(400)]
            anchor = sqlite3.connect(path)
            anchor.execute('PRAGMA journal_mode=WAL')
            anchor.execute('PRAGMA wal_autocheckpoint=0')
            anchor.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)')
            anchor.commit()
            self.addCleanup(anchor.close)

            @contextlib.contextmanager
            def connect():
                db = sqlite3.connect(path, timeout=10)
                db.execute('PRAGMA wal_autocheckpoint=0')
                try:
                    with db:
                        yield db
                finally:
                    db.close()

            store = SyncStore(connect, lambda _key: {'items': items, 'agent': {'id': 'fixture'}})
            self.prepare_revision(store, connect)
            result = store.pull('transcript:fixture', 0)
            full_bytes = len(json.dumps(result, separators=(',', ':'), ensure_ascii=False).encode())
            bytes_after = full_bytes
            cursor = result['checkpoint']['seq']
            delta_sizes = []
            for index in range(4):
                items[index] = {**items[index], 'text': items[index]['text'] + ('x' * 20)}
                self.bump_revision(connect)
                result = store.pull('transcript:fixture', cursor)
                payload = json.loads(result['documents'][0]['payload'])
                self.assertEqual([item['id'] for item in payload['items']], [f'msg-{index:03d}'])
                size = len(json.dumps(result, separators=(',', ':'), ensure_ascii=False).encode())
                delta_sizes.append(size)
                bytes_after += size
                cursor = result['checkpoint']['seq']
            bytes_before = full_bytes * 5
            self.assertLess(bytes_after, bytes_before)
            print({'fivePullBytesBefore': bytes_before, 'fivePullBytesAfter': bytes_after,
                   'fullPullBytes': full_bytes, 'deltaPullBytes': delta_sizes,
                   'savingPercent': round((1 - bytes_after / bytes_before) * 100, 1)})

    def test_stream_revision_table_wal_bytes_per_minute(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sync.sqlite3'
            items = [{'id': f'msg-{i}', 'text': 'x' * 900, 'role': 'assistant'} for i in range(400)]
            items[0] = {'id': 'answer', 'text': '', 'role': 'assistant'}
            anchor = sqlite3.connect(path)
            anchor.execute('PRAGMA journal_mode=WAL')
            anchor.execute('PRAGMA wal_autocheckpoint=0')
            anchor.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT)')
            anchor.commit()
            self.addCleanup(anchor.close)

            @contextlib.contextmanager
            def connect():
                db = sqlite3.connect(path, timeout=10)
                db.execute('PRAGMA wal_autocheckpoint=0')
                try:
                    with db:
                        yield db
                finally:
                    db.close()

            store = SyncStore(connect, lambda _key: {'items': items, 'agent': {'id': 'fixture'}})
            self.prepare_revision(store, connect)
            full = store.pull('transcript:fixture', 0)
            cursor = full['checkpoint']['seq']
            anchor.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            wal_path = Path(str(path) + '-wal')
            before = wal_path.stat().st_size
            for index in range(54):
                size = (40000 * (index + 1) // 54) - (40000 * index // 54)
                items[0] = {**items[0], 'text': items[0]['text'] + ('x' * size)}
                self.bump_revision(connect)
                result = store.pull('transcript:fixture', cursor)
                cursor = result['checkpoint']['seq']
            written = wal_path.stat().st_size - before
            per_minute = written * 60 / 40.5
            with connect() as db:
                records = db.execute("SELECT payload FROM sync_entities WHERE collection='transcript:fixture'").fetchall()
                self.assertTrue(records)
                self.assertTrue(all(row[0] is None for row in records))
                self.assertEqual(db.execute("SELECT count(*) FROM sync_documents WHERE scope='transcript:fixture'").fetchone()[0], 0)
            print({'streamUpdates': 54, 'fixtureSeconds': 40.5,
                   'transcriptEntityWalBytes': written,
                   'transcriptEntityWalBytesPerMinute': round(per_minute)})


if __name__ == '__main__':
    unittest.main()
