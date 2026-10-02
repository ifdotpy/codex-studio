#!/usr/bin/env python3
"""SQLite contention must not replay callbacks or acknowledge an uncommitted event."""
import io
import json
from contextlib import contextmanager
from pathlib import Path
import queue
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_runtime import AppServer, Runtime


def busy_error():
    error = sqlite3.OperationalError('database is locked')
    error.sqlite_errorcode = sqlite3.SQLITE_BUSY
    return error


def io_error():
    error = sqlite3.OperationalError('disk I/O error')
    error.sqlite_errorcode = sqlite3.SQLITE_IOERR
    return error


def fragment(sequence, text, item='response'):
    return {'method': 'item/agentMessage/delta',
            'params': {'threadId': 'thread', 'turnId': 'turn', 'itemId': item, 'delta': text},
            '_studioSupervisorSequence': sequence}


class ReceiptProc:
    def __init__(self, durable):
        self.handle = 'fixture:account'
        self.durable = durable
        self.cursor = 0
        self.acks = []
        self.batches = {}
        self.terminated = False

    def poll(self):
        return 0 if self.terminated else None

    def terminate(self):
        self.terminated = True

    def ack_applied_deltas(self, _lookup):
        return 0

    def register_event_batch(self, sequences):
        if any(b != a + 1 for a, b in zip(sequences, sequences[1:])):
            raise AssertionError('The fixture received a noncontiguous journal batch')
        self.batches[sequences[-1]] = tuple(sequences)

    def ack(self, sequence):
        if self.durable() < sequence:
            raise AssertionError('Supervisor ACK preceded the durable SQLite cursor')
        self.acks.append(sequence)
        self.cursor = sequence


class DispatchFixture:
    def __init__(self, notify, request, lookup, commit, durable):
        self.server = server = AppServer.__new__(AppServer)
        server.supervisor_mode = True
        server.lock = threading.RLock()
        server.callback_lock = threading.RLock()
        server.callbacks = queue.Queue(maxsize=AppServer.CALLBACK_QUEUE_LIMIT)
        server.dispatch_stopped = False
        server.reader_done = threading.Event()
        server.dispatcher_done = threading.Event()
        server.closed = False
        server.transport_error = None
        server.log = io.BytesIO()
        server.close_log_if_idle = lambda: None
        server.died = lambda: None
        server.notification = notify
        server.request = request
        server.supervisor_event_applied = lookup
        server.supervisor_commit = commit
        server.proc = ReceiptProc(durable)
        self.worker = threading.Thread(target=server.dispatch, daemon=True)

    def start(self, events, request=False):
        callback = self.server.request if request else self.server.notification
        for event in events:
            self.server.enqueue(callback, event)
        self.worker.start()

    def wait(self, condition, timeout=3):
        deadline = time.monotonic() + timeout
        while not condition():
            if time.monotonic() >= deadline:
                raise AssertionError('The expected dispatch state did not arrive')
            time.sleep(.005)

    def close(self):
        self.server.reader_done.set()
        if self.worker.ident is not None:
            self.worker.join(3)
            if self.worker.is_alive():
                raise AssertionError('The fixture dispatcher did not stop')


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.05)
            self.changed.clear()


