#!/usr/bin/env python3
"""Portable history preserves data without replay, including failed exports."""
from contextlib import contextmanager
import copy
import hashlib
import json
from pathlib import Path
import sqlite3
import stat
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_portable_history import CONTEXT_CHARS, export_history, history_context
from codex_native_errors import NativeRpcError


class Runtime:
    def __init__(self, root):
        self.root = Path(root)
        with self.db() as db:
            db.executescript('''
                CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);
                CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, text TEXT, kind TEXT);
                CREATE TABLE runtime_search_rows(id TEXT PRIMARY KEY, search_rowid INTEGER);
                CREATE TABLE runtime_search(body TEXT);
            ''')

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.root / 'test.sqlite3')
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    def add(self, identity, text, **extra):
        item = {'id': identity, 'role': 'assistant', 'text': text, **extra}
        with self.db() as db:
            db.execute('INSERT INTO runtime_items VALUES(?,?,?,?)', (identity, 'chat', json.dumps(item), 1))
        return item


class Native:
    def __init__(self, turns=None, item_pages=None):
        self.turns = turns or []
        self.item_pages = item_pages or {}
        self.calls = []
        self.fail = None
        self.page_size = 2

    def call(self, method, params, timeout=30):
        self.calls.append((method, copy.deepcopy(params)))
        if self.fail:
            raise self.fail
        assert method in {'thread/read', 'thread/turns/list', 'thread/turns/items/list'}, method
        if method == 'thread/read':
            return {'thread': {'id': params['threadId'], 'turns': []}, 'env': {'SECRET': 'never export'}}
        values = self.turns if method == 'thread/turns/list' else self.item_pages[params['turnId']]
        offset = int(params.get('cursor', 0))
        return {'data': copy.deepcopy(values[offset:offset+self.page_size]),
                'nextCursor': str(offset+self.page_size) if offset+self.page_size < len(values) else None}


def turn(identity, text='hello', **extra):
    return {'id': identity, 'status': 'completed', 'items': [
        {'id': identity + '-message', 'type': 'agentMessage', 'text': text}], **extra}


class PortableHistoryContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio history $ quotes ')
        self.addCleanup(self.temp.cleanup)
        self.rt = Runtime(self.temp.name)
        self.agent = {'id': 'chat', 'epoch': 2, 'accountKey': 'claude-local', 'threadId': 'native'}

    def export(self, native=None, **changes):
        return export_history(self.rt, {**self.agent, **changes}, 'transfer-1', native or Native())

    def records(self, descriptor):
        return [json.loads(line) for line in Path(descriptor['path']).read_text().splitlines()]

    def test_all_native_pages_preserve_imported_history_beyond_excerpt(self):
        native = Native([turn(str(i), f'Historical turn {i}\n' + 'x' * 17000) for i in range(7)])
        descriptor = self.export(native)
        pages = [r for r in self.records(descriptor) if r['kind'] == 'native_turn_page']
        self.assertEqual([t for p in pages for t in p['page']['data']], native.turns)
        self.assertEqual(len([c for c in native.calls if c[0] == 'thread/turns/list']), 4)
        self.assertEqual(descriptor['counts']['native_turn_page'], 4)
        self.assertGreater(descriptor['bytes'], 7 * 17000)
        context = history_context(self.rt, descriptor)
        self.assertLessEqual(len(context), CONTEXT_CHARS)
        self.assertIn('not a new user request', context)
        self.assertIn(json.dumps(descriptor['path']), context)
        self.assertNotIn('never export', Path(descriptor['path']).read_text())

    def test_partial_turn_items_are_fully_paged_and_scoped(self):
        entries = [{'turnId': 'turn-a', 'item': {'id': str(i), 'type': 'commandExecution',
                    'aggregatedOutput': 'full output\n' * 1000}} for i in range(5)]
        native = Native([turn('turn-a', itemsView='summary')], {'turn-a': entries})
        descriptor = self.export(native)
        pages = [r for r in self.records(descriptor) if r['kind'] == 'native_item_page']
        self.assertEqual([i for p in pages for i in p['page']['data']], entries)
        self.assertEqual(len(pages), 3)
        self.assertTrue(all(p['request']['threadId'] == 'native' for p in pages))

    def test_claude_bare_items_are_supported(self):
        entries = [{'id': 'tool', 'type': 'dynamicToolCall', 'output': ['one', 'two']}]
        native = Native([turn('turn-a', items=[], itemsView='notLoaded')], {'turn-a': entries})
        descriptor = self.export(native)
        pages = [r for r in self.records(descriptor) if r['kind'] == 'native_item_page']
        self.assertEqual(pages[0]['page']['data'], entries)

    def test_full_studio_text_inputs_and_assets_stay_exact(self):
        filename = '/tmp/a "quoted" $file_日本語.png'
        item = self.rt.add('chat:item', 'short', truncated=True, assets=[{'path': filename, 'name': 'image.png'}],
                           inputs=[{'id': 'input', 'text': 'short', 'truncated': True, 'assets': [{'path': filename}]}])
        full = 'all output\n' * 20000
        with self.rt.db() as db:
            db.execute('INSERT INTO runtime_search(rowid,body) VALUES(1,?)', (full,))
            db.execute('INSERT INTO runtime_search_rows VALUES(?,1)', ('chat:item',))
            db.execute('INSERT INTO runtime_events VALUES(?,?,?,?)', ('input', 'chat', 'original user input' * 3000, 'user'))
        descriptor = self.export()
        record = next(r for r in self.records(descriptor) if r['kind'] == 'studio_item')
        self.assertEqual(record['item'], item)
        self.assertEqual(record['fullText'], full)
        self.assertEqual(record['inputEvents'][0]['text'], 'original user input' * 3000)
        self.assertEqual(stat.S_IMODE(Path(descriptor['path']).stat().st_mode), 0o600)
        with self.rt.db() as db:
            self.assertEqual(json.loads(db.execute('SELECT record FROM runtime_items').fetchone()[0]), item)

    def test_retry_after_lost_response_reads_exact_archive_without_native_call(self):
        native = Native([turn('first')])
        original = self.export(native)
        original_bytes = Path(original['path']).read_bytes()
        native.fail = AssertionError('A retry must not ask the source again')
        retry = self.export(native, model='new destination model')
        self.assertEqual(retry, original)
        self.assertEqual(Path(retry['path']).read_bytes(), original_bytes)
        with self.assertRaisesRegex(ValueError, 'another source identity'):
            self.export(native, threadId='different')

    def test_failed_native_read_publishes_nothing_then_retry_succeeds(self):
        native = Native([turn('first')])
        native.fail = TimeoutError('lost read response')
        with self.assertRaises(TimeoutError):
            self.export(native)
        self.assertEqual(list((self.rt.root / 'portable-history').iterdir()), [])
        native.fail = None
        self.assertEqual(self.export(native)['counts']['native_turn_page'], 1)

    def test_tampered_archive_and_descriptor_are_rejected(self):
        descriptor = self.export(Native([turn('first')]))
        with self.assertRaisesRegex(ValueError, 'descriptor or archive changed'):
            history_context(self.rt, {**descriptor, 'sha256': '0' * 64})
        path = Path(descriptor['path'])
        path.write_bytes(path.read_bytes().replace(b'hello', b'wrong'))
        with self.assertRaisesRegex(ValueError, 'content hash changed'):
            history_context(self.rt, descriptor)
        with self.assertRaisesRegex(ValueError, 'content hash changed'):
            self.export()

    def test_wrong_thread_cursor_and_item_identities_fail_visibly(self):
        cases = [
            ('wrong thread', lambda method, params: {'thread': {'id': 'other'}} if method == 'thread/read' else {}),
            ('wrong page', lambda method, params: {'thread': {'id': 'native'}} if method == 'thread/read' else {'data': {}}),
            ('repeated cursor', lambda method, params: {'thread': {'id': 'native'}} if method == 'thread/read' else {'data': [turn(str(params.get('cursor')))], 'nextCursor': 'same'}),
        ]
        for name, callback in cases:
            with self.subTest(name=name):
                native = Native()
                native.call = lambda method, params, timeout=30: callback(method, params)
                with self.assertRaises(ValueError):
                    self.export(native)
        native = Native([turn('t', itemsView='summary')], {'t': [{'turnId': 'other', 'item': {'id': 'i', 'type': 'agentMessage'}}]})
        with self.assertRaisesRegex(ValueError, 'item turn identity'):
            self.export(native)
        with self.assertRaisesRegex(ValueError, 'unknown items view'):
            self.export(Native([turn('t', itemsView='mystery')]))

    def test_repeated_hops_keep_flat_ancestors_and_validate_all(self):
        first = self.export(Native([turn('original', 'First imported native history')]))
        agent = {**self.agent, 'threadId': 'second', 'epoch': 3, 'portableHistory': first}
        second = export_history(self.rt, agent, 'transfer-2', Native())
        agent.update(threadId='third', epoch=4, portableHistory=second)
        third = export_history(self.rt, agent, 'transfer-3', Native())
        ancestors = [r['archive'] for r in self.records(third) if r['kind'] == 'ancestor']
        self.assertEqual(ancestors, [first, second])
        self.assertIn('First imported native history', history_context(self.rt, third))
        Path(first['path']).write_bytes(Path(first['path']).read_bytes().replace(b'First imported', b'False imported'))
        with self.assertRaises(ValueError):
            history_context(self.rt, third)

    def test_path_traversal_symlink_and_other_chat_are_rejected(self):
        descriptor = self.export()
        with self.assertRaisesRegex(ValueError, 'outside'):
            history_context(self.rt, {**descriptor, 'path': '/tmp/elsewhere.jsonl'})
        path = Path(descriptor['path'])
        elsewhere = self.rt.root / 'copied.jsonl'
        path.rename(elsewhere)
        path.symlink_to(elsewhere)
        with self.assertRaisesRegex(ValueError, 'outside'):
            history_context(self.rt, descriptor)
        path.unlink()
        elsewhere.rename(path)
        with self.assertRaisesRegex(ValueError, 'another chat'):
            export_history(self.rt, {**self.agent, 'id': 'other', 'portableHistory': descriptor}, 'transfer-2', Native())

    def test_unavailable_full_studio_text_does_not_silently_truncate(self):
        self.rt.add('chat:item', 'short', truncated=True)
        with self.assertRaisesRegex(ValueError, 'full Studio item is unavailable'):
            self.export()
        self.assertEqual(list((self.rt.root / 'portable-history').iterdir()), [])

    def test_descriptor_hash_and_record_counts_match_published_bytes(self):
        self.rt.add('chat:item', 'text')
        descriptor = self.export(Native([turn('t')]))
        contents = Path(descriptor['path']).read_bytes()
        self.assertEqual(descriptor['sha256'], hashlib.sha256(contents).hexdigest())
        self.assertEqual(descriptor['bytes'], len(contents))
        self.assertEqual(sum(descriptor['counts'].values()), len(contents.splitlines()) - 1)

    def test_only_exact_unmaterialized_native_receipt_allows_empty_history(self):
        native = Native()
        original = native.call

        def unavailable(method, params, timeout=30):
            if method == 'thread/turns/list':
                raise NativeRpcError({'code': -32600, 'message': error_message})
            return original(method, params, timeout)

        native.call = unavailable
        error_message = 'thread native is not materialized yet; thread/turns/list is unavailable before first user message'
        for error_message in ('thread not found: native', error_message.replace('thread native ', 'thread other ')):
            with self.assertRaises(NativeRpcError):
                self.export(native)
        error_message = 'thread native is not materialized yet; thread/turns/list is unavailable before first user message'
        descriptor = self.export(native)
        record = next(r for r in self.records(descriptor) if r['kind'] == 'native_unmaterialized')
        self.assertEqual(record['threadId'], 'native')
        self.assertEqual(record['error']['message'], error_message)


if __name__ == '__main__':
    unittest.main(verbosity=2)
