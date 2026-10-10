#!/usr/bin/env python3
"""An analytics writer cannot hold committed user state or its runtime lock."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import closing, contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime, ResponseTimeout
from codex_streaming import StreamBuffer


class NoNativeServer:
    calls = 0

    def __init__(self, *_args, **_kwargs):
        type(self).calls += 1
        raise AssertionError('This contract cannot start a native transport')


class NotificationAnalyticsWriterContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-notification-analytics-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.calls_before = NoNativeServer.calls
        with patch.object(Runtime, 'schedule', lambda _runtime: None):
            self.runtime = Runtime(self.root, NoNativeServer)
        self.addCleanup(self.runtime.close)
        self.agent = {'id': 'fixture-agent', 'name': 'Original name', 'rootId': 'fixture-agent',
            'isLead': True, 'parentId': None, 'threadId': 'fixture-thread', 'turnId': 'fixture-turn',
            'inFlight': True, 'status': 'running', 'autoWake': False, 'epoch': 3, 'turnEpoch': 3,
            'events': 0, 'model': 'fixture-model', 'effort': 'medium', 'accountKey': 'default',
            'provider': 'codex', 'role': 'orchestrator', 'cwd': str(self.root), 'created': 1,
            'prompt': 'Fixture', 'tokensUsed': 0, 'tokenBudget': None, 'tail': ''}
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.agent)
            db.execute('CREATE TABLE fixture_receipts(id TEXT PRIMARY KEY)')
        with self.runtime.analytics_db() as db:
            db.execute('CREATE TABLE fixture_captures(seq INTEGER PRIMARY KEY, id INTEGER UNIQUE)')

    def tearDown(self):
        self.assertEqual(NoNativeServer.calls, self.calls_before)

    @contextmanager
    def writer(self, path):
        db = sqlite3.connect(path)
        db.execute('BEGIN IMMEDIATE')
        try:
            yield db
        finally:
            db.rollback()
            db.close()

    def count(self, path, table):
        with closing(sqlite3.connect(path)) as db:
            return db.execute('SELECT count(*) FROM ' + table).fetchone()[0]

    def captures(self):
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            return [row[0] for row in db.execute('SELECT id FROM fixture_captures ORDER BY rowid')]

    def drain(self):
        idle = getattr(self.runtime, '_analytics_capture_idle', None)
        if idle is not None:
            self.assertTrue(idle.wait(3), 'Analytics FIFO did not finish')

    def assert_main_available(self):
        acquired = self.runtime.lock.acquire(timeout=.15)
        self.assertTrue(acquired, 'Analytics retains Runtime.lock')
        self.runtime.lock.release()
        db = sqlite3.connect(self.runtime.db_path, timeout=.15)
        try:
            db.execute('BEGIN IMMEDIATE')
        finally:
            db.rollback()
            db.close()

    def run_callback(self, callback):
        finished = threading.Event()
        errors = []
        def run():
            try:
                callback()
            except BaseException as error:
                errors.append(error)
            finally:
                finished.set()
        worker = threading.Thread(target=run)
        worker.start()
        self.addCleanup(worker.join, 3)
        return worker, finished, errors

    def notice(self, method, **params):
        self.runtime.notification({'method': method, 'params': {
            'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'], **params}})

    def test_real_token_notice_commits_budget_before_a_blocked_analytics_writer(self):
        entered = threading.Event()
        original = self.runtime.analytics_agent
        def observed(db, agent):
            entered.set()
            return original(db, agent)
        params = {'responseId': 'exact-response', 'tokenUsage': {
            'total': {'totalTokens': 17}, 'last': {'totalTokens': 17, 'inputTokens': 7, 'outputTokens': 10}}}
        with patch.object(self.runtime, 'analytics_agent', observed):
            with self.writer(self.runtime.analytics_db_path):
                worker, finished, errors = self.run_callback(lambda: self.notice('thread/tokenUsage/updated', **params))
                self.assertTrue(entered.wait(2))
                self.assertTrue(finished.wait(.5), 'A native callback waits for the analytics writer')
                self.assertEqual(errors, [])
                self.assert_main_available()
                self.assertEqual(self.runtime.agent(self.agent['id'])['tokensUsed'], 17)
                self.assertEqual(self.count(self.runtime.db_path, 'runtime_budget_usage'), 1)
                self.assertEqual(self.count(self.runtime.analytics_db_path, 'analytics_usage'), 0)
            worker.join(3)
            self.drain()
        self.assertEqual(self.count(self.runtime.analytics_db_path, 'analytics_usage'), 1)
        self.notice('thread/tokenUsage/updated', **params)
        self.drain()
        self.assertEqual(self.count(self.runtime.db_path, 'runtime_budget_usage'), 1)
        self.assertEqual(self.count(self.runtime.analytics_db_path, 'analytics_usage'), 1)

    def test_zero_budget_capture_never_opens_another_main_writer(self):
        with self.writer(self.runtime.analytics_db_path) as analytics:
            self.notice('thread/tokenUsage/updated', responseId='zero-response', tokenUsage={
                'total': {'totalTokens': 0}, 'last': {'totalTokens': 0, 'inputTokens': 0, 'outputTokens': 0}})
            with self.writer(self.runtime.db_path):
                analytics.rollback()
                self.drain()
        self.assertEqual(self.count(self.runtime.analytics_db_path, 'analytics_usage'), 1)
        self.assertEqual(self.runtime.agent(self.agent['id'])['tokensUsed'], 0)

    def batch_notice(self, samples):
        return {'method': 'item/agentMessage/delta', 'params': {
            **samples[0], 'delta': ''.join(sample['delta'] for sample in samples)},
            '_studioNotificationSamples': samples}

    def batch_samples(self):
        return [{'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'],
                 'itemId': 'batch-item', 'delta': delta} for delta in ('one', 'two')]

    def start_batch_item(self):
        self.notice('item/started', item={'id': 'batch-item', 'type': 'agentMessage', 'text': ''})
        self.drain()

    def batch_item(self):
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            return json.loads(db.execute('SELECT record FROM analytics_items WHERE id=?',
                (':'.join((self.agent['id'], self.agent['threadId'], 'batch-item')),)).fetchone()[0])

    def test_coalesced_notice_queues_one_frozen_capture_outside_both_locks(self):
        self.start_batch_item()
        samples = self.batch_samples() * 64
        message = self.batch_notice(samples)
        entered = threading.Event()
        original = self.runtime.analytics_agent
        def observed(db, agent):
            entered.set()
            return original(db, agent)
        with patch.object(self.runtime, 'analytics_agent', observed), \
                patch.object(StreamBuffer, 'enqueue', return_value=False):
            with self.writer(self.runtime.analytics_db_path):
                worker, finished, errors = self.run_callback(lambda: self.runtime.notification(message))
                self.assertTrue(entered.wait(2))
                self.assertTrue(finished.wait(.5), 'Coalesced analytics retains the main writer')
                self.assertEqual(errors, [])
                self.assert_main_available()
                state = self.runtime.analytics_capture_status()
                self.assertEqual(state['queued'] + state['active'], 1)
                samples[0]['delta'] = 'Changed after the native callback'
                with self.runtime.db() as db:
                    agent = self.runtime.agent(self.agent['id'], db)
                    agent['name'] = 'Changed after the native callback'
                    self.runtime.put(db, 'agents', agent)
            worker.join(3)
            self.drain()
        item = self.batch_item()
        self.assertEqual(item['stream']['deltas'], 128)
        self.assertEqual(item['stream']['chars'], 384)
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            agent = json.loads(db.execute('SELECT record FROM analytics_agents WHERE id=?',
                (self.agent['id'],)).fetchone()[0])
        self.assertEqual(agent['name'], 'Original name')

    def test_deferred_batch_rolls_back_then_captures_each_sample_on_the_worker_db(self):
        self.start_batch_item()
        with self.runtime.analytics_db() as db:
            db.execute("CREATE TRIGGER reject_bulk BEFORE INSERT ON analytics_notifications "
                "WHEN NEW.method='item/agentMessage/delta' AND NEW.count>1 "
                "BEGIN SELECT RAISE(ABORT,'fixture batch rejection'); END")
        timeouts = []
        original = self.runtime.analytics_db
        @contextmanager
        def observed(**kwargs):
            timeouts.append(kwargs.get('busy_timeout'))
            with original(**kwargs) as db:
                yield db
        with patch.object(self.runtime, 'analytics_db', observed), \
                patch.object(StreamBuffer, 'enqueue', return_value=False):
            self.runtime.notification(self.batch_notice(self.batch_samples()))
            self.drain()
        self.assertEqual(timeouts, [50])
        self.assertEqual(self.batch_item()['stream'], {'bytes': 6, 'chars': 6, 'lines': 0, 'deltas': 2})
        self.assertEqual(self.runtime.analytics_capture_status()['failed'], 0)

    def test_deferred_batch_preserves_good_samples_and_exposes_typed_sample_failures(self):
        self.start_batch_item()
        with self.runtime.analytics_db() as db:
            db.execute("CREATE TRIGGER reject_bulk BEFORE INSERT ON analytics_notifications "
                "WHEN NEW.method='item/agentMessage/delta' AND NEW.count>1 "
                "BEGIN SELECT RAISE(ABORT,'fixture batch rejection'); END")
        original = self.runtime.analytics_event
        def observed(db, agent, method, params, **kwargs):
            if method == 'item/agentMessage/delta' and params['delta'] == 'one':
                raise ValueError('fixture-secret-do-not-retain')
            return original(db, agent, method, params, **kwargs)
        with patch.object(self.runtime, 'analytics_event', observed), \
                patch.object(StreamBuffer, 'enqueue', return_value=False):
            self.runtime.notification(self.batch_notice(self.batch_samples()))
            self.drain()
        self.assertEqual(self.batch_item()['stream'], {'bytes': 3, 'chars': 3, 'lines': 0, 'deltas': 1})
        self.assertEqual(self.runtime.analytics_capture_status()['failed'], 1)
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            errors = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()[0])
        self.assertEqual(errors['count'], 1)
        self.assertEqual(errors['last']['errorType'], 'ValueError')
        self.assertNotIn('fixture-secret-do-not-retain', json.dumps(errors))

    def test_deferred_batch_close_uses_one_bounded_attempt_under_a_locked_writer(self):
        self.start_batch_item()
        with self.writer(self.runtime.analytics_db_path), \
                patch.object(StreamBuffer, 'enqueue', return_value=False):
            self.runtime.notification(self.batch_notice(self.batch_samples()))
            until = time.monotonic() + 2
            while not self.runtime.analytics_capture_status()['active'] and time.monotonic() < until:
                time.sleep(.01)
            start = time.monotonic()
            self.runtime.close()
            self.assertLess(time.monotonic() - start, 1)
        state = self.runtime.analytics_capture_status()
        self.assertEqual(state['queued'] + state['active'], 0)
        self.assertEqual(state['failed'], 1)
        self.assertEqual(state['lastError']['errorType'], 'OperationalError')

    def test_deferred_fallback_busy_retry_never_duplicates_a_previous_sample(self):
        with self.runtime.analytics_db() as db:
            db.execute('CREATE TABLE fixture_batch_samples(id TEXT)')
        attempts = []
        def sample(db, _agent, _method, params, **_kwargs):
            db.execute('INSERT INTO fixture_batch_samples VALUES (?)', (params['delta'],))
            if params['delta'] == 'two':
                attempts.append(True)
                if len(attempts) == 1:
                    raise sqlite3.OperationalError('database is locked')
        with patch.object(self.runtime, 'analytics_agent', side_effect=ValueError('fixture batch rejection')), \
                patch.object(self.runtime, 'analytics_event', sample):
            with self.runtime.notification_db() as db:
                self.runtime.analytics_delta_batch_safe(db, self.agent, self.batch_samples(),
                    capture_sink=lambda capture: self.runtime.analytics_safe(db, capture))
                db.execute("INSERT INTO fixture_receipts VALUES ('batch-retry-once')")
            self.drain()
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            rows = db.execute('SELECT id FROM fixture_batch_samples ORDER BY rowid').fetchall()
        self.assertEqual(rows, [('one',), ('two',)])
        self.assertEqual(len(attempts), 2)
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)
        self.assertEqual(self.runtime.analytics_capture_status()['failed'], 0)

    @staticmethod
    def sample(db, number):
        db.execute('INSERT INTO fixture_captures(id) VALUES (?)', (number,))

    def test_implicit_commit_queues_in_order_and_rollback_queues_nothing(self):
        with self.assertRaisesRegex(ValueError, 'fixture rollback'):
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.analytics_safe(db, self.sample, 99)
                db.execute("INSERT INTO fixture_receipts VALUES ('rolled-back')")
                self.assertIsNone(getattr(self.runtime, '_analytics_capture_worker', None))
                raise ValueError('fixture rollback')
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 0)
        with self.runtime.lock, self.runtime.db() as db:
            for number in (12, 4, 8):
                self.runtime.analytics_safe(db, self.sample, number)
            db.execute("INSERT INTO fixture_receipts VALUES ('committed')")
            self.assertEqual(self.captures(), [])
        self.drain()
        self.assertEqual(self.captures(), [12, 4, 8])
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)

    def test_explicit_commit_survives_a_lost_response_without_replay(self):
        def callback():
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.analytics_safe(db, self.sample, 7)
                db.execute("INSERT INTO fixture_receipts VALUES ('exact-receipt')")
                db.commit()
                self.runtime.analytics_safe(db, self.sample, 9)
                db.execute("INSERT INTO fixture_receipts VALUES ('uncommitted')")
                raise ResponseTimeout('fixture lost response')
        with self.writer(self.runtime.analytics_db_path):
            worker, finished, errors = self.run_callback(callback)
            self.assertTrue(finished.wait(.5))
            self.assertEqual([type(error) for error in errors], [ResponseTimeout])
            self.assert_main_available()
            self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)
        worker.join(3)
        self.drain()
        self.assertEqual(self.captures(), [7])

    def test_explicit_rollback_discards_only_its_capture_group(self):
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.analytics_safe(db, self.sample, 1)
            db.execute("INSERT INTO fixture_receipts VALUES ('discard')")
            db.rollback()
            self.runtime.analytics_safe(db, self.sample, 2)
            db.execute("INSERT INTO fixture_receipts VALUES ('keep')")
            db.commit()
        self.drain()
        self.assertEqual(self.captures(), [2])
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)

    def stream(self):
        stream = StreamBuffer(self.runtime)
        self.runtime._stream_buffer = stream
        with patch.object(stream, '_schedule_locked'):
            stream.enqueue({'method': 'item/agentMessage/delta', 'params': {
                'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'],
                'itemId': 'exact-stream-item', 'delta': 'One committed response'}}, 'default', None)
        return stream

    def test_default_stream_flush_releases_both_locks_and_freezes_agent_fields(self):
        stream = self.stream()
        def callback():
            with self.runtime.lock, self.runtime.db() as db:
                stream.flush_locked(db, force=True)
                changed = self.runtime.agent(self.agent['id'], db)
                changed['name'] = 'Changed after stream commit'
                self.runtime.put(db, 'agents', changed)
        with self.writer(self.runtime.analytics_db_path):
            worker, finished, errors = self.run_callback(callback)
            self.assertTrue(finished.wait(.5))
            self.assertEqual(errors, [])
            self.assert_main_available()
            self.assertEqual(self.count(self.runtime.db_path, 'runtime_items'), 1)
            with stream.lock:
                self.assertTrue(all(not entry['batches'] for entry in stream.entries.values()))
        worker.join(3)
        self.drain()
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            value = json.loads(db.execute('SELECT record FROM analytics_agents WHERE id=?',
                (self.agent['id'],)).fetchone()[0])
            count = db.execute("SELECT sum(count) FROM analytics_notifications WHERE method='item/agentMessage/delta'").fetchone()[0]
        self.assertEqual(value['name'], 'Original name')
        self.assertEqual(count, 1)

    def test_failed_stream_commit_keeps_the_batch_and_queues_no_analytics(self):
        stream = self.stream()
        with self.assertRaisesRegex(sqlite3.OperationalError, 'fixture commit failure'):
            with self.runtime.lock, self.runtime.db() as db:
                with patch.object(db, 'commit', side_effect=sqlite3.OperationalError('fixture commit failure')):
                    stream.flush_locked(db, force=True)
        self.assertEqual(self.count(self.runtime.db_path, 'runtime_items'), 0)
        self.assertIsNone(getattr(self.runtime, '_analytics_capture_worker', None))
        with stream.lock:
            self.assertEqual(sum(len(entry['batches']) for entry in stream.entries.values()), 1)
        with self.runtime.lock, self.runtime.db() as db:
            stream.flush_locked(db, force=True)
        self.drain()
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            self.assertEqual(db.execute("SELECT sum(count) FROM analytics_notifications WHERE method='item/agentMessage/delta'").fetchone()[0], 1)

    def test_busy_analytics_retry_never_replays_the_committed_callback(self):
        attempts = []
        def retry(db):
            attempts.append(True)
            self.sample(db, 5)
            if len(attempts) == 1:
                raise sqlite3.OperationalError('database is locked')
        with self.runtime.notification_db() as db:
            self.runtime.analytics_safe(db, retry)
            db.execute("INSERT INTO fixture_receipts VALUES ('provider-once')")
        self.drain()
        self.assertEqual(len(attempts), 2)
        self.assertEqual(self.captures(), [5])
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)

    def test_capture_failure_preserves_later_captures_and_records_one_error(self):
        def failed(db):
            self.sample(db, 2)
            raise ValueError('fixture capture failure')
        with self.runtime.notification_db() as db:
            self.runtime.analytics_safe(db, self.sample, 1)
            self.runtime.analytics_safe(db, failed)
            self.runtime.analytics_safe(db, self.sample, 3)
            db.execute("INSERT INTO fixture_receipts VALUES ('complete')")
        self.drain()
        self.assertEqual(self.captures(), [1, 3])
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            errors = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()[0])
        self.assertEqual(errors['count'], 1)
        self.assertEqual(errors['last']['error'], 'Analytics capture failed')
        self.assertEqual(errors['last']['errorType'], 'ValueError')
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)

    def test_local_queue_failure_retains_captures_without_failing_the_receipt(self):
        with patch.object(threading.Thread, 'start',
                side_effect=RuntimeError('fixture thread start failure')):
            with self.runtime.notification_db() as db:
                self.runtime.analytics_safe(db, self.sample, 1)
                db.execute("INSERT INTO fixture_receipts VALUES ('accepted')")
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)
        self.assertEqual(len(self.runtime._analytics_capture_unscheduled), 1)
        with self.runtime.db() as db:
            self.runtime.analytics_safe(db, self.sample, 2)
        self.drain()
        self.assertEqual(self.captures(), [1, 2])
        self.assertEqual(self.runtime._analytics_capture_unscheduled, [])

    def test_repeated_thread_start_failures_keep_one_bounded_fifo(self):
        limit = self.runtime.analytics_capture_status()['limit']
        with patch.object(threading.Thread, 'start',
                side_effect=RuntimeError('fixture thread start failure')) as start:
            for number in range(1000):
                self.runtime.schedule_analytics_captures([(self.sample, (number,), {}, 64)])
        self.assertEqual(start.call_count, 1000)
        state = self.runtime.analytics_capture_status()
        self.assertEqual(state['queued'], limit)
        self.assertEqual(state['active'], 0)
        self.assertEqual(state['bytes'], limit * 64)
        self.assertEqual(state['overflow'], 1000 - limit)
        self.assertEqual(len(self.runtime._analytics_capture_unscheduled), limit)
        self.assertIsNone(getattr(self.runtime, '_analytics_capture_worker', None))
        self.assertIsNone(getattr(self.runtime, '_analytics_capture_pool', None))
        self.runtime.schedule_analytics_captures([], overflow=1)
        self.drain()
        self.assertEqual(self.captures(), list(range(limit)))
        state = self.runtime.analytics_capture_status()
        self.assertEqual(state['queued'] + state['active'], 0)
        self.assertEqual(state['bytes'], 0)
        self.assertEqual(state['completed'], limit)

    def test_close_drains_committed_captures_before_releasing_the_lease(self):
        entered = threading.Event()
        def capture(db):
            entered.set()
            self.sample(db, 1)
        with self.writer(self.runtime.analytics_db_path):
            with self.runtime.notification_db() as db:
                self.runtime.analytics_safe(db, capture)
                db.execute("INSERT INTO fixture_receipts VALUES ('before-close')")
            self.assertTrue(entered.wait(2))
            observed = []
            original = self.runtime._close_wal_keeper
            def close_keeper():
                state = self.runtime.analytics_capture_status()
                observed.append((state['queued'], state['active'], self.runtime.lease.closed))
                original()
            self.runtime._close_wal_keeper = close_keeper
            worker, finished, errors = self.run_callback(self.runtime.close)
            self.assertTrue(finished.wait(1))
            self.assert_main_available()
        self.assertTrue(finished.wait(3))
        worker.join(3)
        self.assertEqual(errors, [])
        self.assertTrue(self.runtime.lease.closed)
        self.assertEqual(self.captures(), [])
        self.assertEqual(self.runtime.analytics_capture_status()['failed'], 1)
        self.assertEqual(observed, [(0, 0, False)])

    def test_healthy_close_persists_every_committed_capture(self):
        limit = self.runtime.analytics_capture_status()['limit']
        entered, release = threading.Event(), threading.Event()
        original = self.runtime.run_analytics_capture
        def gated(operation):
            entered.set()
            self.assertTrue(release.wait(3))
            original(operation)
        self.addCleanup(release.set)
        with patch.object(self.runtime, 'run_analytics_capture', gated):
            with self.runtime.db() as db:
                for number in range(limit):
                    self.runtime.analytics_safe(db, self.sample, number)
                db.execute("INSERT INTO fixture_receipts VALUES ('healthy-close')")
            self.assertTrue(entered.wait(2))
            worker, finished, errors = self.run_callback(self.runtime.close)
            until = time.monotonic() + 2
            while not self.runtime.closed and time.monotonic() < until:
                time.sleep(.01)
            self.assertTrue(self.runtime.closed)
            release.set()
            self.assertTrue(finished.wait(3))
            worker.join(3)
        self.assertEqual(errors, [])
        self.assertEqual(self.captures(), list(range(limit)))
        state = self.runtime.analytics_capture_status()
        self.assertEqual(state['completed'], limit)
        self.assertEqual(state['failed'], 0)
        self.assertEqual(state['queued'] + state['active'], 0)
        self.assertEqual(state['bytes'], 0)
        self.assertTrue(self.runtime.lease.closed)

    def test_transaction_overflow_is_visible_only_after_a_successful_commit(self):
        limit = self.runtime.analytics_capture_status()['limit']
        with self.assertRaisesRegex(ValueError, 'fixture rollback'):
            with self.runtime.db() as db:
                for number in range(limit + 5):
                    self.runtime.analytics_safe(db, self.sample, number)
                entry = self.runtime._callback_db.after_commit_analytics[db]
                self.assertEqual(len(entry['captures']), limit)
                self.assertEqual(entry['overflow'], 5)
                self.assertEqual(self.runtime.analytics_capture_status()['failed'], 0)
                raise ValueError('fixture rollback')
        self.assertEqual(self.runtime.analytics_capture_status()['overflow'], 0)
        with self.writer(self.runtime.analytics_db_path):
            with self.runtime.db() as db:
                for number in range(limit + 5):
                    self.runtime.analytics_safe(db, self.sample, number)
                db.execute("INSERT INTO fixture_receipts VALUES ('bounded-commit')")
            state = self.runtime.analytics_capture_status()
            self.assertEqual(state['queued'] + state['active'], limit)
            self.assertEqual(state['overflow'], 5)
            self.assertEqual(state['failed'], 5)
            self.assertLessEqual(state['bytes'], state['byteLimit'])
            self.assert_main_available()
        self.drain()
        self.assertEqual(self.captures(), list(range(limit)))
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            errors = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()[0])
        self.assertEqual(errors['count'], 5)
        self.assertEqual(errors['last']['code'], 'queueOverflow')

    def test_payload_limit_exposes_a_compact_error_without_retaining_the_payload(self):
        secret = 'fixture-secret-do-not-retain'
        payload = secret + 'x' * self.runtime.analytics_capture_status()['byteLimit']
        with self.runtime.db() as db:
            self.runtime.analytics_safe(db, lambda target, value: self.sample(target, 1), payload)
            db.execute("INSERT INTO fixture_receipts VALUES ('large-capture')")
        self.drain()
        state = self.runtime.analytics_capture_status()
        self.assertEqual(state['overflow'], 1)
        self.assertEqual(state['bytes'], 0)
        self.assertEqual(state['queued'] + state['active'], 0)
        self.assertNotIn(secret, json.dumps(state))
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)
        self.assertEqual(self.captures(), [])
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            errors = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()[0])
        self.assertEqual(errors['count'], 1)
        self.assertNotIn(secret, json.dumps(errors))

    def test_full_retry_budget_and_overflow_preserve_exact_main_budget_and_cursor(self):
        import codex_runtime
        now = [0.0]
        clock = SimpleNamespace(**{name: getattr(time, name) for name in dir(time) if not name.startswith('__')})
        clock.monotonic = lambda: now[0]
        limit = self.runtime.analytics_capture_status()['limit']
        with self.writer(self.runtime.analytics_db_path), patch.object(codex_runtime, 'time', clock):
            for number in range(limit + 6):
                self.notice('thread/tokenUsage/updated', responseId='exact-response-' + str(number),
                    rawTokenUsageRecord={'response_id': 'exact-response-' + str(number)},
                    tokenUsage={'total': {'totalTokens': number + 1},
                                'last': {'totalTokens': 1, 'inputTokens': 0, 'outputTokens': 1}})
            self.runtime.commit_supervisor_event('exact-handle', {'method': 'thread/tokenUsage/updated'},
                70, 'default', None)
            state = self.runtime.analytics_capture_status()
            self.assertEqual(state['queued'] + state['active'], limit)
            self.assertEqual(state['overflow'], 6)
            self.assertLessEqual(state['bytes'], state['byteLimit'])
            self.assertEqual(self.runtime.agent(self.agent['id'])['tokensUsed'], limit + 6)
            self.assertEqual(self.count(self.runtime.db_path, 'runtime_budget_usage'), limit + 6)
            with closing(sqlite3.connect(self.runtime.db_path)) as db:
                self.assertEqual(db.execute("SELECT sequence FROM runtime_supervisor_cursor WHERE handle='exact-handle'").fetchone()[0], 70)
            now[0] = 61.0
            until = time.monotonic() + 2
            while self.runtime.analytics_capture_status()['failed'] == 6 and time.monotonic() < until:
                time.sleep(.01)
            self.assertGreater(self.runtime.analytics_capture_status()['failed'], 6)
            self.assert_main_available()
        self.drain()
        self.assertEqual(self.runtime.agent(self.agent['id'])['tokensUsed'], limit + 6)
        self.assertEqual(self.count(self.runtime.db_path, 'runtime_budget_usage'), limit + 6)

    def test_close_finishes_a_full_queue_under_a_locked_analytics_writer(self):
        limit = self.runtime.analytics_capture_status()['limit']
        with self.writer(self.runtime.analytics_db_path):
            with self.runtime.db() as db:
                for number in range(limit):
                    self.runtime.analytics_safe(db, self.sample, number)
                db.execute("INSERT INTO fixture_receipts VALUES ('close-full-queue')")
            until = time.monotonic() + 2
            while self.runtime.analytics_capture_status()['active'] == 0 and time.monotonic() < until:
                time.sleep(.01)
            start = time.monotonic()
            self.runtime.close()
            self.assertLess(time.monotonic() - start, 5)
            state = self.runtime.analytics_capture_status()
            self.assertEqual(state['queued'] + state['active'], 0)
            self.assertEqual(state['bytes'], 0)
            self.assertEqual(state['failed'], limit)
            self.assertTrue(self.runtime.lease.closed)
        self.assertEqual(self.count(self.runtime.db_path, 'fixture_receipts'), 1)

    def test_supervisor_stream_uses_the_same_fifo_as_item_start(self):
        entered, release = threading.Event(), threading.Event()
        original = self.runtime.run_analytics_capture
        def gated(operation):
            entered.set()
            self.assertTrue(release.wait(3))
            original(operation)
        self.addCleanup(release.set)
        with patch.object(self.runtime, 'run_analytics_capture', gated):
            self.notice('item/started', item={'id': 'ordered-item', 'type': 'agentMessage', 'text': ''})
            self.assertTrue(entered.wait(2))
            notice = {'method': 'item/agentMessage/delta', 'params': {
                'threadId': self.agent['threadId'], 'turnId': self.agent['turnId'],
                'itemId': 'ordered-item', 'delta': 'three'}, '_studioSupervisorSequence': 2}
            self.runtime.notification(notice)
            self.runtime.commit_supervisor_event('fixture-handle', notice, 2, 'default', None)
            self.assertEqual(self.count(self.runtime.analytics_db_path, 'analytics_items'), 0)
            with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
                self.assertEqual(db.execute("SELECT coalesce(sum(count),0) FROM analytics_notifications WHERE method='item/agentMessage/delta'").fetchone()[0], 0)
            release.set()
            self.drain()
        with closing(sqlite3.connect(self.runtime.analytics_db_path)) as db:
            item = json.loads(db.execute('SELECT record FROM analytics_items WHERE id=?',
                (':'.join((self.agent['id'], self.agent['threadId'], 'ordered-item')),)).fetchone()[0])
        self.assertEqual(item['stream']['deltas'], 1)
        self.assertEqual(item['stream']['chars'], 5)

    def test_reusable_connections_restore_their_commit_and_rollback_methods(self):
        self.runtime.__dict__.setdefault('_callback_db', threading.local()).reuse = True
        with self.runtime.db() as db:
            self.runtime.analytics_safe(db, self.sample, 3)
            db.execute("INSERT INTO fixture_receipts VALUES ('reused')")
        self.assertNotIn('commit', db.__dict__)
        self.assertNotIn('rollback', db.__dict__)
        self.assertFalse(db.in_transaction)
        with self.runtime.db() as reused:
            self.assertIs(reused, db)
            reused.commit()
        self.drain()
        self.assertEqual(self.captures(), [3])


if __name__ == '__main__':
    unittest.main()