class SupervisorPersistenceRetry(unittest.TestCase):
    def runtime_fixture(self, directory):
        runtime = QuietRuntime(Path(directory), server_factory=lambda *args: None)
        self.addCleanup(runtime.close)
        agent = runtime.create({'name': 'Worker', 'cwd': directory, 'prompt': ''},
                               draft=True, defer=True)
        with runtime.lock, runtime.db() as db:
            agent.update(threadId='thread', turnId='turn', status='running', inFlight=True,
                         autoWake=False, events=0)
            runtime.put(db, 'agents', agent)
        delivered = []

        def notify(message):
            delivered.append(message['_studioSupervisorSequence'])
            runtime.notification(message)

        def durable():
            with runtime.read_db() as db:
                row = db.execute('SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?',
                                 ('fixture:account',)).fetchone()
                return row[0] if row else 0

        fixture = DispatchFixture(notify, lambda _: None,
                                  lambda sequence: runtime.supervisor_event_applied('fixture:account', sequence),
                                  lambda message, sequence: runtime.commit_supervisor_event(
                                      'fixture:account', message, sequence, 'default', None), durable)
        self.addCleanup(fixture.close)
        return runtime, agent, fixture, delivered

    def stream_count(self, runtime, agent_id):
        with runtime.analytics_db() as db:
            return db.execute('SELECT coalesce(sum(count),0) FROM analytics_notifications '
                              'WHERE agent=? AND method=?',
                              (agent_id, 'item/agentMessage/delta')).fetchone()[0]

    def synthetic(self, *, lookup_busy=0, commit_busy=0, failure=None):
        delivered, lookups, commits = [], [], []
        state = {'cursor': 0}

        def notify(message):
            delivered.append(message['_studioSupervisorSequence'])

        def lookup(sequence):
            lookups.append(sequence)
            if len(lookups) <= lookup_busy:
                raise busy_error()
            if failure == 'lookup':
                raise io_error()
            return sequence <= state['cursor']

        def commit(message, sequence):
            commits.append((sequence, message.get('_studioSupervisorBatchCount', 1)))
            if len(commits) <= commit_busy:
                raise busy_error()
            if failure == 'commit':
                raise io_error()
            state['cursor'] = sequence

        fixture = DispatchFixture(notify, lambda _: None, lookup, commit, lambda: state['cursor'])
        self.addCleanup(fixture.close)
        fixture.start([fragment(1, 'a'), fragment(2, 'b'),
                       {'method': 'fixture/terminal', '_studioSupervisorSequence': 3}])
        fixture.wait(lambda: fixture.server.proc.cursor == 3 or fixture.server.transport_error)
        return fixture, delivered, lookups, commits

    def assert_connection_open(self, fixture):
        self.assertIsNone(fixture.server.transport_error)
        self.assertFalse(fixture.server.proc.terminated)
        self.assertFalse(fixture.server.closed)
        self.assertTrue(fixture.worker.is_alive())

    def test_busy_receipt_lookup_retries_before_the_callback(self):
        fixture, delivered, lookups, commits = self.synthetic(lookup_busy=2)
        self.assert_connection_open(fixture)
        self.assertEqual(lookups, [1, 1, 1, 3])
        self.assertEqual(delivered, [1, 2, 3])
        self.assertEqual(commits, [(2, 2), (3, 1)])
        self.assertEqual(fixture.server.proc.acks, [2, 3])

    def test_busy_batch_commit_retries_without_repeating_any_fragment(self):
        fixture, delivered, lookups, commits = self.synthetic(commit_busy=2)
        self.assert_connection_open(fixture)
        self.assertEqual(lookups, [1, 3])
        self.assertEqual(delivered, [1, 2, 3])
        self.assertEqual(commits, [(2, 2), (2, 2), (2, 2), (3, 1)])
        self.assertEqual(fixture.server.proc.acks, [2, 3])

    def test_non_busy_lookup_and_commit_errors_do_not_retry_or_ack(self):
        for failure in ('lookup', 'commit'):
            with self.subTest(failure=failure):
                fixture, delivered, lookups, commits = self.synthetic(failure=failure)
                self.assertIn('disk I/O error', fixture.server.transport_error)
                self.assertTrue(fixture.server.proc.terminated)
                self.assertEqual(lookups, [1])
                self.assertEqual(commits, [] if failure == 'lookup' else [(2, 2)])
                self.assertEqual(delivered, [] if failure == 'lookup' else [1, 2])
                self.assertEqual(fixture.server.proc.acks, [])
                fixture.close()

    def test_a_busy_native_request_callback_is_not_replayed(self):
        requests, lookups, commits = [], [], []

        def request(message):
            requests.append(message['id'])
            raise busy_error()

        fixture = DispatchFixture(lambda _: None, request,
                                  lambda sequence: lookups.append(sequence),
                                  lambda message, sequence: commits.append(sequence), lambda: 0)
        self.addCleanup(fixture.close)
        fixture.start([{'id': 'exact-request', 'method': 'item/tool/call',
                        '_studioSupervisorSequence': 1}], request=True)
        fixture.wait(lambda: fixture.server.transport_error)
        self.assertEqual(requests, ['exact-request'])
        self.assertEqual(lookups, [])
        self.assertEqual(commits, [])
        self.assertEqual(fixture.server.proc.acks, [])
        self.assertTrue(fixture.server.proc.terminated)

    def test_real_sqlite_writer_releases_between_commit_retries(self):
        with tempfile.TemporaryDirectory(prefix='studio-supervisor-busy-') as directory:
            runtime = QuietRuntime(Path(directory), server_factory=lambda *args: None)
            self.addCleanup(runtime.close)
            agent = runtime.create({'name': 'Worker', 'cwd': directory, 'prompt': ''},
                                   draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                agent.update(threadId='thread', turnId='turn', status='running', inFlight=True,
                             autoWake=False, events=0)
                runtime.put(db, 'agents', agent)
            writer = sqlite3.connect(runtime.db_path)
            writer.execute('BEGIN IMMEDIATE')
            self.addCleanup(writer.close)
            failed_commit = threading.Event()
            delivered, commits, errors = [], [], []

            def notify(message):
                delivered.append(message['_studioSupervisorSequence'])
                runtime.notification(message)

            def durable():
                with runtime.read_db() as db:
                    row = db.execute('SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?',
                                     ('fixture:account',)).fetchone()
                    return row[0] if row else 0

            def commit(message, sequence):
                commits.append(sequence)
                try:
                    runtime.commit_supervisor_event('fixture:account', message, sequence, 'default', None)
                except sqlite3.OperationalError as error:
                    errors.append(error.sqlite_errorcode)
                    failed_commit.set()
                    raise

            fixture = DispatchFixture(notify, lambda _: None,
                                      lambda sequence: runtime.supervisor_event_applied('fixture:account', sequence),
                                      commit, durable)
            self.addCleanup(fixture.close)
            fixture.start([fragment(1, 'a'), fragment(2, 'b')])
            try:
                self.assertTrue(failed_commit.wait(3), 'A held SQLite writer must trigger the real commit retry')
                self.assertEqual(delivered, [1, 2])
                self.assertEqual(fixture.server.proc.acks, [])
                self.assertEqual(durable(), 0)
                # The commit error releases Runtime.lock before the retry delay.
                acquired = runtime.lock.acquire(timeout=.2)
                self.assertTrue(acquired)
                if acquired:
                    runtime.lock.release()
                writer.commit()
                fixture.wait(lambda: fixture.server.proc.cursor == 2 or fixture.server.transport_error)
                self.assert_connection_open(fixture)
                self.assertTrue(all(code & 255 == sqlite3.SQLITE_BUSY for code in errors), errors)
                self.assertGreaterEqual(len(commits), 2)
                self.assertEqual(delivered, [1, 2])
                self.assertEqual(fixture.server.proc.acks, [2])
                self.assertEqual(durable(), 2)
                with runtime.read_db() as db:
                    row = db.execute('SELECT record FROM runtime_items WHERE id=?',
                                     (agent['id'] + ':response',)).fetchone()
                    self.assertEqual(json.loads(row[0])['text'], 'ab')
                with runtime.analytics_db() as db:
                    count = db.execute('SELECT coalesce(sum(count),0) FROM analytics_notifications '
                                       'WHERE agent=? AND method=?',
                                       (agent['id'], 'item/agentMessage/delta')).fetchone()[0]
                    self.assertEqual(count, 2, 'Failed runtime commits must not repeat analytics capture')
            finally:
                writer.rollback()
                fixture.close()
                runtime.close()

    def test_user_send_does_not_wait_for_stream_analytics_after_durable_commit(self):
        with tempfile.TemporaryDirectory(prefix='studio-stream-analytics-send-') as directory:
            runtime = QuietRuntime(Path(directory), server_factory=lambda *args: None)
            self.addCleanup(runtime.close)
            runtime._fast_delivery_enabled = False
            stream_agent = runtime.create({'name': 'Stream worker', 'cwd': directory, 'prompt': ''},
                                          draft=True, defer=True)
            send_agent = runtime.create({'name': 'Independent chat', 'cwd': directory, 'prompt': ''},
                                        draft=True, defer=True)
            with runtime.lock, runtime.db() as db:
                stream_agent.update(threadId='thread', turnId='turn', status='running',
                                    inFlight=True, autoWake=False, events=0)
                runtime.put(db, 'agents', stream_agent)
                send_agent.update(status='paused', autoWake=False)
                runtime.put(db, 'agents', send_agent)
            analytics_started, release_analytics, sent = (
                threading.Event(), threading.Event(), threading.Event())
            capture_calls, receipts, send_errors, delivered = [], [], [], []
            original_capture = runtime.analytics_event

            def blocked_capture(db, agent, method, params, **options):
                capture_calls.append(method)
                analytics_started.set()
                if not release_analytics.wait(5):
                    raise AssertionError('The test did not release the analytics capture')
                return original_capture(db, agent, method, params, **options)

            runtime.analytics_event = blocked_capture

            def notify(message):
                delivered.append(message['_studioSupervisorSequence'])
                runtime.notification(message)

            def durable():
                with runtime.read_db() as db:
                    row = db.execute('SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?',
                                     ('fixture:account',)).fetchone()
                    return row[0] if row else 0

            def send():
                try:
                    receipts.append(runtime.send(send_agent['id'], 'Send while analytics waits',
                                                 message_id='during-analytics', delivery='after_tool'))
                except Exception as error:
                    send_errors.append(error)
                finally:
                    sent.set()

            fixture = DispatchFixture(notify, lambda _: None,
                                      lambda sequence: runtime.supervisor_event_applied('fixture:account', sequence),
                                      lambda message, sequence: runtime.commit_supervisor_event(
                                          'fixture:account', message, sequence, 'default', None), durable)
            self.addCleanup(fixture.close)
            sender = threading.Thread(target=send, daemon=True)
            fixture.start([fragment(1, 'a'), fragment(2, 'b')])
            try:
                self.assertTrue(analytics_started.wait(3))
                self.assertEqual(durable(), 2, 'The stream cursor commits before analytics begins')
                self.assertEqual(fixture.server.proc.acks, [])
                sender.start()
                self.assertTrue(sent.wait(.5), 'User send must complete while stream analytics remains blocked')
                self.assertEqual(send_errors, [])
                self.assertEqual(receipts, [{'id': 'during-analytics', 'status': 'queued'}])
                self.assertFalse(release_analytics.is_set())
                with runtime.read_db() as db:
                    event = db.execute('SELECT agent,text,status FROM runtime_events WHERE id=?',
                                       ('during-analytics',)).fetchone()
                    self.assertEqual(tuple(event),
                                     (send_agent['id'], 'Send while analytics waits', 'pending'))
                release_analytics.set()
                fixture.wait(lambda: fixture.server.proc.cursor == 2 or fixture.server.transport_error)
                self.assert_connection_open(fixture)
                self.assertEqual(delivered, [1, 2])
                self.assertEqual(capture_calls, ['item/agentMessage/delta'])
                with runtime.analytics_db() as db:
                    count = db.execute('SELECT coalesce(sum(count),0) FROM analytics_notifications '
                                       'WHERE agent=? AND method=?',
                                       (stream_agent['id'], 'item/agentMessage/delta')).fetchone()[0]
                    self.assertEqual(count, 2)
            finally:
                release_analytics.set()
                if sender.ident is not None:
                    sender.join(3)
                    self.assertFalse(sender.is_alive())
                fixture.close()
                runtime.close()

    def test_real_sqlite_writer_does_not_hold_runtime_lock_during_lifecycle_wait(self):
        with tempfile.TemporaryDirectory(prefix='studio-lifecycle-writer-') as directory:
            runtime, agent, fixture, delivered = self.runtime_fixture(directory)
            writer = sqlite3.connect(runtime.db_path)
            original_db = runtime.db
            contention = threading.Event()
            busy_codes = []

            @contextmanager
            def observed_db(**options):
                try:
                    with original_db(**options) as db:
                        yield db
                except sqlite3.OperationalError as error:
                    if getattr(error, 'sqlite_errorcode', 0) & 255 == sqlite3.SQLITE_BUSY:
                        busy_codes.append(error.sqlite_errorcode)
                        contention.set()
                    raise

            runtime.db = observed_db
            writer.execute('BEGIN IMMEDIATE')
            fixture.start([{'method': 'item/started',
                            'params': {'threadId': 'thread', 'turnId': 'turn',
                                       'item': {'id': 'lifecycle-command', 'type': 'commandExecution',
                                                'command': 'fixture', 'aggregatedOutput': ''}},
                            '_studioSupervisorSequence': 1}])
            try:
                self.assertTrue(contention.wait(3), 'The lifecycle must meet the held SQLite writer')
                self.assertEqual(fixture.server.proc.acks, [])
                self.assertEqual(fixture.server.proc.durable(), 0)
                acquired = runtime.lock.acquire(timeout=.2)
                self.assertTrue(acquired, 'Lifecycle writer contention must release Runtime.lock between attempts')
                if acquired:
                    runtime.lock.release()
                self.assertFalse(fixture.server.proc.terminated)
                writer.commit()
                fixture.wait(lambda: fixture.server.proc.cursor == 1 or fixture.server.transport_error)
                self.assert_connection_open(fixture)
                self.assertTrue(busy_codes)
                self.assertEqual(delivered, [1])
                self.assertEqual(fixture.server.proc.acks, [1])
                with runtime.read_db() as db:
                    row = db.execute('SELECT record FROM runtime_tasks WHERE id=?',
                                     (agent['id'] + ':lifecycle-command',)).fetchone()
                    self.assertEqual(json.loads(row[0])['status'], 'running')
                    row = db.execute('SELECT record FROM runtime_agents WHERE id=?',
                                     (agent['id'],)).fetchone()
                    self.assertEqual(json.loads(row[0])['events'], 1)
                with runtime.analytics_db() as db:
                    count = db.execute('SELECT coalesce(sum(count),0) FROM analytics_notifications '
                                       'WHERE agent=? AND method=?', (agent['id'], 'item/started')).fetchone()[0]
                    self.assertEqual(count, 1)
            finally:
                writer.rollback()
                writer.close()
                fixture.close()
                runtime.db = original_db
                runtime.close()

    def test_outer_runtime_lock_owner_does_not_wait_for_a_sqlite_writer(self):
        with tempfile.TemporaryDirectory(prefix='studio-nested-lifecycle-writer-') as directory:
            runtime = QuietRuntime(Path(directory), server_factory=lambda *args: None)
            writer = sqlite3.connect(runtime.db_path)
            writer.execute('BEGIN IMMEDIATE')
            finished = threading.Event()
            effects, errors = [], []

            def recover():
                try:
                    with runtime.lock, runtime.notification_db():
                        effects.append('body')
                except sqlite3.OperationalError as error:
                    errors.append(error)
                finally:
                    finished.set()

            worker = threading.Thread(target=recover)
            worker.start()
            try:
                self.assertTrue(finished.wait(.5), 'An outer lock owner must fail before a long writer wait')
                self.assertEqual(effects, [])
                self.assertEqual(len(errors), 1)
                self.assertEqual(errors[0].sqlite_errorcode & 255, sqlite3.SQLITE_BUSY)
                acquired = runtime.lock.acquire(timeout=.2)
                self.assertTrue(acquired)
                if acquired:
                    runtime.lock.release()
            finally:
                writer.rollback()
                writer.close()
                worker.join(timeout=3)
                runtime.close()

    def test_analytics_context_commit_busy_retries_the_same_capture_once(self):
        with tempfile.TemporaryDirectory(prefix='studio-analytics-commit-busy-') as directory:
            runtime, agent, fixture, delivered = self.runtime_fixture(directory)
            original_db = runtime.analytics_db
            original_capture = runtime.capture_stream_analytics
            contexts, captures = [], []

            @contextmanager
            def fail_first_commit():
                contexts.append(1)
                with original_db() as db:
                    yield db
                    if len(contexts) == 1:
                        # Fail context completion before COMMIT. Its existing DB
                        # scope rolls the capture back before the retry starts.
                        raise busy_error()

            def remember_capture(operation):
                def observed(db):
                    captures.append(operation)
                    return operation(db)
                return original_capture(observed)

            runtime.analytics_db = fail_first_commit
            runtime.capture_stream_analytics = remember_capture
            fixture.start([fragment(1, 'a'), fragment(2, 'b')])
            try:
                fixture.wait(lambda: fixture.server.proc.cursor == 2 or fixture.server.transport_error)
                self.assert_connection_open(fixture)
                self.assertEqual(delivered, [1, 2])
                self.assertEqual(fixture.server.proc.acks, [2])
                self.assertEqual(len(contexts), 2)
                self.assertEqual(len(captures), 2)
                self.assertIs(captures[0], captures[1])
                runtime.analytics_db = original_db
                self.assertEqual(self.stream_count(runtime, agent['id']), 2)
                self.assertIsNone(getattr(runtime, '_stream_analytics_error', None))
            finally:
                fixture.close()
                runtime.analytics_db = original_db
                runtime.capture_stream_analytics = original_capture
                runtime.close()

    def test_lifecycle_body_busy_does_not_repeat_a_started_effect(self):
        with tempfile.TemporaryDirectory(prefix='studio-lifecycle-body-busy-') as directory:
            runtime, _agent, fixture, delivered = self.runtime_fixture(directory)
            original_item = runtime.item
            item_effects = []

            def started_effect(*args, **options):
                item_effects.append(1)
                raise busy_error()

            runtime.item = started_effect
            fixture.start([{'method': 'item/started',
                            'params': {'threadId': 'thread', 'turnId': 'turn',
                                       'item': {'id': 'started-response', 'type': 'agentMessage'}},
                            '_studioSupervisorSequence': 1}])
            try:
                fixture.wait(lambda: fixture.server.transport_error)
                self.assertIn('database is locked', fixture.server.transport_error)
                self.assertEqual(delivered, [1])
                self.assertEqual(item_effects, [1])
                self.assertEqual(fixture.server.proc.acks, [])
                self.assertEqual(fixture.server.proc.durable(), 0)
                self.assertTrue(fixture.server.proc.terminated)
            finally:
                fixture.close()
                runtime.item = original_item
                runtime.close()

    def test_permanent_analytics_commit_error_preserves_native_text_and_cursor(self):
        with tempfile.TemporaryDirectory(prefix='studio-analytics-commit-error-') as directory:
            runtime, agent, fixture, delivered = self.runtime_fixture(directory)
            original_db = runtime.analytics_db
            contexts = []

            @contextmanager
            def fail_commit():
                contexts.append(1)
                with original_db() as db:
                    yield db
                    raise io_error()

            runtime.analytics_db = fail_commit
            fixture.start([fragment(1, 'a'), fragment(2, 'b')])
            try:
                fixture.wait(lambda: fixture.server.proc.cursor == 2 or fixture.server.transport_error)
                self.assert_connection_open(fixture)
                self.assertEqual(delivered, [1, 2])
                self.assertEqual(fixture.server.proc.acks, [2])
                self.assertEqual(fixture.server.proc.durable(), 2)
                self.assertEqual(contexts, [1])
                error = runtime._stream_analytics_error
                self.assertEqual(error['handle'], 'fixture:account')
                self.assertEqual(error['sequence'], 2)
                self.assertIn('disk I/O error', error['error'])
                runtime.analytics_db = original_db
                self.assertEqual(self.stream_count(runtime, agent['id']), 0)
                with runtime.read_db() as db:
                    row = db.execute('SELECT record FROM runtime_items WHERE id=?',
                                     (agent['id'] + ':response',)).fetchone()
                    self.assertEqual(json.loads(row[0])['text'], 'ab')
            finally:
                fixture.close()
                runtime.analytics_db = original_db
                runtime.close()

    def test_reader_disconnect_stops_a_pending_persistence_retry(self):
        fixture = DispatchFixture(lambda _: None, lambda _: None,
                                  lambda _: False, lambda *_: None, lambda: 0)
        fixture.server.reader_done.set()
        attempts = []

        def blocked():
            attempts.append(1)
            raise busy_error()

        with self.assertRaises(sqlite3.OperationalError):
            fixture.server.persistence_retry(blocked)
        self.assertEqual(attempts, [1])

    def test_persistence_retry_expires_without_an_unbounded_loop(self):
        fixture = DispatchFixture(lambda _: None, lambda _: None,
                                  lambda _: False, lambda *_: None, lambda: 0)
        attempts = []

        def blocked():
            attempts.append(1)
            raise busy_error()

        with patch('codex_runtime.time.monotonic', side_effect=[0, 61]):
            with self.assertRaises(sqlite3.OperationalError):
                fixture.server.persistence_retry(blocked)
        self.assertEqual(attempts, [1])


if __name__ == '__main__':
    unittest.main()
