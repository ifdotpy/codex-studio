#!/usr/bin/env python3
"""Supervisor dispatch must not open a SQLite connection or transaction it does not need.

Each journal event commits its cursor in the transaction that applies the
event. Cursor lookups and analytics captures reuse their thread's connection.
"""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

HANG_GUARD_SECONDS = 10

import io
import queue
import sqlite3
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_runtime
import codex_sqlite
from codex_runtime import AppServer, Runtime

HANDLE = 'fixture:account'


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait()
            self.changed.clear()


class JournalProc:
    def __init__(self, changed):
        self.changed = changed
        self.acks = []
        self.terminated = False

    def poll(self):
        return 0 if self.terminated else None

    def terminate(self):
        self.terminated = True
        self.changed.set()

    def ack_applied_deltas(self, _lookup):
        return 0

    def register_event_batch(self, _sequences):
        pass

    def ack(self, sequence):
        self.acks.append(sequence)
        self.changed.set()


class Dispatcher:
    """A supervisor-mode AppServer dispatcher without a native process."""

    def __init__(self, runtime, handle):
        self.changed = threading.Event()
        self.server = server = AppServer.__new__(AppServer)
        server.supervisor_mode = True
        server.supervisor_handle = handle
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
        server.notification = lambda message: runtime.notification(message)
        server.request = lambda _message: None
        server.supervisor_event_applied = lambda sequence: runtime.supervisor_event_applied(HANDLE, sequence)
        server.supervisor_commit = lambda message, sequence: runtime.commit_supervisor_event(
            HANDLE, message, sequence, 'default', None)
        server.proc = JournalProc(self.changed)
        fail_transport = server.fail_transport

        def signal_failure(error):
            try:
                fail_transport(error)
            finally:
                self.changed.set()

        server.fail_transport = signal_failure
        self.worker = threading.Thread(target=server.dispatch, daemon=True)

    def run(self, events, done):
        for event in events:
            self.server.enqueue(self.server.notification, event)
        self.worker.start()
        deadline = time.monotonic() + HANG_GUARD_SECONDS
        while not done(self.server):
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not self.changed.wait(remaining):
                raise AssertionError('The expected dispatch state did not arrive')
            self.changed.clear()

    def close(self):
        self.server.reader_done.set()
        if self.worker.ident is not None:
            self.worker.join(HANG_GUARD_SECONDS)
            if self.worker.is_alive():
                raise AssertionError('The fixture dispatcher did not stop')


def item(sequence, item_id=None):
    return {'method': 'item/started',
            'params': {'threadId': 'thread', 'turnId': 'turn',
                       'item': {'id': item_id or f'reasoning-{sequence}', 'type': 'reasoning'}},
            '_studioSupervisorSequence': sequence}


def runtime_transactions():
    sites = codex_sqlite.diagnostics().get('transaction', {}).get('sites', {})
    return sites.get('Runtime.db', {}).get('transactions', 0)


