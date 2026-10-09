#!/usr/bin/env python3
"""Monitor input waits preserve receipts without holding the runtime lock."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import importlib.util
import json
from pathlib import Path
import queue
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('runtime_fixture', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_runtime import ResponseTimeout, SubmissionRejected
from codex_native_errors import NativeRpcError
from codex_native_tools import refresh_account


class QuietRuntime(fixture.Runtime):
    def schedule(self):
        pass

    def schedule_fast_dispatch(self, *args, **kwargs):
        pass


class InputServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.pending = {}
        self.callbacks, self.clock_replies = queue.Queue(), queue.Queue()
        self.proc = self
        self.gates = {}
        self.deferred = set()
        self.inputs = []
        self.submission_error = None
        self.response_error = None
        self.native_gate = None

    def poll(self):
        return None

    def call(self, method, params, timeout=60):
        if method == 'thread/loaded/list':
            if self.native_gate:
                entered, release = self.native_gate
                entered.set()
                if not release.wait(3):
                    raise RuntimeError('The fixture native check timed out')
            return {'data': []}
        if method == 'command/exec/terminate':
            return {}
        return super().call(method, params, timeout)

    def submit(self, method, params):
        if method not in {'command/exec/write', 'command/exec/resize'}:
            return super().submit(method, params)
        self.calls.append((method, params))
        future = concurrent.futures.Future()
        self.inputs.append((method, params, future))
        gate = self.gates.get(params['processId'])
        if gate:
            entered, release = gate
            entered.set()
            if not release.wait(3):
                raise RuntimeError('The fixture native writer timed out')
        if self.submission_error:
            raise self.submission_error
        if self.response_error:
            future.set_exception(self.response_error)
        elif params['processId'] not in self.deferred:
            future.set_result({})
        return future

    def wait(self, submitted, timeout=60):
        if not submitted.done():
            raise ResponseTimeout('Monitor input response timed out; outcome unknown')
        return submitted.result()


class MonitorInputLockContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-monitor-input-')
        self.runtime = QuietRuntime(Path(self.temp.name), InputServer)
        self.addCleanup(self.temp.cleanup)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.agent = self.new_agent('Monitor owner')
        self.other = self.new_agent('Other chat')
        self.monitor = self.new_monitor(self.agent, 'monitor-one')
        self.threads = []

    def new_agent(self, name):
        agent = self.runtime.create({'name': name, 'cwd': self.temp.name, 'prompt': ''}, draft=True, defer=True)
        with self.runtime.db() as db:
            agent = self.runtime.agent(agent['id'], db)
            agent.update(autoWake=True, status='waiting')
            self.runtime.put(db, 'agents', agent)
        return agent

    def new_monitor(self, agent, key):
        with self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {'id': key, 'agent': agent['id'], 'epoch': agent['epoch'],
                'status': 'running', 'interactive': True, 'command': 'fixture-command', 'created': 1})
        return key

    def record(self, key=None):
        with self.runtime.read_db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?',
                                        (key or self.monitor,)).fetchone()[0])

    def worker(self, action):
        done, errors = threading.Event(), []
        def run():
            try:
                action()
            except Exception as error:
                errors.append(error)
            finally:
                done.set()
        thread = threading.Thread(target=run, daemon=True)
        thread.start()
        self.threads.append(thread)
        return done, errors

    def gate(self, key=None):
        entered, release = threading.Event(), threading.Event()
        self.server.gates[key or self.monitor] = entered, release
        self.addCleanup(release.set)
        return entered, release

    def join(self):
        for thread in self.threads:
            thread.join(3)
            self.assertFalse(thread.is_alive())

    def test_write_and_resize_do_not_block_another_chat_or_stop(self):
        for payload in ({'text': 'Exact bytes'}, {'rows': 30, 'cols': 90}):
            with self.subTest(payload=payload):
                self.agent = self.new_agent('Gated monitor owner')
                key = self.new_monitor(self.agent, 'monitor-' + str(len(self.server.inputs)))
                entered, release = self.gate(key)
                done, errors = self.worker(lambda: self.runtime.monitor_input(key, payload))
                self.assertTrue(entered.wait(2))
                send_done, send_errors = self.worker(lambda: self.runtime.send(self.other['id'], 'Other input', key + '-send'))
                stop_done, stop_errors = self.worker(lambda: self.runtime.stop(self.agent['id']))
                try:
                    self.assertTrue(send_done.wait(1), 'Another chat must not wait for monitor input I/O')
                    self.assertTrue(stop_done.wait(1), 'Stop must not wait for monitor input I/O')
                    reserved = self.record(key)['inputAttempt']
                    self.assertEqual(reserved['params']['processId'], key)
                    self.assertEqual(reserved['status'], 'submitted')
                finally:
                    release.set()
                    self.join()
                self.assertTrue(done.is_set())
                self.assertEqual(errors + send_errors + stop_errors, [])
                self.assertEqual(self.record(key)['inputAttempt']['id'], reserved['id'])

    def test_two_monitors_have_independent_input_guards(self):
        second = self.new_monitor(self.other, 'monitor-two')
        entered, release = self.gate()
        done, errors = self.worker(lambda: self.runtime.monitor_input(self.monitor, {'text': 'First'}))
        self.assertTrue(entered.wait(2))
        next_done, next_errors = self.worker(lambda: self.runtime.monitor_input(second, {'text': 'Second'}))
        try:
            self.assertTrue(next_done.wait(1), 'A monitor guard must not block another monitor')
        finally:
            release.set()
            self.join()
        self.assertTrue(done.is_set())
        self.assertEqual(errors + next_errors, [])
        self.assertEqual(len(self.server.inputs), 2)

    def test_connect_rechecks_owner_account_epoch_cancel_and_connection(self):
        for change in ('owner', 'account', 'epoch', 'cancel', 'connection', 'stdin'):
            with self.subTest(change=change):
                key = self.new_monitor(self.agent, 'changed-' + change)
                original = self.runtime.connect
                def connect(account):
                    server = original(account)
                    with self.runtime.lock, self.runtime.db() as db:
                        monitor = self.record(key)
                        agent = self.runtime.agent(self.agent['id'], db)
                        if change == 'owner':
                            monitor['agent'] = self.other['id']
                        elif change == 'account':
                            agent['accountKey'] = 'changed-account'
                        elif change == 'epoch':
                            agent['epoch'] += 1
                        elif change == 'cancel':
                            monitor['cancelRequested'] = True
                        elif change == 'connection':
                            self.runtime.offline_accounts.add(account)
                        else:
                            monitor['stdinClosed'] = True
                        self.runtime.put(db, 'agents', agent)
                        self.runtime.put(db, 'monitors', monitor)
                    return server
                before = len(self.server.inputs)
                with patch.object(self.runtime, 'connect', side_effect=connect), self.assertRaises(ValueError):
                    self.runtime.monitor_input(key, {'text': 'Must not be submitted'}, owner=self.agent['id'], epoch=self.agent['epoch'])
                self.assertEqual(len(self.server.inputs), before)
                self.runtime.offline_accounts.clear()
                self.agent = self.new_agent('Next owner')

    def test_unknown_write_and_resize_hold_exact_receipt_without_replay(self):
        for payload in ({'text': 'Exact bytes'}, {'rows': 30, 'cols': 90}, {'closeStdin': True}):
            with self.subTest(payload=payload):
                key = self.new_monitor(self.agent, 'unknown-' + str(len(self.server.inputs)))
                self.server.deferred.add(key)
                operations = []
                original = self.runtime.submit_reserved
                def submit(server, method, params, operation_id=None):
                    saved = self.record(key)['inputAttempt']
                    self.assertEqual(operation_id, 'monitor-input:' + key + ':' + saved['id'])
                    operations.append(operation_id)
                    return original(server, method, params, operation_id)
                with patch.object(self.runtime, 'submit_reserved', side_effect=submit):
                    with self.assertRaises(ResponseTimeout):
                        self.runtime.monitor_input(key, payload)
                    receipt = self.record(key)['inputAttempt']
                    self.assertEqual(receipt['status'], 'uncertain')
                    with self.assertRaises(ValueError):
                        self.runtime.monitor_input(key, payload)
                    self.assertEqual(operations, ['monitor-input:' + key + ':' + receipt['id']])
                self.server.inputs[-1][2].set_result({})
                fixture.eventually(lambda: 'inputAttempt' not in self.record(key))
                self.assertEqual(self.record(key)['lastInputAttempt']['id'], receipt['id'])
                if payload.get('closeStdin'):
                    self.assertTrue(self.record(key)['stdinClosed'])

    def test_definite_submission_and_rpc_rejections_release_only_the_exact_attempt(self):
        for error in (SubmissionRejected('No native bytes were submitted'),
                      NativeRpcError({'code': -32600, 'message': 'EOF rejected'})):
            with self.subTest(error=type(error).__name__):
                key = self.new_monitor(self.agent, 'rejected-' + type(error).__name__)
                if isinstance(error, SubmissionRejected):
                    self.server.submission_error = error
                else:
                    self.server.response_error = error
                with self.assertRaises(type(error)):
                    self.runtime.monitor_input(key, {'closeStdin': True})
                record = self.record(key)
                self.assertNotIn('inputAttempt', record)
                self.assertNotIn('stdinCloseRequested', record)
                self.assertFalse(record.get('stdinClosed', False))
                self.assertEqual(record['lastInputAttempt']['status'], 'failed')
                self.assertEqual(record['lastInputAttempt']['error'], str(error))
                self.server.submission_error = self.server.response_error = None
                self.runtime.monitor_input(key, {'text': 'Still open'})
                self.assertNotEqual(self.record(key)['lastInputAttempt']['id'], record['lastInputAttempt']['id'])

    def test_unknown_submission_error_cannot_replay_plain_input(self):
        self.server.submission_error = OSError('The native write result was lost')
        with self.assertRaisesRegex(RuntimeError, 'outcome unknown'):
            self.runtime.monitor_input(self.monitor, {'text': 'Exact uncertain bytes'})
        before = self.record()['inputAttempt']
        self.assertEqual(before['status'], 'uncertain')
        self.server.submission_error = None
        with self.assertRaises(ValueError):
            self.runtime.monitor_input(self.monitor, {'text': 'Exact uncertain bytes'})
        self.assertEqual(self.record()['inputAttempt'], before)
        self.assertEqual(len(self.server.inputs), 1)

    def test_unlabelled_transport_failure_preserves_receipt_on_a_healthy_connection(self):
        self.server.response_error = RuntimeError('Supervisor event or reattach barrier was not durably applied')
        with self.assertRaisesRegex(RuntimeError, 'reattach barrier'):
            self.runtime.monitor_input(self.monitor, {'text': 'Exact uncertain input'})
        before = self.record()['inputAttempt']
        self.assertEqual(before['status'], 'uncertain')
        self.assertTrue(self.runtime.connection_current('default', before['connectionId']))
        self.assertIs(self.runtime.servers['default'], self.server)
        self.server.response_error = None
        with self.assertRaisesRegex(ValueError, 'outcome is unknown'):
            self.runtime.monitor_input(self.monitor, {'text': 'Exact uncertain input'})
        self.assertEqual(self.record()['inputAttempt'], before)
        self.assertEqual(len(self.server.inputs), 1)

    def test_stop_before_reservation_submits_no_native_input(self):
        original = self.runtime.connect
        def connect(account):
            server = original(account)
            self.runtime.stop(self.agent['id'])
            return server
        with patch.object(self.runtime, 'connect', side_effect=connect), self.assertRaises(ValueError):
            self.runtime.monitor_input(self.monitor, {'text': 'Must not be submitted'})
        self.assertEqual(self.server.inputs, [])
        self.assertNotIn('inputAttempt', self.record())

    def test_known_reply_releases_input_before_its_queued_callback(self):
        callbacks = []
        with patch.object(self.server, 'on_result', side_effect=lambda future, callback: callbacks.append((future, callback))):
            self.runtime.monitor_input(self.monitor, {'text': 'First'})
            first = self.record()['lastInputAttempt']['id']
            self.assertNotIn('inputAttempt', self.record())
            self.runtime.monitor_input(self.monitor, {'text': 'Second'})
            second = self.record()['lastInputAttempt']['id']
            self.assertNotEqual(first, second)
        for future, callback in callbacks:
            callback(future)
        self.assertEqual(self.record()['lastInputAttempt']['id'], second)

    def test_late_ack_does_not_adopt_stopped_replaced_or_cancelled_monitor(self):
        for change in ('stop', 'owner', 'connection', 'cancel'):
            with self.subTest(change=change):
                key = self.new_monitor(self.agent, 'late-' + change)
                self.server.deferred.add(key)
                with self.assertRaises(ResponseTimeout):
                    self.runtime.monitor_input(key, {'closeStdin': True})
                if change == 'stop':
                    self.runtime.stop(self.agent['id'])
                elif change == 'cancel':
                    self.runtime.cancel_monitor(key)
                else:
                    with self.runtime.lock, self.runtime.db() as db:
                        monitor = self.record(key)
                        if change == 'owner':
                            monitor['agent'] = self.other['id']
                            self.runtime.put(db, 'monitors', monitor)
                        else:
                            self.runtime.connection_ids['default'] = 'replacement-connection'
                before = self.record(key)
                self.server.inputs[-1][2].set_result({})
                self.assertEqual(self.record(key), before)
                self.agent = self.new_agent('Next late owner')

    def test_same_monitor_serializes_text_and_eof(self):
        entered, release = self.gate()
        done, errors = self.worker(lambda: self.runtime.monitor_input(self.monitor, {'text': 'First'}))
        self.assertTrue(entered.wait(2))
        next_done, next_errors = self.worker(lambda: self.runtime.monitor_input(self.monitor, {'closeStdin': True}))
        try:
            self.assertFalse(next_done.wait(.1))
            self.assertEqual(len(self.server.inputs), 1)
        finally:
            release.set()
            self.join()
        self.assertEqual(errors + next_errors, [])
        self.assertTrue(done.is_set() and next_done.is_set())
        self.assertEqual([row[1]['closeStdin'] for row in self.server.inputs], [False, True])
        self.assertTrue(self.record()['stdinClosed'])

    def test_idle_codex_refresh_and_other_account_claude_input_complete(self):
        with self.runtime.db() as db:
            agent = self.runtime.agent(self.agent['id'], db)
            agent.update(accountKey='claude-other', provider='claude')
            self.runtime.put(db, 'agents', agent)
        self.runtime.accounts = type('Accounts', (), {
            'get': lambda _, key: {'provider': 'claude' if key == 'claude-other' else 'codex', 'status': 'ready'},
            'home': lambda _, key: self.temp.name,
        })()
        self.runtime.connect('claude-other')
        entered, release = threading.Event(), threading.Event()
        self.server.native_gate = entered, release
        refresh_done, refresh_errors = self.worker(lambda: refresh_account(self.runtime, 'default'))
        self.assertTrue(entered.wait(2))
        connect_entered = threading.Event()
        original = self.runtime.connect
        def connect(account):
            connect_entered.set()
            self.assertFalse(self.runtime.lock._is_owned(), 'Connect must run outside Runtime.lock')
            return original(account)
        with patch.object(self.runtime, 'connect', side_effect=connect):
            input_done, input_errors = self.worker(lambda: self.runtime.monitor_input(self.monitor, {'text': 'Claude input'}))
            try:
                self.assertTrue(connect_entered.wait(2))
            finally:
                release.set()
                self.join()
        self.assertTrue(refresh_done.is_set() and input_done.is_set())
        self.assertEqual(refresh_errors + input_errors, [])
        self.assertEqual(len(self.runtime.servers['claude-other'].inputs), 1)


if __name__ == '__main__':
    unittest.main()
