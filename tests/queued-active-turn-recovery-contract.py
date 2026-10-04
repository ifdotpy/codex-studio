#!/usr/bin/env python3
"""Restore an exact surviving turn behind a proven unsent local input wait."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('connection_fixture', Path(__file__).with_name('connection-recovery-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_connection_recovery as recovery


class QueuedActiveRecovery(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-queued-active-')
        self.addCleanup(self.temp.cleanup)
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.NativeHistoryServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=Path(self.temp.name), handle='account:default', generation=3)
        self.key = self.runtime.create({'name': 'Queued live turn', 'cwd': self.temp.name, 'prompt': ''},
                                       draft=True, defer=True)['id']
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent.update(threadId='native-thread', turnId='live-turn', status='queued', autoWake=True,
                         inFlight=False, activeTools=[{'id': 'remote-compile', 'type': 'commandExecution', 'name': 'Remote compile'}])
            for index in range(4):
                self.runtime.enqueue(db, agent, 'user', 'Pending input ' + str(index), 'pending-' + str(index))
            attempt = {'id': 'unsent-attempt', 'epoch': agent['epoch'], 'accountKey': agent['accountKey'],
                       'events': ['pending-0'], 'submitted': False, 'activeAtReservation': False}
            source = {field: agent[field] for field in ('id', 'epoch', 'accountKey', 'threadId')}
            wait = {'source': {**source, 'attemptId': attempt['id']}, 'events': attempt['events'],
                    'action': None, 'scope': 'local', 'error': 'Context repair waits for the current agent operation'}
            agent.update(startAttempt=attempt, contextRepairWait=wait, error=wait['error'],
                contextRepair={'id': 'unchanged-repair', 'agent': self.key, 'source': source, 'phase': 'unchanged'},
                restartRecovery={**source, 'turnId': 'live-turn', 'stage': 'continued',
                                 'autoWake': True, 'outcome': 'active'},
                connectionRecovery={'source': 'native_thread_read', 'turnId': 'live-turn',
                                    'outcome': 'active', 'automatic': True})
            self.runtime.put(db, 'agents', agent)
        self.initial = self.runtime.agent(self.key)
        self.server.native = {'id': 'native-thread', 'status': {'type': 'active'},
                              'turns': [{'id': 'live-turn', 'status': 'inProgress', 'items': []}]}
        self.server.calls.clear()

    def update(self, **values):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent.update(values)
            self.runtime.put(db, 'agents', agent)
        return agent

    def events(self):
        with self.runtime.read_db() as db:
            return [tuple(row) for row in db.execute('SELECT * FROM runtime_events ORDER BY id')]

    def assert_native_reads(self):
        self.assertEqual([method for method, _ in self.server.calls],
                         ['thread/read', 'thread/turns/list', 'thread/read', 'thread/turns/list'])
        for method, params in self.server.calls:
            self.assertEqual(params['threadId'], 'native-thread')
            if method == 'thread/read':
                self.assertFalse(params['includeTurns'])
            else:
                self.assertEqual(params['limit'], 1)
                self.assertEqual(params['itemsView'], 'notLoaded')

    def failed_idle_repair(self):
        scope = {field: self.initial[field] for field in ('epoch', 'accountKey', 'threadId', 'turnId')}
        return self.update(
            contextRepair={**self.initial['contextRepair'], 'phase': 'failed',
                'error': 'Context repair requires a confirmed idle native thread; native status: {"type":"active"}'},
            restartRecovery={**self.initial['restartRecovery'], 'stage': 'finished'},
            disconnectRecovery={**scope, 'autoWake': True, 'source': 'restart'},
            connectionRecovery={'source': 'native_thread_read', 'turnId': 'older-turn', 'outcome': 'interrupted'})

    def test_surviving_restart_turn_replaces_false_queued_state_without_native_input(self):
        self.failed_idle_repair()
        events = self.events()
        self.assertEqual(recovery.recover(self.runtime, self.key), {'status': 'adopted', 'turnId': 'live-turn'})
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'running')
        self.assertTrue(agent['inFlight'])
        self.assertEqual(agent['connectionRecovery']['turnId'], 'live-turn')
        self.assertNotIn('contextRepairWait', agent)
        self.assertEqual(self.events(), events)
        self.assert_native_reads()

    def test_failed_idle_repair_needs_the_exact_restart_scope(self):
        original = self.failed_idle_repair()
        for field, value in [('turnId', 'older-turn'), ('epoch', 1), ('source', 'transport'), ('autoWake', False)]:
            with self.subTest(field=field):
                changed = {**original['disconnectRecovery'], field: value}
                self.update(disconnectRecovery=changed)
                self.assertEqual(recovery.recover(self.runtime, self.key), {'status': 'superseded'})
        self.assertEqual(self.server.calls, [])

    def test_failed_idle_repair_does_not_replace_the_exact_native_turn(self):
        original = self.failed_idle_repair()
        self.server.native['turns'] = [{'id': 'newer-turn', 'status': 'inProgress', 'items': []}]
        self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'unconfirmed')
        self.assertEqual(self.runtime.agent(self.key), original)

    def test_automatic_and_manual_adopt_live_turn_without_new_native_input(self):
        events = self.events()
        for automatic in (False, True):
            with self.subTest(automatic=automatic):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.initial)
                self.server.calls.clear()
                with patch.object(self.runtime, 'connect', side_effect=AssertionError('Existing transport only')):
                    result = recovery.recover(self.runtime, self.key, automatic=automatic)
                self.assertEqual(result, {'status': 'adopted', 'turnId': 'live-turn'})
                agent = self.runtime.agent(self.key)
                self.assertEqual(agent['status'], 'running')
                self.assertTrue(agent['autoWake'])
                self.assertTrue(agent['inFlight'])
                self.assertEqual(agent['turnId'], 'live-turn')
                self.assertIsNone(agent['error'])
                self.assertNotIn('startAttempt', agent)
                self.assertNotIn('contextRepairWait', agent)
                self.assertEqual(agent['contextRepair'], self.initial['contextRepair'])
                self.assertEqual(agent['activeTools'], self.initial['activeTools'])
                self.assertEqual(self.events(), events)
                self.assert_native_reads()
                self.assertEqual(recovery.recover(self.runtime, self.key, automatic=automatic)['status'], 'superseded')

    def test_unknown_operation_receipts_survive_without_command_replay(self):
        operations = [('tasks', {'id': 'remote-compile', 'status': 'lost'}),
                      ('monitors', {'id': 'compile-monitor', 'status': 'lost'}),
                      ('tool_requests', {'id': 'unknown-tool', 'stage': 'finished', 'outcome': 'unknown'})]
        with self.runtime.db() as db:
            for table, operation in operations:
                self.runtime.put(db, table, {**operation, 'agent': self.key, 'turnId': 'live-turn'})
        events = self.events()
        self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'adopted')
        self.assertEqual(self.events(), events)
        with self.runtime.read_db() as db:
            for table, operation in operations:
                stored = json.loads(db.execute(f'SELECT record FROM runtime_{table}').fetchone()[0])
                self.assertEqual(stored, {**operation, 'agent': self.key, 'turnId': 'live-turn'})
        self.assert_native_reads()

    def test_changed_permission_wait_attempt_repair_and_uncertain_input_stay_held(self):
        initial = self.initial
        cases = [
            {'status': 'paused', 'autoWake': False, 'epoch': initial['epoch'] + 1},
            {'nativeFailureHold': True}, {'accountTransferId': 'transfer'}, {'workspaceOperation': 'operation'},
            {'startAttempt': {**initial['startAttempt'], 'submitted': True}},
            {'startAttempt': {**initial['startAttempt'], 'observedTurnId': 'another-turn'}},
            {'contextRepairWait': {**initial['contextRepairWait'], 'scope': 'native'}},
            {'contextRepairWait': {**initial['contextRepairWait'], 'events': ['pending-1']}},
            {'contextRepair': {**initial['contextRepair'], 'phase': 'unknown'}},
            {'restartRecovery': {**initial['restartRecovery'], 'turnId': 'another-turn'}},
            {'connectionRecovery': {**initial['connectionRecovery'], 'outcome': 'completed'}},
        ]
        for change in cases:
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', {**initial, **change})
                self.server.calls.clear()
                self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'superseded')
                self.assertEqual(self.runtime.agent(self.key), {**initial, **change})
                self.assertEqual(self.server.calls, [])
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', initial)
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id='pending-1'")
        events = self.events()
        self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'unconfirmed')
        self.assertEqual(self.runtime.agent(self.key), initial)
        self.assertEqual(self.events(), events)

    def test_callback_stop_and_connection_replacement_win_before_restore(self):
        for change in ({'status': 'paused', 'autoWake': False, 'epoch': 1},
                       {'contextRepair': {**self.initial['contextRepair'], 'phase': 'unknown'}}):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.initial)
                self.server.before_apply = lambda: self.update(**change)
                self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'superseded')
                self.assertEqual(self.runtime.agent(self.key), {**self.initial, **change})
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.initial)
        self.server.before_apply = lambda: self.runtime.connection_ids.update(default='replacement')
        self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'superseded')
        self.assertEqual(self.runtime.agent(self.key), self.initial)

    def test_changed_latest_turn_and_native_approval_are_not_adopted(self):
        native = copy.deepcopy(self.server.native)
        for change in ({'status': {'type': 'active', 'activeFlags': ['waitingOnApproval']}},
                       {'turns': [{'id': 'another-turn', 'status': 'inProgress', 'items': []}]}):
            with self.subTest(change=change):
                self.server.native = {**native, **change}
                self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'unconfirmed')
                self.assertEqual(self.runtime.agent(self.key), self.initial)

    def test_pending_request_after_native_reads_preserves_the_unsent_wait(self):
        for turn_id in ('live-turn', None):
            with self.subTest(turn_id=turn_id):
                def pending_request():
                    with self.runtime.db() as db:
                        self.runtime.put(db, 'requests', {'id': 'pending-request', 'agent': self.key,
                            'turnId': turn_id, 'status': 'pending', 'method': 'item/tool/requestUserInput'})
                self.server.before_apply = pending_request
                self.server.calls.clear()
                events = self.events()
                self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'unconfirmed')
                self.assertEqual(self.runtime.agent(self.key), self.initial)
                self.assertEqual(self.events(), events)
                self.assert_native_reads()
                with self.runtime.db() as db:
                    record = json.loads(db.execute('SELECT record FROM runtime_requests').fetchone()[0])
                    self.assertEqual(record['status'], 'pending')
                    self.assertEqual(record['turnId'], turn_id)
                    db.execute('DELETE FROM runtime_requests')

    def test_terminal_turn_reconciles_text_and_preserves_pending_and_unknown_receipts(self):
        operations = [('tasks', {'id': 'remote-compile', 'status': 'lost'}),
                      ('monitors', {'id': 'compile-monitor', 'status': 'lost'}),
                      ('tool_requests', {'id': 'unknown-tool', 'stage': 'finished', 'outcome': 'unknown'})]
        with self.runtime.db() as db:
            for table, operation in operations:
                self.runtime.put(db, table, {**operation, 'agent': self.key, 'turnId': 'live-turn'})
        events = self.events()
        self.server.native = {'id': 'native-thread', 'status': {'type': 'idle'}, 'turns': [{
            'id': 'live-turn', 'status': 'completed', 'items': [
                {'id': 'answer', 'type': 'agentMessage', 'text': 'The compile result', 'phase': 'final_answer'},
                {'id': 'remote-compile', 'type': 'commandExecution', 'command': 'must-not-run'},
            ]}]}
        original = self.runtime.apply_turn_recovery
        def apply(expected, connection, state, turn):
            self.assertTrue(self.runtime.lock._is_owned())
            with self.runtime.read_db() as db:
                committed = self.runtime.agent(self.key, db)
            self.assertTrue(committed['inFlight'])
            self.assertNotIn('startAttempt', committed)
            self.assertNotIn('contextRepairWait', committed)
            return original(expected, connection, state, turn)
        with patch.object(self.runtime, 'apply_turn_recovery', side_effect=apply):
            result = recovery.recover(self.runtime, self.key)
        self.assertEqual(result, {'status': 'reconciled', 'turnId': 'live-turn', 'outcome': 'completed'})
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'queued')
        self.assertFalse(agent['inFlight'])
        self.assertIsNone(agent['turnId'])
        self.assertEqual(agent['lastAnswer'], 'The compile result')
        self.assertEqual(agent['restartRecovery']['stage'], 'finished')
        self.assertEqual(agent['contextRepair'], self.initial['contextRepair'])
        self.assertEqual(self.events(), events)
        with self.runtime.read_db() as db:
            for table, operation in operations:
                stored = json.loads(db.execute(f'SELECT record FROM runtime_{table}').fetchone()[0])
                self.assertEqual(stored, {**operation, 'agent': self.key, 'turnId': 'live-turn'})
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0], 1)
            item = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?',
                                        (self.key + ':answer',)).fetchone()[0])
            self.assertEqual(item['text'], 'The compile result')
            self.assertEqual(item['turnStatus'], 'completed')
        self.assertEqual([method for method, _ in self.server.calls],
            ['thread/read', 'thread/turns/list', 'thread/read', 'thread/turns/list',
             'thread/turns/list', 'thread/items/list', 'thread/turns/list', 'thread/read', 'thread/turns/list'])
        item_reads = [params for method, params in self.server.calls if method == 'thread/items/list']
        self.assertEqual(len(item_reads), 1)
        self.assertEqual(item_reads[0]['turnId'], 'live-turn')
        self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'superseded')
        self.assertEqual(len(self.server.calls), 9)

    def test_failed_and_interrupted_terminal_turns_use_the_normal_outcome_policy(self):
        for outcome in ('failed', 'interrupted'):
            with self.subTest(outcome=outcome):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.initial)
                    db.execute('DELETE FROM runtime_completed_turns')
                events = self.events()
                error = {'message': 'The native turn failed'} if outcome == 'failed' else None
                self.server.native = {'id': 'native-thread', 'status': {'type': 'idle'}, 'turns': [{
                    'id': 'live-turn', 'status': outcome, 'error': error, 'items': []}]}
                self.server.calls.clear()
                self.assertEqual(recovery.recover(self.runtime, self.key),
                                 {'status': 'reconciled', 'turnId': 'live-turn', 'outcome': outcome})
                agent = self.runtime.agent(self.key)
                self.assertEqual(agent['status'], 'failed' if outcome == 'failed' else 'queued')
                self.assertEqual(bool(agent.get('nativeFailureHold')), outcome == 'failed')
                self.assertFalse(agent['inFlight'])
                self.assertIsNone(agent['turnId'])
                self.assertEqual(agent['lastCompletedTurnStatus'], outcome)
                self.assertEqual(agent['restartRecovery']['stage'], 'finished')
                self.assertNotIn('startAttempt', agent)
                self.assertNotIn('contextRepairWait', agent)
                self.assertEqual(self.events(), events)
                self.assertTrue(all(method in {'thread/read', 'thread/turns/list', 'thread/items/list'}
                                    for method, _ in self.server.calls))

    def test_terminal_full_turn_and_final_native_status_must_still_match(self):
        self.server.native = {'id': 'native-thread', 'status': {'type': 'idle'},
            'turns': [{'id': 'live-turn', 'status': 'completed', 'items': [
                {'id': 'answer', 'type': 'agentMessage', 'text': 'Exact answer'}]}]}
        original = self.server.call
        for change in ('id', 'outcome', 'latest', 'request'):
            with self.subTest(change=change):
                pages = []
                self.server.calls.clear()
                self.server.before_apply = None
                self.server.native['turns'][0]['status'] = 'completed'
                def call(method, params, timeout=60):
                    result = original(method, params, timeout)
                    if method == 'thread/items/list':
                        if change == 'id': result['data'][0]['turnId'] = 'other-turn'
                        if change == 'outcome': self.server.native['turns'][0]['status'] = 'failed'
                    if method == 'thread/turns/list':
                        pages.append(params['itemsView'])
                        if len(pages) == 5 and change == 'latest':
                            return {'data': [{'id': 'newer-turn', 'status': 'completed'}]}
                    return result
                if change == 'request':
                    def pending_request():
                        with self.runtime.db() as db:
                            self.runtime.put(db, 'requests', {'id': 'pending-request', 'agent': self.key,
                                'turnId': 'live-turn', 'status': 'pending', 'method': 'item/commandExecution/requestApproval'})
                    self.server.before_apply = pending_request
                events = self.events()
                with patch.object(self.server, 'call', side_effect=call):
                    self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'unconfirmed')
                self.assertEqual(self.runtime.agent(self.key), self.initial)
                self.assertEqual(self.events(), events)

    def test_second_native_check_and_supervisor_generation_must_still_match(self):
        original = self.server.call
        pages = []
        def call(method, params, timeout=60):
            result = original(method, params, timeout)
            if method == 'thread/turns/list':
                pages.append(True)
                if len(pages) == 2:
                    return {'data': [{'id': 'newer-turn', 'status': 'inProgress'}]}
            return result
        with patch.object(self.server, 'call', side_effect=call):
            self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'unconfirmed')
        self.assertEqual(self.runtime.agent(self.key), self.initial)
        self.assert_native_reads()
        self.server.calls.clear()
        self.server.before_apply = lambda: setattr(self.server.proc, 'generation', 4)
        self.assertEqual(recovery.recover(self.runtime, self.key)['status'], 'superseded')
        self.assertEqual(self.runtime.agent(self.key), self.initial)
        self.assert_native_reads()


if __name__ == '__main__':
    unittest.main()
