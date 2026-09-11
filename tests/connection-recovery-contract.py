#!/usr/bin/env python3
"""Disconnected turn reconciliation uses native reads without work replay."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest
import urllib.error
import urllib.request

spec = importlib.util.spec_from_file_location('turn_fixture', Path(__file__).with_name('turn-recovery-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_canvas import Canvas, make_server
from codex_connection_recovery import recover


class Runtime(fixture.Runtime):
    def schedule(self):
        pass


class ConnectionRecoveryContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-connection-recovery-')
        self.addCleanup(self.temp.cleanup)
        self.runtime = Runtime(Path(self.temp.name), fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        lead = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': ''}, draft=True, defer=True)
        self.key = lead['id']
        self.disconnect(self.key)
        self.a = self.runtime.agent(self.key)
        self.server.native = {
            'id': 'native-thread', 'status': {'type': 'idle'},
            'turns': [{'id': 'lost-turn', 'status': 'completed', 'items': [
                {'id': 'final-answer', 'type': 'agentMessage', 'text': 'Full final answer', 'phase': 'final_answer'},
            ]}],
        }
        self.server.calls.clear()

    def disconnect(self, key, **extra):
        self.update(key, threadId='native-thread', turnId='lost-turn', status='interrupted',
                    autoWake=False, inFlight=False, startAttempt=None,
                    error='Codex disconnected. Review the transcript before resuming.', **extra)

    def update(self, key=None, **values):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(key or self.key, db)
            a.update(values)
            self.runtime.put(db, 'agents', a)
        return a

    def stored_item(self, suffix):
        with self.runtime.db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?',
                                        (self.key + ':' + suffix,)).fetchone()[0])

    def read_calls_only(self):
        self.assertTrue(self.server.calls)
        self.assertTrue(all(method in {'thread/read', 'thread/turns/list'} for method, _ in self.server.calls), self.server.calls)
        for method, params in self.server.calls:
            self.assertEqual(params['threadId'], 'native-thread')
            if method == 'thread/read':
                self.assertFalse(params['includeTurns'])

    def only_connection_check_changed(self, previous):
        a = self.runtime.agent(self.key)
        receipt = a.pop('connectionCheck')
        self.assertEqual(a, previous)
        for field in ('epoch', 'accountKey', 'threadId', 'turnId'):
            self.assertEqual(receipt[field], previous[field])
        self.assertEqual(receipt['previousError'], previous['error'])
        self.assertGreater(receipt['at'], 0)
        self.assertIn(receipt['nativeState'], {'active', 'idle', 'notLoaded'})
        return receipt

    def test_completed_restores_text_retains_error_receipt_and_repeated_call_skips(self):
        with self.runtime.db() as db:
            self.runtime.item(db, self.key, 'final-answer', 'assistant', 'Partial',
                              turnId='lost-turn', streaming=True)
        self.runtime.loaded.add(self.key)
        result = recover(self.runtime, self.key)
        self.assertEqual(result, {'status': 'reconciled', 'turnId': 'lost-turn', 'outcome': 'completed'})
        a = self.runtime.agent(self.key)
        self.assertEqual(a['lastAnswer'], 'Full final answer')
        self.assertEqual(a['status'], 'completed')
        self.assertIsNone(a['error'])
        self.assertIsNone(a['turnId'])
        self.assertFalse(a['autoWake'])
        self.assertFalse(a['inFlight'])
        self.assertNotIn(self.key, self.runtime.loaded)
        self.assertEqual(a['connectionRecovery']['previousError'], self.a['error'])
        item = self.stored_item('final-answer')
        self.assertEqual(item['text'], 'Full final answer')
        self.assertEqual(item['turnStatus'], 'completed')
        self.assertFalse(item['streaming'])
        notice = self.stored_item('connection-recovery:lost-turn')
        self.assertEqual(notice['previousError'], self.a['error'])
        self.assertEqual(notice['details'], self.a['error'])
        self.read_calls_only()
        calls = list(self.server.calls)
        self.assertEqual(recover(self.runtime, self.key), {'status': 'superseded'})
        self.assertEqual(self.server.calls, calls)
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0], 1)

    def test_failed_retains_native_error_and_failure_hold(self):
        error = {'message': 'Quota exceeded', 'codexErrorInfo': 'usageLimitExceeded'}
        self.server.native['turns'][0].update(status='failed', error=error)
        self.assertEqual(recover(self.runtime, self.key)['outcome'], 'failed')
        a = self.runtime.agent(self.key)
        self.assertEqual(a['error'], error)
        self.assertTrue(a['nativeFailureHold'])
        self.assertFalse(a['autoWake'])
        self.read_calls_only()

    def test_interrupted_unloaded_turn_is_confirmed_without_resume(self):
        self.update(error='Server restarted during a turn. Review history, then send a new instruction.')
        self.server.native['status']['type'] = 'notLoaded'
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.assertEqual(recover(self.runtime, self.key)['outcome'], 'interrupted')
        self.assertEqual(self.runtime.agent(self.key)['status'], 'interrupted')
        self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        self.read_calls_only()

    def test_worker_recovery_preserves_parent_queue_tasks_requests_and_tools(self):
        self.update(autoWake=True, status='waiting', error=None,
                    threadId='parent-thread', turnId=None)
        worker = self.runtime.create({'name': 'Worker', 'prompt': 'Review the result', 'role': 'reviewer'}, parent=self.key, defer=True)
        parent = self.runtime.agent(self.key)
        self.key = worker['id']
        self.disconnect(self.key)
        with self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            self.runtime.enqueue(db, a, 'user', 'Do not replay', 'queued-input')
            db.execute("UPDATE runtime_events SET status='pending' WHERE id='queued-input'")
            self.runtime.put(db, 'tasks', {'id': 'task', 'agent': self.key, 'status': 'running', 'processId': 'command'})
            self.runtime.put(db, 'monitors', {'id': 'monitor', 'agent': self.key, 'status': 'running'})
            self.runtime.put(db, 'requests', {'id': 'question', 'agent': self.key, 'status': 'pending', 'method': 'agent/asyncQuestion'})
            self.runtime.put(db, 'tool_requests', {'id': 'uncertain-tool', 'agent': self.key, 'stage': 'interrupted', 'outcome': 'unknown'})
        self.server.native['turns'][0]['items'].extend([
            {'id': 'tool', 'type': 'commandExecution', 'command': 'must not run', 'status': 'completed'},
            {'id': 'question-output', 'type': 'agentMessage', 'text': 'Question',
             'questions': [{'title': 'Do not create a user request'}], 'phase': 'commentary'},
        ])
        tables = ('runtime_events', 'runtime_event_meta', 'runtime_tasks', 'runtime_monitors',
                  'runtime_requests', 'runtime_tool_requests', 'runtime_checkpoints')
        def stored():
            with self.runtime.db() as db:
                return {table: [tuple(row) for row in db.execute(f'SELECT * FROM {table} ORDER BY id')]
                        for table in tables if db.execute('SELECT 1 FROM sqlite_master WHERE name=?', (table,)).fetchone()}
        before = stored()
        self.server.calls.clear()
        self.assertEqual(recover(self.runtime, self.key)['status'], 'reconciled')
        self.assertEqual(stored(), before)
        self.assertEqual(self.runtime.agent(parent['id']), parent)
        self.assertEqual(self.runtime.agent(self.key)['lastAnswer'], 'Full final answer')
        self.read_calls_only()

    def test_read_failure_never_changes_agent(self):
        self.server.read_error = TimeoutError('Read timeout')
        result = recover(self.runtime, self.key)
        self.assertEqual(result['status'], 'unconfirmed')
        self.assertIn('Read timeout', result['error'])
        self.assertEqual(self.runtime.agent(self.key), self.a)
        self.read_calls_only()

    def test_active_missing_unknown_and_wrong_native_identity_are_unconfirmed(self):
        initial = copy.deepcopy(self.server.native)
        cases = [
            {'status': {'type': 'active'}},
            {'id': 'different-thread'},
            {'turns': []},
            {'turns': [{'id': 'different-turn', 'status': 'completed'}]},
            {'turns': [{'id': 'lost-turn', 'status': 'inProgress'}]},
            {'turns': [{'id': 'lost-turn', 'status': 'future-state'}]},
        ]
        for case in cases:
            with self.subTest(case=case):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                self.server.native = {**copy.deepcopy(initial), **case}
                result = recover(self.runtime, self.key)
                self.assertEqual(result['status'], 'unconfirmed')
                if case.get('id') == 'different-thread':
                    self.assertEqual(self.runtime.agent(self.key), self.a)
                    self.assertFalse(result.get('checked'))
                else:
                    self.assertTrue(result['checked'])
                    self.only_connection_check_changed(self.a)
        self.read_calls_only()

    def test_native_becomes_active_or_changes_identity_after_turn_read(self):
        original = self.server.call
        for change in ({'status': {'type': 'active'}}, {'id': 'other-thread'}):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                self.server.native.update(id='native-thread', status={'type': 'idle'})
                def call(method, params, timeout=60):
                    result = original(method, params, timeout)
                    if method == 'thread/turns/list':
                        self.server.native.update(change)
                    return result
                self.server.call = call
                result = recover(self.runtime, self.key)
                self.assertEqual(result['status'], 'unconfirmed')
                if change.get('id'):
                    self.assertEqual(self.runtime.agent(self.key), self.a)
                    self.assertFalse(result.get('checked'))
                else:
                    self.assertTrue(result['checked'])
                    self.only_connection_check_changed(self.a)

    def test_unconfirmed_read_receipt_cannot_overwrite_a_new_agent_state(self):
        original = self.server.call
        self.server.native['status']['type'] = 'active'
        for change in ({'epoch': 3}, {'turnId': 'new-turn'}, {'accountTransferId': 'transfer'},
                       {'deletedAt': 1}, {'accountKey': 'another-account'}):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                def call(method, params, timeout=60):
                    result = original(method, params, timeout)
                    if method == 'thread/read':
                        self.update(**change)
                    return result
                self.server.call = call
                self.assertEqual(recover(self.runtime, self.key)['status'], 'superseded')
                self.assertEqual(self.runtime.agent(self.key), {**self.a, **change})
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.a)
        connection = self.runtime.connection_ids['default']
        def replaced(method, params, timeout=60):
            result = original(method, params, timeout)
            self.runtime.connection_ids['default'] = 'replacement'
            return result
        self.server.call = replaced
        self.assertEqual(recover(self.runtime, self.key)['status'], 'superseded')
        self.assertEqual(self.runtime.agent(self.key), self.a)
        self.runtime.connection_ids['default'] = connection
        self.read_calls_only()

    def test_second_read_failure_has_no_positive_connection_receipt(self):
        original = self.server.call
        reads = 0
        def call(method, params, timeout=60):
            nonlocal reads
            if method == 'thread/read':
                reads += 1
                if reads == 2:
                    raise TimeoutError('Connection lost before confirmation')
            return original(method, params, timeout)
        self.server.call = call
        result = recover(self.runtime, self.key)
        self.assertEqual(result['status'], 'unconfirmed')
        self.assertFalse(result.get('checked'))
        self.assertEqual(self.runtime.agent(self.key), self.a)

    def test_server_replacement_with_same_connection_id_invalidates_old_reads(self):
        original_connect = self.runtime.connect
        original_call = self.server.call
        connection = self.runtime.connection_ids['default']
        for stage in ('connect', 'read', 'apply'):
            with self.subTest(stage=stage):
                self.server.calls.clear()
                self.server.native['status']['type'] = 'active' if stage == 'read' else 'idle'
                def replace():
                    with self.runtime.lock:
                        self.runtime.servers['default'] = object()
                def connect(account='default'):
                    server = original_connect(account)
                    replace()
                    return server
                def call(method, params, timeout=60):
                    result = original_call(method, params, timeout)
                    replace()
                    return result
                self.runtime.connect = connect if stage == 'connect' else original_connect
                self.server.call = call if stage == 'read' else original_call
                self.server.before_apply = replace if stage == 'apply' else None
                try:
                    self.assertEqual(recover(self.runtime, self.key), {'status': 'superseded'})
                    self.assertEqual(self.runtime.connection_ids['default'], connection)
                    self.assertEqual(self.runtime.agent(self.key), self.a)
                    if stage == 'connect':
                        self.assertEqual(self.server.calls, [])
                    else:
                        self.read_calls_only()
                    with self.runtime.db() as db:
                        self.assertEqual(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0], 0)
                finally:
                    self.runtime.servers['default'] = self.server
                    self.runtime.connect = original_connect
                    self.server.call = original_call
                    self.server.before_apply = None

    def test_stop_new_turn_transfer_connection_and_delete_win_callback_races(self):
        initial_connection = self.runtime.connection_ids['default']
        for change in (
            {'epoch': 999, 'status': 'paused', 'error': 'Stopped by user'},
            {'turnId': 'new-turn', 'status': 'running', 'inFlight': True, 'autoWake': True},
            {'accountTransferId': 'transfer'}, {'accountKey': 'another-account'},
            {'workspaceOperation': 'restore'}, {'deletedAt': 1},
        ):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                self.server.before_apply = lambda: self.update(**change)
                self.assertEqual(recover(self.runtime, self.key)['status'], 'superseded')
                self.assertEqual(self.runtime.agent(self.key), {**self.a, **change})
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.a)
        self.server.before_apply = lambda: self.runtime.connection_ids.update(default='replacement')
        self.assertEqual(recover(self.runtime, self.key)['status'], 'superseded')
        self.assertEqual(self.runtime.agent(self.key), self.a)
        self.runtime.connection_ids['default'] = initial_connection
        self.read_calls_only()

    def test_explicit_stop_policy_block_and_unsubmitted_turn_are_ineligible(self):
        for change in (
            {'status': 'paused', 'error': 'Stopped by user'},
            {'error': 'An unrelated failure'}, {'autoWake': True}, {'inFlight': True},
            {'startAttempt': {'id': 'pending'}}, {'turnId': None},
            {'accountTransferId': 'transfer'}, {'workspaceOperation': 'restore'},
            {'nativeThreadBlock': {'threadId': 'native-thread', 'error': {'codexErrorInfo': 'misalignmentPolicyViolation'}}},
        ):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', {**self.a, **change})
                self.assertEqual(recover(self.runtime, self.key), {'status': 'superseded'})
        self.assertEqual(self.server.calls, [])

    def test_http_requires_session_token_and_trusted_origin(self):
        canvas = Canvas(Path(self.temp.name))
        canvas.runtime = self.runtime
        server = make_server(canvas)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        origin = f'http://127.0.0.1:{server.server_port}'
        def request(path, body=None, headers=None):
            request = urllib.request.Request(origin + path,
                data=json.dumps(body).encode() if body is not None else None,
                headers={'Content-Type': 'application/json', **(headers or {})})
            try:
                with urllib.request.urlopen(request, timeout=5) as response:
                    return response.status, json.loads(response.read())
            except urllib.error.HTTPError as error:
                try:
                    return error.code, json.loads(error.read())
                finally:
                    error.close()
        try:
            token = request('/api/state')[1]['token']
            path, body = '/api/connection-recovery', {'id': self.key}
            self.assertEqual(request(path, body)[0], 403)
            self.assertEqual(request(path, body, {'Origin': 'https://evil.invalid', 'X-Canvas-Token': token})[0], 403)
            self.assertEqual(self.server.calls, [])
            status, result = request(path, body, {'Origin': origin, 'X-Canvas-Token': token})
            self.assertEqual(status, 200)
            self.assertEqual(result['status'], 'reconciled')
            self.read_calls_only()
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == '__main__':
    unittest.main(verbosity=2)
