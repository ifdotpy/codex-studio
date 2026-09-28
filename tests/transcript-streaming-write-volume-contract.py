#!/usr/bin/env python3
"""Transcript projection deltas keep unchanged history off the wire."""
import json
import contextlib
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_sync import SyncStore


class TranscriptStreamingWriteVolumeContract(unittest.TestCase):
    def test_cursor_returns_full_then_sparse_delta_for_lagging_cursor(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sync.sqlite3'
            items = [{'id': f'msg-{i}', 'text': 'x' * 900, 'role': 'assistant'} for i in range(400)]
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

            store = SyncStore(connect, lambda: {}, lambda _key: {'items': items, 'agent': {'id': 'fixture'}})
            first = store.pull('transcript:fixture', 0)
            self.assertEqual(len(first['documents']), 1)
            cursor = first['checkpoint']['seq']
            self.assertEqual(len(json.loads(first['documents'][0]['payload'])['items']), 400)
            anchor.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            wal_path = Path(str(path) + '-wal')
            wal_before = wal_path.stat().st_size

            # The cursor intentionally lags several item revisions, rather
            # than being exactly one version behind.
            for i in range(5):
                items[i] = {**items[i], 'text': items[i]['text'] + f'delta-{i}'}
            changed = store.pull('transcript:fixture', cursor)
            patch = json.loads(changed['documents'][0]['payload'])
            self.assertTrue(patch['delta'])
            self.assertEqual({item['id'] for item in patch['items']}, {f'msg-{i}' for i in range(5)})
            self.assertEqual(set(patch['itemRevisions']), {f'msg-{i}' for i in range(5)})
            self.assertNotIn('order', patch)
            next_cursor = changed['checkpoint']['seq']
            self.assertEqual(store.pull('transcript:fixture', next_cursor)['documents'], [])

            with connect() as db:
                rows = db.execute("SELECT id,payload FROM sync_entities WHERE collection='transcript:fixture'").fetchall()
                self.assertTrue(rows)
                self.assertTrue(all(row[1] is None for row in rows))
                self.assertEqual(db.execute("SELECT count(*) FROM sync_documents WHERE scope='transcript:fixture'").fetchone()[0], 0)


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

            store = SyncStore(connect, lambda: {}, lambda _key: {'items': items, 'agent': {'id': 'fixture'}})
            full = store.pull('transcript:fixture', 0)
            cursor = full['checkpoint']['seq']
            anchor.execute('PRAGMA wal_checkpoint(TRUNCATE)')
            wal_path = Path(str(path) + '-wal')
            before = wal_path.stat().st_size
            for index in range(54):
                size = (40000 * (index + 1) // 54) - (40000 * index // 54)
                items[0] = {**items[0], 'text': items[0]['text'] + ('x' * size)}
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
