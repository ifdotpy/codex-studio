#!/usr/bin/env python3
"""Native thread writes preserve receipts without blocking other chat input."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import importlib.util
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import PreparationPending


class Runtime(fixture.Runtime):
    def schedule(self):
        pass

    def schedule_fast_dispatch(self, *args, **kwargs):
        pass


class PreparationWriterContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-preparation-writer-')
        self.addCleanup(self.temp.cleanup)
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.agent = self.runtime.create({'name': 'Prepare', 'cwd': self.temp.name, 'prompt': ''}, draft=True, defer=True)
        self.other = self.runtime.create({'name': 'Other', 'cwd': self.temp.name, 'prompt': ''}, draft=True, defer=True)
        self.server.calls.clear()

    def test_thread_write_receipt_commits_before_io_and_unrelated_send_does_not_wait(self):
        for method in ('thread/start', 'thread/resume'):
            with self.subTest(method=method):
                with self.runtime.db() as db:
                    agent = self.runtime.agent(self.agent['id'], db)
                    agent.update(threadId='existing-thread' if method == 'thread/resume' else None)
                    self.runtime.put(db, 'agents', agent)
                self.runtime.loaded.discard(agent['id'])
                entered, release, sent = threading.Event(), threading.Event(), threading.Event()
                failures, operations = [], []
                original = self.runtime.submit_reserved
                def submit(server, requested, params, operation_id=None):
                    if requested == method:
                        with self.runtime.read_db() as db:
                            saved = self.runtime.agent(agent['id'], db)
                        operation = self.runtime.preparations[agent['id']]
                        self.assertEqual(saved['prepareAttempt'], operation['id'])
                        self.assertEqual(operation_id, 'prepare:' + agent['id'] + ':' + operation['id'])
                        operations.append(operation['id'])
                        entered.set()
                        if not release.wait(3):
                            raise RuntimeError('The fixture native write gate timed out')
                    return original(server, requested, params, operation_id)
                def prepare():
                    try:
                        self.runtime.prepare(agent)
                    except Exception as error:
                        failures.append(error)
                def send():
                    try:
                        self.runtime.send(self.other['id'], 'Other input', 'other-' + method)
                        sent.set()
                    except Exception as error:
                        failures.append(error)
                with patch.object(self.runtime, 'submit_reserved', side_effect=submit):
                    worker = threading.Thread(target=prepare)
                    worker.start()
                    try:
                        self.assertTrue(entered.wait(2))
                        sender = threading.Thread(target=send)
                        sender.start()
                        self.assertTrue(sent.wait(1), 'Another chat must enqueue before native input drains')
                        with self.runtime.read_db() as db:
                            event = db.execute('SELECT status FROM runtime_events WHERE id=?', ('other-' + method,)).fetchone()
                        self.assertEqual(event[0], 'pending')
                    finally:
                        release.set()
                        worker.join(3)
                        if 'sender' in locals():
                            sender.join(3)
                self.assertFalse(worker.is_alive())
                self.assertEqual(failures, [])
                self.assertEqual(len(operations), 1)

    def test_stop_during_native_write_rejects_late_preparation_and_never_starts_a_turn(self):
        entered, release, stopped = threading.Event(), threading.Event(), threading.Event()
        self.addCleanup(release.set)
        original = self.server.submit
        def submit(method, params):
            if method == 'thread/start':
                entered.set()
                if not release.wait(3):
                    raise RuntimeError('The fixture native write gate timed out')
            return original(method, params)
        with patch.object(self.server, 'submit', side_effect=submit):
            self.runtime.send(self.agent['id'], 'First input', 'stopped-input')
            self.runtime.dispatch(self.agent['id'])
            self.assertTrue(entered.wait(2))
            operation = self.runtime.preparations[self.agent['id']]
            stopper = threading.Thread(target=lambda: (self.runtime.stop(self.agent['id']), stopped.set()))
            stopper.start()
            try:
                self.assertTrue(stopped.wait(1), 'Stop must not wait for native thread submission')
                agent = self.runtime.agent(self.agent['id'])
                self.assertFalse(agent['autoWake'])
                self.assertEqual(agent['status'], 'paused')
                self.assertEqual(agent['epoch'], 1)
            finally:
                release.set()
                stopper.join(3)
            fixture.eventually(lambda: operation['future'].done())
            fixture.eventually(lambda: self.runtime.delivery_receipt('stopped-input')['status'] == 'cancelled')
        agent = self.runtime.agent(self.agent['id'])
        self.assertEqual(agent['status'], 'paused')
        self.assertFalse(agent['autoWake'])
        self.assertIsNone(agent['threadId'])
        self.assertNotIn(agent['id'], self.runtime.loaded)
        self.assertFalse(any(method == 'turn/start' for method, _ in self.server.calls))
        self.assertEqual(sum(method == 'thread/start' for method, _ in self.server.calls), 1)

    def test_response_timeout_reuses_the_exact_future_without_thread_start_replay(self):
        self.runtime.preparation_wait_seconds = .01
        native = concurrent.futures.Future()
        requests = []
        original = self.server.submit
        def submit(method, params):
            if method != 'thread/start':
                return original(method, params)
            requests.append((method, params))
            return native
        with patch.object(self.server, 'submit', side_effect=submit):
            waits = []
            for _ in range(2):
                with self.assertRaises(PreparationPending) as pending:
                    self.runtime.prepare(self.agent)
                waits.append(pending.exception.future)
            self.assertIs(waits[0], waits[1])
            self.assertEqual(len(requests), 1)
            native.set_result({'thread': {'id': 'late-native-thread'}})
            fixture.eventually(lambda: waits[0].done())
            self.assertEqual(waits[0].result()['threadId'], 'late-native-thread')
            self.assertEqual(self.runtime.prepare(self.agent)['threadId'], 'late-native-thread')
            self.assertEqual(len(requests), 1)
        self.assertFalse(any(method == 'turn/start' for method, _ in self.server.calls))

    def test_unknown_write_timeout_holds_the_exact_attempt_without_replay(self):
        requests = []
        original = self.server.submit
        def submit(method, params):
            if method != 'thread/start':
                return original(method, params)
            requests.append((method, params))
            raise OSError('Native write response was lost')
        with patch.object(self.server, 'submit', side_effect=submit):
            waits = []
            for _ in range(2):
                with self.assertRaises(PreparationPending) as pending:
                    self.runtime.prepare(self.agent)
                waits.append(pending.exception.future)
                self.runtime.preparation_wait_seconds = .01
            self.assertIs(waits[0], waits[1])
            self.assertEqual(len(requests), 1)
            self.assertFalse(waits[0].done())
            self.assertEqual(self.runtime.agent(self.agent['id'])['prepareAttempt'],
                             self.runtime.preparations[self.agent['id']]['id'])
        self.assertIsNone(self.runtime.agent(self.agent['id'])['threadId'])
        self.assertFalse(any(method == 'turn/start' for method, _ in self.server.calls))


if __name__ == '__main__':
    unittest.main()