class SupervisorWriteReduction(unittest.TestCase):
    def runtime_fixture(self):
        directory = tempfile.TemporaryDirectory(prefix='studio-supervisor-writes-')
        self.addCleanup(directory.cleanup)
        runtime = QuietRuntime(Path(directory.name), server_factory=lambda *args: None)
        self.addCleanup(runtime.close)
        agent = runtime.create({'name': 'Worker', 'cwd': directory.name, 'prompt': ''},
                               draft=True, defer=True)
        with runtime.lock, runtime.db() as db:
            agent.update(threadId='thread', turnId='turn', status='running', inFlight=True,
                         autoWake=False, events=0)
            runtime.put(db, 'agents', agent)
        return runtime, agent

    def dispatcher(self, runtime, handle=HANDLE):
        dispatcher = Dispatcher(runtime, handle)
        self.addCleanup(dispatcher.close)
        return dispatcher

    def durable(self, runtime):
        with runtime.read_db() as db:
            row = db.execute('SELECT sequence FROM runtime_supervisor_cursor WHERE handle=?',
                             (HANDLE,)).fetchone()
            return row[0] if row else 0

    def events(self, runtime, agent):
        with runtime.read_db() as db:
            return runtime.agent(agent['id'], db)['events']

    def test_each_event_commits_its_cursor_in_its_own_effect_transaction(self):
        counts = {}
        for handle in (None, HANDLE):
            with self.subTest(handle=handle):
                runtime, agent = self.runtime_fixture()
                dispatcher = self.dispatcher(runtime, handle)
                before = runtime_transactions()
                dispatcher.run([item(sequence) for sequence in range(1, 5)],
                               lambda server: server.proc.acks[-1:] == [4] or server.transport_error)
                counts[handle] = runtime_transactions() - before
                self.assertIsNone(dispatcher.server.transport_error)
                self.assertEqual(dispatcher.server.proc.acks, [1, 2, 3, 4])
                self.assertEqual(self.durable(runtime), 4)
                self.assertEqual(self.events(runtime, agent), 4)
                # Close now: the next measurement counts process-wide transactions.
                dispatcher.close()
                runtime.close()
        # Without a journal handle the dispatcher keeps the separate cursor commit.
        self.assertEqual(counts[None], 8)
        self.assertEqual(counts[HANDLE], 4)

    def test_a_failed_effect_transaction_leaves_the_cursor_and_sends_no_ack(self):
        runtime, agent = self.runtime_fixture()
        record_task = runtime.record_task

        def fail_second(db, a, method, params, stale):
            if params['item']['id'] == 'reasoning-2':
                raise RuntimeError('fixture effect failure')
            return record_task(db, a, method, params, stale)

        runtime.record_task = fail_second
        dispatcher = self.dispatcher(runtime)
        dispatcher.run([item(1), item(2)], lambda server: server.transport_error)
        self.assertEqual(dispatcher.server.proc.acks, [1])
        self.assertEqual(self.durable(runtime), 1)
        self.assertEqual(self.events(runtime, agent), 1)
        self.assertFalse(runtime.supervisor_event_applied(HANDLE, 2))

    def test_committed_effects_are_not_applied_again_after_a_later_failure(self):
        runtime, agent = self.runtime_fixture()

        def fail(_observation):
            raise RuntimeError('fixture failure after the effect commit')

        runtime._dispatch_token_rate_observation = fail
        dispatcher = self.dispatcher(runtime)
        dispatcher.run([item(1)], lambda server: server.transport_error)
        self.assertEqual(dispatcher.server.proc.acks, [])
        # The cursor committed with the effects, so a replay of event 1 is skipped.
        self.assertEqual(self.durable(runtime), 1)
        self.assertEqual(self.events(runtime, agent), 1)
        self.assertTrue(runtime.supervisor_event_applied(HANDLE, 1))

    def test_an_event_without_an_effect_transaction_commits_its_own_cursor(self):
        runtime, _agent = self.runtime_fixture()
        dispatcher = self.dispatcher(runtime)
        before = runtime_transactions()
        dispatcher.run([{'method': 'fixture/unthreaded', 'params': {}, '_studioSupervisorSequence': 1}],
                       lambda server: server.proc.acks or server.transport_error)
        self.assertIsNone(dispatcher.server.transport_error)
        self.assertEqual(dispatcher.server.proc.acks, [1])
        self.assertEqual(self.durable(runtime), 1)
        self.assertEqual(runtime_transactions() - before, 1)

    def test_cursor_lookups_reuse_one_read_connection_per_thread(self):
        runtime, _agent = self.runtime_fixture()
        canvas_uri = runtime.db_path.absolute().as_uri() + '?mode=ro'
        opened = []
        connect = sqlite3.connect

        def counted(database, *args, **kwargs):
            if database == canvas_uri and threading.current_thread().name == 'cursor-reader':
                opened.append(database)
            return connect(database, *args, **kwargs)

        results, errors = [], []

        def lookups():
            try:
                results.append(runtime.supervisor_event_applied(HANDLE, 1))
                for sequence in range(1, 4):
                    with runtime.lock, runtime.db() as db:
                        db.execute('INSERT INTO runtime_supervisor_cursor VALUES (?,?) ON CONFLICT(handle) '
                                   'DO UPDATE SET sequence=excluded.sequence', (HANDLE, sequence))
                    # Each lookup reads the latest committed cursor.
                    results.append(runtime.supervisor_event_applied(HANDLE, sequence))
                    results.append(runtime.supervisor_event_applied(HANDLE, sequence + 1))
                # This branch has no dispatcher close hook; the reader ends with its thread.
                runtime._callback_db.cursor_reader.close()
            except BaseException as error:
                errors.append(error)

        with patch.object(sqlite3, 'connect', counted):
            reader = threading.Thread(target=lookups, name='cursor-reader')
            reader.start()
            reader.join(HANG_GUARD_SECONDS)
        self.assertFalse(reader.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(results, [False, True, False, True, False, True, False])
        # One connection for all lookups on the dispatcher thread.
        self.assertEqual(len(opened), 1)

    def test_the_analytics_capture_worker_reuses_its_connection(self):
        runtime, _agent = self.runtime_fixture()
        opened = []
        sqlite_connect = codex_runtime.sqlite_connect

        def counted(database, *args, **kwargs):
            if kwargs.get('site') == 'Runtime.analytics' and threading.current_thread().name == 'studio-analytics-capture':
                opened.append(database)
            return sqlite_connect(database, *args, **kwargs)

        captured = []

        def capture(db, value):
            captured.append(db.execute('SELECT ?', (value,)).fetchone()[0])

        def failing(_db):
            raise RuntimeError('fixture capture failure')

        def run(captures):
            idle = runtime.__dict__.get('_analytics_capture_idle')
            if idle is not None:
                self.assertTrue(idle.wait(HANG_GUARD_SECONDS))
            runtime.schedule_analytics_captures(captures)
            idle = runtime.__dict__['_analytics_capture_idle']
            deadline = time.monotonic() + HANG_GUARD_SECONDS
            while time.monotonic() < deadline:
                state = runtime.analytics_capture_state()
                if idle.is_set() and not state['queued'] and not state['active']:
                    return
                idle.wait(0.05)
            raise AssertionError('The analytics captures did not finish')

        with patch.object(codex_runtime, 'sqlite_connect', counted):
            run([(capture, (index,), {}, 1) for index in range(5)])
            first = len(opened)
            run([(failing, (), {}, 1), (capture, (5,), {}, 1), (capture, (6,), {}, 1)])
        self.assertEqual(captured, list(range(7)))
        # The worker may already hold a connection from the fixture setup.
        self.assertLessEqual(first, 1)
        # A failed capture scope discards its connection; the next one opens once.
        self.assertEqual(len(opened), first + 1)


if __name__ == '__main__':
    unittest.main()
