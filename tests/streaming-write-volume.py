#!/usr/bin/env python3
"""Fixture for durable streaming, analytics parity, and SQLite WAL volume."""
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('runtime_contract_fixture', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import Runtime
from codex_streaming import StreamBuffer
from codex_transcript_history import history_item


class StreamingWriteVolume(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtimes = []

    def tearDown(self):
        for runtime in self.runtimes:
            runtime.close()
        self.temp.cleanup()

    def runtime(self, name):
        runtime = Runtime(Path(self.temp.name) / name, fixture.FakeServer)
        self.runtimes.append(runtime)
        agent = runtime.create({'name': name, 'cwd': self.temp.name, 'prompt': 'Fixture'})
        fixture.eventually(lambda: runtime.agent(agent['id'])['status'] == 'running')
        return runtime, runtime.agent(agent['id'])

    def send(self, runtime, agent, method, **params):
        runtime.notification({'method': method, 'params': {
            'threadId': agent['threadId'], 'turnId': agent['turnId'], **params}})

    def run_stream(self, name, parts, *, legacy=False, measure_wal=False):
        runtime, agent = self.runtime(name)
        self.send(runtime, agent, 'item/started', item={'id': 'answer', 'type': 'agentMessage', 'text': ''})
        reader = None
        if measure_wal:
            reader = sqlite3.connect(f'file:{runtime.db_path}?mode=ro', uri=True)
            reader.execute('BEGIN')
            reader.execute('SELECT count(*) FROM runtime_items').fetchone()
        wal = Path(str(runtime.db_path) + '-wal')
        before = wal.stat().st_size if wal.exists() else 0
        try:
            if legacy:
                with patch.object(StreamBuffer, 'enqueue', return_value=False):
                    for part in parts:
                        self.send(runtime, agent, 'item/agentMessage/delta', itemId='answer', delta=part)
            else:
                for part in parts:
                    self.send(runtime, agent, 'item/agentMessage/delta', itemId='answer', delta=part)
            self.send(runtime, agent, 'item/completed', item={'id': 'answer', 'type': 'agentMessage', 'text': ''.join(parts)})
            self.send(runtime, agent, 'thread/tokenUsage/updated', tokenUsage={
                'total': {'totalTokens': 123}, 'last': {'totalTokens': 123},
                'modelContextWindow': 10000})
            written = (wal.stat().st_size if wal.exists() else 0) - before
        finally:
            if reader:
                reader.close()
        with runtime.db() as db:
            notices = db.execute("SELECT sum(count),sum(bytes) FROM analytics_notifications WHERE agent=? AND method='item/agentMessage/delta'", (agent['id'],)).fetchone()
            item = json.loads(db.execute("SELECT record FROM analytics_items WHERE agent=? AND json_extract(record,'$.itemId')='answer'", (agent['id'],)).fetchone()[0])
            turn = json.loads(db.execute('SELECT record FROM analytics_turns WHERE agent=? ORDER BY at DESC LIMIT 1', (agent['id'],)).fetchone()[0])
            search = db.execute("SELECT body FROM runtime_search WHERE id=?", (agent['id'] + ':answer',)).fetchone()[0]
            budget = json.loads(db.execute('SELECT record FROM runtime_budget WHERE id=?', (agent['id'],)).fetchone()[0])
            usage = [json.loads(row[0])['total'] for row in db.execute('SELECT record FROM analytics_usage WHERE agent=?', (agent['id'],))]
        return {'written': written, 'notices': tuple(notices), 'stream': item['stream'],
                'firstOutputAt': turn.get('firstOutputAt'), 'events': runtime.agent(agent['id'])['events'],
                'search': search, 'text': runtime.transcript(agent['id'])['items'][-1]['text'],
                'budgetSpent': budget['spent'], 'usage': usage}

    def test_2000_deltas_40kb_wal_before_after(self):
        parts = ['abcdefghij123456789\n' for _ in range(2000)]
        before = self.run_stream('legacy', parts, legacy=True, measure_wal=True)
        after = self.run_stream('buffered', parts, measure_wal=True)
        print({'legacyWalBytes': before['written'], 'bufferedWalBytes': after['written'],
               'ratio': round(after['written'] / before['written'], 4)})
        self.assertEqual(before['text'], ''.join(parts)[:20000])
        self.assertEqual(after['text'], ''.join(parts)[:20000])
        self.assertEqual(before['search'], ''.join(parts))
        self.assertEqual(after['search'], ''.join(parts))
        self.assertLess(after['written'], before['written'] / 4)
        self.assertEqual(before['notices'], after['notices'])
        self.assertEqual(before['stream'], after['stream'])
        self.assertEqual(before['events'], after['events'])
        self.assertEqual((before['budgetSpent'], before['usage']), (after['budgetSpent'], after['usage']))

    def test_5mb_command_output_has_bounded_memory_and_wal(self):
        def measure(name, deltas):
            runtime, agent = self.runtime(name)
            item_id = agent['id'] + ':command'
            self.send(runtime, agent, 'item/started', item={
                'id': 'command', 'type': 'commandExecution', 'command': 'fixture'})
            reader = sqlite3.connect(f'file:{runtime.db_path}?mode=ro', uri=True)
            reader.execute('BEGIN')
            reader.execute('SELECT count(*) FROM runtime_items').fetchone()
            wal = Path(str(runtime.db_path) + '-wal')
            before = wal.stat().st_size if wal.exists() else 0
            try:
                # Five equal flushes isolate payload size from timer scheduling.
                with patch.object(StreamBuffer, '_schedule_locked', return_value=None):
                    for index in range(deltas):
                        self.send(runtime, agent, 'item/commandExecution/outputDelta',
                                  itemId='command', delta='x' * 100)
                        if (index + 1) % (deltas // 5) == 0:
                            entry = next(iter(runtime._stream_buffer.entries.values()))
                            self.assertLessEqual(len(entry['batches'][0][0]), 12000)
                            with runtime.lock, runtime.db() as db:
                                runtime._stream_buffer.flush_locked(db)
                                self.assertIsNone(db.execute('SELECT 1 FROM runtime_search WHERE id=?',
                                                             (item_id,)).fetchone())
                            self.assertLessEqual(len(entry['base']), 12000)
                    self.send(runtime, agent, 'item/completed', item={
                        'id': 'command', 'type': 'commandExecution', 'command': 'fixture', 'exitCode': 0})
                written = (wal.stat().st_size if wal.exists() else 0) - before
            finally:
                reader.close()
            with runtime.db() as db:
                stored = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?',
                                               (item_id,)).fetchone()[0])
                command = json.loads(stored['text'])
                analytics = json.loads(db.execute("SELECT record FROM analytics_items WHERE agent=? "
                                                  "AND json_extract(record,'$.itemId')='command'",
                                                  (agent['id'],)).fetchone()[0])
                indexed = db.execute('SELECT body FROM runtime_search WHERE id=?', (item_id,)).fetchone()
            self.assertEqual(command['aggregatedOutput'], 'x' * 12000)
            self.assertTrue(command['outputTruncated'])
            self.assertEqual(analytics['stream']['chars'], deltas * 100)
            self.assertEqual(analytics['stream']['bytes'], deltas * 100)
            self.assertEqual(analytics['stream']['deltas'], deltas)
            self.assertIsNotNone(indexed)
            self.assertLessEqual(len(json.loads(indexed[0])['aggregatedOutput']), 12000)
            return written

        one_mb = measure('command-1mb', 10000)
        five_mb = measure('command-5mb', 50000)
        print({'commandWal1mb': one_mb, 'commandWal5mb': five_mb,
               'ratio': round(five_mb / one_mb, 3)})
        self.assertLess(five_mb, one_mb * 2)

    def test_command_analytics_match_legacy_after_truncation(self):
        snapshots = []
        for name, legacy in (('command-legacy', True), ('command-buffered', False)):
            runtime, agent = self.runtime(name)
            self.send(runtime, agent, 'item/started', item={
                'id': 'command', 'type': 'commandExecution', 'command': 'fixture'})
            parts = ['é\n' * 100 for _ in range(100)]
            if legacy:
                with patch.object(StreamBuffer, 'enqueue', return_value=False):
                    for part in parts:
                        self.send(runtime, agent, 'item/commandExecution/outputDelta',
                                  itemId='command', delta=part)
            else:
                for part in parts:
                    self.send(runtime, agent, 'item/commandExecution/outputDelta',
                              itemId='command', delta=part)
            self.send(runtime, agent, 'item/completed', item={
                'id': 'command', 'type': 'commandExecution', 'command': 'fixture', 'exitCode': 0})
            with runtime.db() as db:
                notices = tuple(db.execute("SELECT sum(count),sum(bytes) FROM analytics_notifications "
                                           "WHERE agent=? AND method='item/commandExecution/outputDelta'",
                                           (agent['id'],)).fetchone())
                item = json.loads(db.execute("SELECT record FROM analytics_items WHERE agent=? "
                                             "AND json_extract(record,'$.itemId')='command'",
                                             (agent['id'],)).fetchone()[0])
                row = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?',
                                            (agent['id'] + ':command',)).fetchone()[0])
            snapshots.append((notices, item['stream'], json.loads(row['text'])['aggregatedOutput']))
        self.assertEqual(snapshots[0], snapshots[1])
        self.assertEqual(snapshots[1][1]['bytes'], 30000)
        self.assertEqual(snapshots[1][1]['chars'], 20000)
        self.assertEqual(snapshots[1][1]['lines'], 10000)

    def test_interleaved_agents_items_and_terminal_outcomes(self):
        runtime, first = self.runtime('interleaved')
        second = runtime.create({'name': 'second', 'cwd': self.temp.name, 'prompt': 'Fixture'})
        fixture.eventually(lambda: runtime.agent(second['id'])['status'] == 'running')
        second = runtime.agent(second['id'])
        for agent in (first, second):
            for item in ('one', 'two'):
                self.send(runtime, agent, 'item/started', item={'id': item, 'type': 'agentMessage', 'text': ''})
        for index in range(20):
            for agent in (first, second):
                for item in ('one', 'two'):
                    self.send(runtime, agent, 'item/agentMessage/delta', itemId=item,
                              delta=f'{agent["id"][:4]}:{item}:{index};')
        self.send(runtime, first, 'turn/completed', turn={'id': first['turnId'], 'status': 'interrupted'})
        self.send(runtime, second, 'turn/completed', turn={'id': second['turnId'], 'status': 'failed'})
        for agent in (first, second):
            transcript = runtime.transcript(agent['id'])
            for item in ('one', 'two'):
                row = next(row for row in transcript['items'] if row['id'].endswith(':' + item))
                self.assertEqual(row['text'], ''.join(f'{agent["id"][:4]}:{item}:{i};' for i in range(20)))
                self.assertEqual(row['turnStatus'], 'interrupted' if agent is first else 'failed')

    def test_stream_becomes_visible_within_bound(self):
        runtime, agent = self.runtime('visible')
        self.send(runtime, agent, 'item/started', item={'id': 'answer', 'type': 'agentMessage', 'text': ''})
        started = time.monotonic()
        self.send(runtime, agent, 'item/agentMessage/delta', itemId='answer', delta='visible text')
        fixture.eventually(lambda: any(row['id'].endswith(':answer') and row['text'] == 'visible text'
                                       for row in runtime.transcript(agent['id'])['items']), timeout=1.5)
        self.assertLess(time.monotonic() - started, 1.5)

    def test_analytics_parity_and_flush_on_stop(self):
        parts = ['a\n', 'é', 'tail'] * 10
        before = self.run_stream('parity-legacy', parts, legacy=True)
        after = self.run_stream('parity-buffered', parts)
        for key in ('notices', 'stream', 'events', 'text', 'budgetSpent', 'usage'):
            self.assertEqual(before[key], after[key], key)
        self.assertIsNotNone(before['firstOutputAt'])
        self.assertIsNotNone(after['firstOutputAt'])
        runtime, agent = self.runtime('stop')
        self.send(runtime, agent, 'item/started', item={'id': 'partial', 'type': 'agentMessage', 'text': ''})
        self.send(runtime, agent, 'item/agentMessage/delta', itemId='partial', delta='persist before interrupt')
        runtime.stop(agent['id'])
        row = next(row for row in runtime.transcript(agent['id'])['items'] if row['id'].endswith(':partial'))
        self.assertEqual(row['text'], 'persist before interrupt')

    def test_command_output_tail_survives_flush_completion_and_restart(self):
        runtime, agent = self.runtime('command-restart')
        self.send(runtime, agent, 'item/started', item={
            'id': 'command', 'type': 'commandExecution', 'command': 'fixture'})
        output = 'output\n' * 4000
        for part in (output[:14000], output[14000:]):
            self.send(runtime, agent, 'item/commandExecution/outputDelta', itemId='command', delta=part)
        fixture.eventually(lambda: len(runtime.task_detail(agent['id'] + ':command').get('tail', '')) == 12000)
        with runtime.db() as db:
            body = db.execute('SELECT body FROM runtime_search WHERE id=?',
                              (agent['id'] + ':command',)).fetchone()
            self.assertIsNone(body)
        self.assertLessEqual(len(next(iter(runtime._stream_buffer.entries.values()))['base']), 12000)
        self.send(runtime, agent, 'item/completed', item={
            'id': 'command', 'type': 'commandExecution', 'command': 'fixture', 'exitCode': 0})
        row = history_item(runtime, agent['id'], agent['id'] + ':command')
        self.assertEqual(json.loads(row['text'])['aggregatedOutput'], output[-12000:])
        self.assertTrue(json.loads(row['text'])['outputTruncated'])
        runtime.close()
        reopened = Runtime(Path(self.temp.name) / 'command-restart', fixture.FakeServer)
        self.runtimes.append(reopened)
        row = history_item(reopened, agent['id'], agent['id'] + ':command')
        self.assertEqual(json.loads(row['text'])['aggregatedOutput'], output[-12000:])

    def test_multiple_flushes_preserve_full_assistant_text(self):
        runtime, agent = self.runtime('assistant-multiple')
        self.send(runtime, agent, 'item/started', item={'id': 'answer', 'type': 'agentMessage', 'text': ''})
        parts = ['a' * 15000, 'b' * 15000, 'c' * 15000]
        for index, part in enumerate(parts):
            self.send(runtime, agent, 'item/agentMessage/delta', itemId='answer', delta=part)
            expected = ''.join(parts[:index + 1])
            fixture.eventually(lambda: history_item(runtime, agent['id'], agent['id'] + ':answer')['text'] == expected)
        # A missing authoritative text must keep every flushed delta.
        self.send(runtime, agent, 'item/completed', item={'id': 'answer', 'type': 'agentMessage'})
        row = history_item(runtime, agent['id'], agent['id'] + ':answer')
        self.assertEqual(row['text'], ''.join(parts))
        self.assertFalse(row['streaming'])

    def test_authoritative_completion_replaces_draft_once(self):
        runtime, agent = self.runtime('authoritative')
        self.send(runtime, agent, 'item/started', item={'id': 'answer', 'type': 'agentMessage', 'text': ''})
        self.send(runtime, agent, 'item/agentMessage/delta', itemId='answer', delta='draft')
        self.send(runtime, agent, 'item/completed', item={
            'id': 'answer', 'type': 'agentMessage', 'text': 'corrected final'})
        row = history_item(runtime, agent['id'], agent['id'] + ':answer')
        self.assertEqual(row['text'], 'corrected final')
        self.assertFalse(row['streaming'])

    def test_stale_delta_keeps_notification_count_without_reopening_item(self):
        snapshots = []
        for name, legacy in (('stale-legacy', True), ('stale-buffered', False)):
            runtime, agent = self.runtime(name)
            self.send(runtime, agent, 'turn/completed', turn={'id': agent['turnId'], 'status': 'completed'})
            message = {'method': 'item/agentMessage/delta', 'params': {
                'threadId': agent['threadId'], 'turnId': agent['turnId'],
                'itemId': 'late', 'delta': 'ignored text'}}
            if legacy:
                with patch.object(StreamBuffer, 'enqueue', return_value=False):
                    runtime.notification(message)
            else:
                runtime.notification(message)
                with runtime.lock, runtime.db() as db:
                    runtime._stream_buffer.flush_locked(db, force=True)
            with runtime.db() as db:
                notice = tuple(db.execute("SELECT sum(count),sum(bytes) FROM analytics_notifications "
                                          "WHERE agent=? AND method='item/agentMessage/delta'",
                                          (agent['id'],)).fetchone())
                item = db.execute('SELECT 1 FROM runtime_items WHERE id=?', (agent['id'] + ':late',)).fetchone()
            snapshots.append((notice, runtime.agent(agent['id'])['events'], item))
        self.assertEqual(snapshots[0], snapshots[1])

    def test_background_command_keeps_stream_after_turn_completion(self):
        runtime, agent = self.runtime('background-command')
        item = {'id': 'command', 'type': 'commandExecution', 'command': 'fixture', 'processId': '42'}
        self.send(runtime, agent, 'item/started', item=item)
        self.send(runtime, agent, 'item/commandExecution/outputDelta', itemId='command', delta='before\n')
        self.send(runtime, agent, 'turn/completed', turn={'id': agent['turnId'], 'status': 'completed'})
        self.send(runtime, agent, 'item/commandExecution/outputDelta', itemId='command', delta='after\n')
        self.send(runtime, agent, 'item/completed', item={**item, 'exitCode': 0})
        row = history_item(runtime, agent['id'], agent['id'] + ':command')
        self.assertEqual(json.loads(row['text'])['aggregatedOutput'], 'before\nafter\n')
        self.assertEqual(runtime.task_detail(agent['id'] + ':command')['tail'], 'before\nafter\n')

    def test_native_error_flushes_pending_text_immediately(self):
        runtime, agent = self.runtime('native-error')
        self.send(runtime, agent, 'item/started', item={'id': 'answer', 'type': 'agentMessage', 'text': ''})
        self.send(runtime, agent, 'item/agentMessage/delta', itemId='answer', delta='before error')
        self.send(runtime, agent, 'error', error={'message': 'temporary', 'codexErrorInfo': 'other'},
                  willRetry=True)
        row = history_item(runtime, agent['id'], agent['id'] + ':answer')
        self.assertEqual(row['text'], 'before error')


if __name__ == '__main__':
    unittest.main()
