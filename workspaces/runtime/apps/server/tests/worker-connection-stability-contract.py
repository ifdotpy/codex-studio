#!/usr/bin/env python3
"""Account recovery stays independent and only adopts a proven surviving turn."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch
from entity_test_support import context_repair_wait

spec = importlib.util.spec_from_file_location(
    'connection_fixture', Path(__file__).with_name('connection-recovery-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_connection_recovery as recovery


class WorkerConnectionStability(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-worker-connection-')
        self.addCleanup(self.temp.cleanup)
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=Path(self.temp.name), handle='account:default', generation=4)
        self.key = self.runtime.create({'name': 'Worker', 'cwd': self.temp.name, 'prompt': ''},
                                       draft=True, defer=True)['id']
        self.authorize()
        self.server.native = {'id': 'native-thread', 'status': {'type': 'active'},
                              'turns': [{'id': 'lost-turn', 'status': 'inProgress', 'items': []}]}
        self.server.calls.clear()

    def update(self, key=None, **values):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(key or self.key, db)
            agent.update(values)
            self.runtime.put(db, 'agents', agent)
        return agent

    def authorize(self, **values):
        agent = self.update(threadId='native-thread', turnId='lost-turn',
                            status='interrupted', inFlight=False, autoWake=False,
                            error='Codex disconnected. Review the transcript before resuming.',
                            startAttempt=None, nativeFailureHold=False)
        permission = {field: agent.get(field) for field in ('epoch', 'accountKey', 'threadId', 'turnId')}
        permission.update(autoWake=True, connectionId=self.runtime.connection_ids['default'],
                          supervisor={'stateDir': str(Path(self.temp.name).resolve()),
                                      'handle': 'account:default', 'generation': 4})
        return self.update(disconnectRecovery=permission, **values)

    def assert_reads_only(self, server=None):
        calls = (server or self.server).calls
        self.assertTrue(calls)
        self.assertTrue(all(method in {'thread/read', 'thread/turns/list'} for method, _ in calls), calls)

    def idle(self):
        fixture.fixture.fixture.eventually(lambda: not getattr(self.runtime, '_connection_recovery_busy', False))

    def test_transport_loss_adopts_exact_active_turn_without_new_input(self):
        result = recovery.recover(self.runtime, self.key, automatic=True)
        self.assertEqual(result, {'status': 'adopted', 'turnId': 'lost-turn'})
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'running')
        self.assertTrue(agent['autoWake'])
        self.assertTrue(agent['inFlight'])
        self.assertIsNone(agent['error'])
        self.assertIn(self.key, self.runtime.loaded)
        self.assert_reads_only()
        self.assertEqual(recovery.recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events').fetchone()[0], 0)

    def test_confirmed_existing_attempt_does_not_block_read_only_adoption(self):
        agent = self.runtime.agent(self.key)
        for turn_id in ('lost-turn', None):
            with self.subTest(turn_id=turn_id):
                attempt = {'id': 'existing-start', 'epoch': agent['epoch'], 'action': None,
                           'accountKey': agent['accountKey'], 'submitted': True, 'events': []}
                if turn_id:
                    attempt['turnId'] = turn_id
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', {**agent, 'startAttempt': attempt,
                        'disconnectRecovery': {**agent['disconnectRecovery'], 'startAttempt': attempt}})
                self.assertEqual(recovery.recover(self.runtime, self.key, automatic=True)['status'], 'adopted')
                self.assertEqual(self.runtime.agent(self.key)['startAttempt'], attempt)
        self.assert_reads_only()

    def test_identity_permission_approval_and_unknown_receipts_remain_held(self):
        initial = self.runtime.agent(self.key)
        cases = [
            {'disconnectRecovery': {**initial['disconnectRecovery'], 'epoch': -1}},
            {'disconnectRecovery': {**initial['disconnectRecovery'], 'autoWake': False}},
            {'disconnectRecovery': {**initial['disconnectRecovery'], 'turnId': 'other-turn'}},
            {'disconnectRecovery': {**initial['disconnectRecovery'], 'supervisor': {
                **initial['disconnectRecovery']['supervisor'], 'generation': 3}}},
            {'nativeFailureHold': True}, {'status': 'approval'},
            {'accountTransferId': 'transfer'}, {'workspaceOperation': 'operation'},
            {'contextRepair': {'id': 'unknown-fork', 'phase': 'unknown'}},
            {'contextRepairWait': context_repair_wait('Waiting for receipt')},
            {'status': 'paused', 'epoch': initial['epoch'] + 1, 'error': 'Stopped by user'},
            {'startAttempt': {'id': 'unknown-start', 'submitted': True}},
        ]
        for change in cases:
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', {**initial, **change})
                self.server.calls.clear()
                result = recovery.recover(self.runtime, self.key, automatic=True)
                self.assertIn(result['status'], {'superseded', 'unconfirmed'})
                current = self.runtime.agent(self.key)
                current.pop('connectionCheck', None)
                self.assertEqual(current, {**initial, **change})
                self.assertFalse(any(method == 'turn/start' for method, _ in self.server.calls))
        for table, operation in [
            ('tasks', {'id': 'unknown-task', 'status': 'lost'}),
            ('monitors', {'id': 'unknown-monitor', 'status': 'lost'}),
            ('tool_requests', {'id': 'unknown-tool', 'outcome': 'unknown', 'stage': 'finished'}),
            ('requests', {'id': 'pending-approval', 'status': 'pending'}),
        ]:
            with self.subTest(table=table):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', initial)
                    self.runtime.put(db, table, {**operation, 'agent': self.key, 'turnId': 'lost-turn'})
                result = recovery.recover(self.runtime, self.key, automatic=True)
                self.assertEqual(result['status'], 'unconfirmed')
                self.assertFalse(self.runtime.agent(self.key)['autoWake'])
                with self.runtime.db() as db:
                    stored = json.loads(db.execute(f'SELECT record FROM runtime_{table}').fetchone()[0])
                    self.assertEqual(stored, {**operation, 'agent': self.key, 'turnId': 'lost-turn'})
                    db.execute(f'DELETE FROM runtime_{table}')
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', initial)
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       ('unknown-input', self.key, 'user', 'Original input', 'uncertain',
                        1, initial['epoch'], None, None))
        self.assertEqual(recovery.recover(self.runtime, self.key, automatic=True)['status'], 'unconfirmed')
        self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        with self.runtime.db() as db:
            self.assertEqual(tuple(db.execute('SELECT id,text,status FROM runtime_events').fetchone()),
                             ('unknown-input', 'Original input', 'uncertain'))
        self.assert_reads_only()

    def test_old_active_turn_and_pending_native_approval_are_not_adopted(self):
        initial = self.runtime.agent(self.key)
        native = copy.deepcopy(self.server.native)
        cases = [
            {'turns': [{'id': 'newer-turn', 'status': 'inProgress', 'items': []}, *native['turns']]},
            {'status': {'type': 'active', 'activeFlags': ['waitingOnApproval']}},
            {'status': {'type': 'active', 'activeFlags': ['waitingOnUserInput']}},
        ]
        for change in cases:
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', initial)
                self.server.native = {**native, **change}
                self.assertEqual(recovery.recover(self.runtime, self.key, automatic=True)['status'], 'unconfirmed')
                self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        self.assert_reads_only()

    def test_user_stop_and_connection_replacement_win_before_apply(self):
        initial = self.runtime.agent(self.key)
        for change in ({'status': 'paused', 'epoch': 1, 'error': 'Stopped by user'},
                       {'nativeFailureHold': True}):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', initial)
                self.server.before_apply = lambda: self.update(**change)
                self.assertEqual(recovery.recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
                self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        self.server.before_apply = None
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', initial)
        self.server.before_apply = lambda: self.runtime.connection_ids.update(default='replacement-connection')
        self.assertEqual(recovery.recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
        self.assert_reads_only()

    def test_stalled_account_does_not_block_other_accounts_or_use_shared_pool(self):
        other = self.runtime.create({'name': 'Other account', 'cwd': self.temp.name, 'prompt': ''},
                                    draft=True, defer=True)
        other = self.update(other['id'], accountKey='other', threadId='other-thread', turnId='other-turn',
                            status='interrupted', autoWake=False, inFlight=False,
                            error='Codex disconnected. Review the transcript before resuming.')
        account_get = self.runtime.accounts.get
        with patch.object(self.runtime.accounts, 'get', side_effect=lambda key:
                          account_get('default') if key == 'other' else account_get(key)):
            other_server = self.runtime.connect('other')
            other_server.native = {'id': 'other-thread', 'status': {'type': 'idle'},
                                   'turns': [{'id': 'other-turn', 'status': 'completed', 'items': []}]}
            self.server.read_gate = threading.Event()
            other_completed = threading.Event()
            original_put = self.runtime.put

            def put_and_signal(db, table, record):
                result = original_put(db, table, record)
                if table == 'agents' and record.get('id') == other['id'] and record.get('status') == 'completed':
                    other_completed.set()
                return result

            try:
                with patch.object(self.runtime, 'put', side_effect=put_and_signal), \
                     patch.object(self.runtime.recovery_pool, 'submit', side_effect=AssertionError('shared pool blocked')):
                    recovery.tick(self.runtime, [self.runtime.agent(self.key), other])
                    self.assertTrue(self.server.read_entered.wait(2))
                    self.assertTrue(other_completed.wait(30), 'other account recovery did not complete')
                    calls = list(self.server.calls)
                    recovery.tick(self.runtime, [self.runtime.agent(self.key), self.runtime.agent(other['id'])])
                    self.assertEqual(self.server.calls, calls)
                    self.assertTrue(self.runtime._connection_recovery_busy)
            finally:
                self.server.read_gate.set()
                self.idle()
            self.assert_reads_only(other_server)

    def test_account_jobs_have_a_limit_and_waiting_accounts_receive_the_next_slot(self):
        account_get = self.runtime.accounts.get
        agents = [self.runtime.agent(self.key)]
        servers = [self.server]
        with patch.object(self.runtime.accounts, 'get', side_effect=lambda key: account_get('default')):
            for index in range(recovery.MAX_ACCOUNT_RECOVERIES):
                account = 'other-' + str(index)
                agent = self.runtime.create({'name': account, 'cwd': self.temp.name, 'prompt': ''},
                                            draft=True, defer=True)
                agent = self.update(agent['id'], accountKey=account, threadId='thread-' + account,
                    turnId='turn-' + account, status='interrupted', autoWake=False, inFlight=False,
                    error='Codex disconnected. Review the transcript before resuming.')
                server = self.runtime.connect(account)
                server.native = {'id': agent['threadId'], 'status': {'type': 'idle'},
                    'turns': [{'id': agent['turnId'], 'status': 'completed', 'items': []}]}
                agents.append(agent)
                servers.append(server)
            for server in servers:
                server.read_gate = threading.Event()
            try:
                recovery.tick(self.runtime, agents)
                for server in servers[:-1]:
                    self.assertTrue(server.read_entered.wait(1))
                self.assertEqual(len(self.runtime._connection_recovery_jobs), 8)
                self.assertFalse(servers[-1].read_entered.is_set())
                servers[1].read_gate.set()
                fixture.fixture.fixture.eventually(lambda: len(self.runtime._connection_recovery_jobs) == 7)
                recovery.tick(self.runtime, [self.runtime.agent(agent['id']) for agent in agents])
                self.assertTrue(servers[-1].read_entered.wait(1))
                self.assertEqual(len(self.runtime._connection_recovery_jobs), 8)
            finally:
                for server in servers:
                    server.read_gate.set()
                self.idle()
            for server in servers:
                self.assert_reads_only(server)

    def test_independent_service_recovers_without_global_dispatch(self):
        self.server.native['status']['type'] = 'idle'
        self.server.native['turns'][0]['status'] = 'completed'
        completed = threading.Event()
        original_put = self.runtime.put

        def put_and_signal(db, table, record):
            result = original_put(db, table, record)
            if table == 'agents' and record.get('id') == self.key and record.get('status') == 'completed':
                completed.set()
            return result

        with patch.object(self.runtime, 'put', side_effect=put_and_signal), \
             patch.object(self.runtime, 'dispatch', side_effect=AssertionError('scheduler must not run')):
            manager = recovery.start(self.runtime, interval=.02)
            self.addCleanup(manager.close)
            self.assertTrue(completed.wait(30), 'independent recovery did not complete')
            self.assertIs(recovery.start(self.runtime, interval=.02), manager)
            manager.close()
            self.assertFalse(manager.thread.is_alive())
        self.assert_reads_only()

    def test_timer_skips_busy_runtime_and_closes_without_waiting_for_its_lock(self):
        with self.runtime.lock, patch.object(self.runtime, 'scheduler_agents') as scan:
            manager = recovery.start(self.runtime, interval=.01)
            threading.Event().wait(.05)
            manager.close()
            scan.assert_not_called()
            self.assertFalse(manager.thread.is_alive())
        self.assertEqual(self.server.calls, [])

    def test_live_update_waits_for_the_previous_recovery_reader(self):
        self.runtime._connection_recovery_busy = True
        recovery.tick(self.runtime, [self.runtime.agent(self.key)])
        self.assertEqual(self.server.calls, [])
        self.assertEqual(self.runtime._connection_recovery_jobs, {})
        self.runtime._connection_recovery_busy = False
        recovery.tick(self.runtime, [self.runtime.agent(self.key)])
        self.idle()
        self.assertEqual(self.runtime.agent(self.key)['status'], 'running')
        self.assert_reads_only()

    def test_new_disconnect_during_the_same_turn_does_not_inherit_old_backoff(self):
        original = self.runtime.agent(self.key)
        for error in (None, TimeoutError('Temporary read failure')):
            with self.subTest(error=error), patch('codex_connection_recovery.time.time', return_value=100):
                initial = copy.deepcopy(original)
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', initial)
                self.runtime._connection_recovery_checks = {}
                self.server.read_error = error
                recovery.tick(self.runtime, [initial])
                self.idle()
                self.assertTrue(all(check['failures'] == (1 if error else 0)
                                    for check in self.runtime._connection_recovery_checks.values()))
                previous_calls = len(self.server.calls)
                initial['disconnectRecovery']['at'] = 99
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', initial)
                self.server.read_error = None
                recovery.tick(self.runtime, [initial])
                self.idle()
                self.assertEqual(self.runtime.agent(self.key)['status'], 'running')
                self.assertGreater(len(self.server.calls), previous_calls)
        self.assert_reads_only()

    def test_ready_codex_connection_does_not_wait_for_another_account_start(self):
        entered, release, returned = threading.Event(), threading.Event(), threading.Event()
        result = []
        def hold_start():
            with self.runtime.start_lock:
                entered.set()
                release.wait(2)
        def connect():
            result.append(self.runtime.connect('default'))
            returned.set()
        holder = threading.Thread(target=hold_start)
        reader = threading.Thread(target=connect)
        holder.start()
        try:
            self.assertTrue(entered.wait(1))
            reader.start()
            self.assertTrue(returned.wait(.5), 'Ready Codex account waits behind another account start')
        finally:
            release.set()
            holder.join()
            if reader.ident is not None:
                reader.join()
        self.assertEqual(result, [self.server])

    def test_ready_connection_waits_for_an_account_tool_refresh_reservation(self):
        entered, release, returned = threading.Event(), threading.Event(), threading.Event()
        result = []
        def hold_refresh():
            with self.runtime.start_lock:
                with self.runtime.lock:
                    self.runtime._native_tools_refreshing = {'default'}
                entered.set()
                release.wait(2)
                with self.runtime.lock:
                    self.runtime._native_tools_refreshing.clear()
        def connect():
            result.append(self.runtime.connect('default'))
            returned.set()
        holder = threading.Thread(target=hold_refresh)
        reader = threading.Thread(target=connect)
        holder.start()
        try:
            self.assertTrue(entered.wait(1))
            reader.start()
            self.assertFalse(returned.wait(.1), 'Ready connection bypassed its tool refresh reservation')
            release.set()
            self.assertTrue(returned.wait(1))
        finally:
            release.set()
            holder.join()
            if reader.ident is not None:
                reader.join()
        self.assertEqual(result, [self.server])

    def test_late_constructor_closes_unpublished_transport_and_drains_callbacks(self):
        entered, release, returned = threading.Event(), threading.Event(), threading.Event()
        callback_release, callback_done = threading.Event(), threading.Event()
        constructed, errors = [], []
        runtime = self.runtime
        class GatedServer(fixture.fixture.RecoveryServer):
            def __init__(self, *args):
                super().__init__(*args)
                self.join_called = False
                def callback():
                    callback_release.wait(3)
                    with runtime.lock:
                        callback_done.set()
                self.callback = threading.Thread(target=callback)
                self.callback.start()
                constructed.append(self)
                entered.set()
                if not release.wait(3):
                    raise TimeoutError('Fixture constructor was not released')
            def close(self):
                super().close()
                callback_release.set()
            def join_callbacks(self, timeout=10):
                self.join_called = True
                self.callback.join(min(timeout, 2))
                return not self.callback.is_alive()
        def connect():
            try:
                runtime.connect('late')
            except Exception as error:
                errors.append(error)
            finally:
                returned.set()
        reader = threading.Thread(target=connect)
        account_get = runtime.accounts.get
        factory = runtime.factory
        runtime.factory = GatedServer
        with patch.object(runtime.accounts, 'get', side_effect=lambda key: account_get('default')):
            try:
                reader.start()
                self.assertTrue(entered.wait(1))
                with runtime.lock:
                    runtime.closed = True
                    self.assertNotIn('late', runtime.servers)
                release.set()
                self.assertTrue(returned.wait(2), 'Late constructor did not close and drain outside Runtime.lock')
                self.assertEqual(len(constructed), 1)
                self.assertTrue(constructed[0].closed)
                self.assertTrue(constructed[0].join_called)
                self.assertTrue(callback_done.is_set())
                self.assertFalse(constructed[0].callback.is_alive())
                self.assertNotIn('late', runtime.servers)
                self.assertEqual(len(errors), 1)
                self.assertIsInstance(errors[0], RuntimeError)
                self.assertEqual(str(errors[0]), 'Runtime is stopped')
            finally:
                release.set()
                callback_release.set()
                reader.join(3)
                for server in constructed:
                    server.close()
                    server.join_callbacks()
                with runtime.lock:
                    runtime.servers.pop('late', None)
                    runtime.closed = False
                runtime.factory = factory


if __name__ == '__main__':
    unittest.main()
