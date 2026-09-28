#!/usr/bin/env python3
"""Transcript projection deltas keep unchanged history off the wire."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_sync import SyncStore


class TranscriptStreamingWriteVolumeContract(unittest.TestCase):
    def test_cursor_returns_full_then_changed_items_and_recovers_stale_cursor(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'sync.sqlite3'
            items = [{'id': f'msg-{i}', 'text': 'x' * 900, 'role': 'assistant'} for i in range(400)]

            def connect():
                return sqlite3.connect(path, timeout=10)

            store = SyncStore(connect, lambda: {}, lambda _key: {'items': items, 'agent': {'id': 'fixture'}})
            first = store.pull('transcript:fixture', 0)
            self.assertEqual(len(first['documents']), 1)
            cursor = first['checkpoint']['seq']
            self.assertEqual(len(json.loads(first['documents'][0]['payload'])['items']), 400)

            items[0] = {**items[0], 'text': items[0]['text'] + 'delta'}
            changed = store.pull('transcript:fixture', cursor)
            patch = json.loads(changed['documents'][0]['payload'])
            self.assertTrue(patch['delta'])
            self.assertEqual([item['id'] for item in patch['items']], ['msg-0'])
            self.assertEqual(set(patch['itemRevisions']), {'msg-0'})
            self.assertNotIn('order', patch)
            next_cursor = changed['checkpoint']['seq']
            self.assertEqual(store.pull('transcript:fixture', next_cursor)['documents'], [])

            stale = store.pull('transcript:fixture', cursor)
            self.assertNotIn('delta', json.loads(stale['documents'][0]['payload']))
            self.assertEqual(len(json.loads(stale['documents'][0]['payload'])['items']), 400)


if __name__ == '__main__':
    unittest.main()
