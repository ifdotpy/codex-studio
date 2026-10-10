#!/usr/bin/env python3
"""Exact historical input proof preserves the newer unsent input batch."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('context_input_fixture',
    Path(__file__).with_name('context-repair-wait-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
repair, eventually = f.repair, f.eventually


class HistoricalInputWait(f.ContextWait):
    def setUp(self):
        super().setUp()
        self.jobs = []
        self.serial = 0
        self.before_history = None
        self.history_error = None
        self.native_state = 'idle'
        self.native_turns = []
        self.next_cursor = None
        self.native_thread = self.tid
        original_submit = self.runtime.recovery_pool.submit

        def submit(function, *args):
            if function.__name__ != '_run_restart_input_check':
                return original_submit(function, *args)
            self.jobs.append((function, args))
            return concurrent.futures.Future()

        self.capture = patch.object(self.runtime.recovery_pool, 'submit', side_effect=submit)
        self.capture.start()
        self.addCleanup(self.capture.stop)
        original_call = self.server.call

        def call(method, params, timeout=10):
            if method not in {'thread/read', 'thread/turns/list'}:
                return original_call(method, params, timeout)
            self.server.calls.append((method, copy.deepcopy(params)))
            if method == 'thread/read':
                return {'thread':{'id':self.native_thread,
                                  'status':{'type':self.native_state}, 'path':str(self.path)}}
            if self.before_history:
                callback, self.before_history = self.before_history, None
                callback()
            if self.history_error:
                raise self.history_error
            return {'data':copy.deepcopy(self.native_turns), 'nextCursor':self.next_cursor}

        self.server.call = call

    def make_wait(self, *, outcome='accepted', native=True, metadata=None):
        self.serial += 1
        self.old_id, self.new_id = 'historical-' + str(self.serial), 'new-input-' + str(self.serial)
        self.runtime.send(self.a['id'], 'Keep the old receipt.', message_id=self.old_id)
        self.runtime.send(self.a['id'], 'Keep the new input.', message_id=self.new_id)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.a['id'], db)
            db.execute("UPDATE runtime_events SET kind='monitor_exit',status='uncertain',error='Native response was lost' WHERE id=?",
                       (self.old_id,))
            if metadata is None:
                metadata = {'contextManifest':{'epoch':[self.tid, 1], 'versions':{}, 'sequence':1}}
            db.execute('INSERT OR REPLACE INTO runtime_event_meta VALUES (?,?)',
                       (self.old_id, json.dumps(metadata)))
            attempt = {'id':'new-attempt-' + str(self.serial), 'events':[self.new_id],
                       'submitted':False, 'epoch':agent['epoch'], 'accountKey':'default'}
            agent.update(startAttempt=attempt, status='queued', autoWake=True, inFlight=False,
                nativeFailureHold=None, turnId=None,
                contextRepair={'phase':'unchanged', 'source':{'threadId':self.tid},
                               'compactions':agent.get('compactions', 0),
                               'checkedEventIds':[self.event['id']]})
            error = 'Context repair waits for a confirmed input receipt: ' + self.old_id
            agent.update(error=error, contextRepairWait={'source':repair._identity(agent),
                'events':[self.new_id], 'action':None, 'actionRequestId':None, 'actionIdentity':None,
                'scope':'local', 'error':error, 'nextCheckAt':0})
            self.runtime.put(db, 'agents', agent)
            self.pending_before = dict(db.execute('SELECT * FROM runtime_events WHERE id=?',
                                                  (self.new_id,)).fetchone())
        self.attempt_before = copy.deepcopy(attempt)
        self.native_turns = [{'id':'historical-native-turn', 'status':'completed',
            'startOutcome':outcome, 'clientUserMessageId':self.old_id}] if native else []
        self.server.calls.clear()

    def receipt(self, key):
        with self.runtime.db() as db:
            return dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (key,)).fetchone())

    def run_check(self):
        function, args = self.jobs.pop(0)
        function(*args)

    def assert_preserved_new_input(self):
        agent = self.runtime.agent(self.a['id'])
        self.assertEqual(agent['startAttempt'], self.attempt_before)
        self.assertFalse(agent['inFlight'])
        self.assertIsNone(agent['turnId'])
        self.assertEqual(self.receipt(self.new_id), self.pending_before)
        self.assertFalse(any(method in {'turn/start', 'turn/steer', 'thread/resume', 'thread/fork',
                                        'command/exec'} for method, _ in self.server.calls))

    def test_manual_recovery_resolves_only_old_receipt(self):
        self.make_wait()
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved')
        self.assertEqual(result['inputs'], [{'id':self.old_id, 'decision':'delivered',
                                            'turnId':'historical-native-turn'}])
        self.assertEqual(self.receipt(self.old_id)['status'], 'delivered')
        self.assertEqual(self.receipt(self.old_id)['turn_id'], 'historical-native-turn')
        self.assertFalse(self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.assert_preserved_new_input()
        self.assertEqual(repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])['status'], 'not_needed')

    def test_saved_positive_receipt_does_not_read_large_native_history(self):
        self.make_wait()
        with patch('codex_native_input_projection.accepted_turns', return_value=self.native_turns):
            result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved')
        self.assertEqual(self.receipt(self.old_id)['status'], 'delivered')
        self.assert_preserved_new_input()
        self.assertEqual(self.server.calls, [])

    def test_historical_fallback_requests_the_turn_summary(self):
        self.make_wait()
        original = self.server.call

        def bounded(method, params, timeout=10):
            if method == 'thread/turns/list' and params.get('itemsView') != 'summary':
                raise TimeoutError('Full tool output blocks the native pipe')
            return original(method, params, timeout)

        self.server.call = bounded
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved')
        self.assert_preserved_new_input()

    def test_dispatch_schedules_one_check_then_sends_only_new_input_once(self):
        self.make_wait()
        self.runtime.dispatch()
        self.assertEqual(len(self.jobs), 1)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.a['id'], db)
            agent['contextRepairWait']['nextCheckAt'] = 0
            self.runtime.put(db, 'agents', agent)
        self.runtime.dispatch()
        self.assertEqual(len(self.jobs), 1)
        self.assert_preserved_new_input()
        self.run_check()
        self.assertEqual(self.receipt(self.old_id)['status'], 'delivered')
        self.assert_preserved_new_input()
        with patch.object(repair, 'repair_before_start', side_effect=lambda _runtime, agent:agent):
            self.runtime.dispatch()
            eventually(lambda:self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
            for _ in range(2):
                self.runtime.dispatch()
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual([params['clientUserMessageId'] for params in starts], [self.new_id])
        self.assertEqual(self.runtime.agent(self.a['id'])['startAttempt']['events'], [self.new_id])
        self.assertEqual(self.runtime.agent(self.a['id'])['turnId'], 'resumed-turn')
        self.assertEqual(self.receipt(self.old_id)['turn_id'], 'historical-native-turn')

    def test_native_client_id_confirms_old_input_without_adopting_old_turn(self):
        self.make_wait(native=False)
        self.native_turns = [{'id':'historical-native-turn', 'status':'completed',
                             'items':[{'type':'userMessage', 'clientId':self.old_id}]}]
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved')
        self.assert_preserved_new_input()

    def test_absent_idle_or_active_input_remains_uncertain(self):
        for state in ('idle', 'active', 'notLoaded'):
            with self.subTest(state=state):
                self.make_wait(native=False)
                self.native_state = state
                self.native_turns = [{'id':'another-turn', 'clientUserMessageId':'another-input'}]
                result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
                self.assertTrue(self.runtime.agent(self.a['id']).get('contextRepairWait'))
                self.assert_preserved_new_input()

    def test_preparing_or_unknown_native_outcome_is_not_acceptance(self):
        for outcome in ('preparing', 'unknown'):
            with self.subTest(outcome=outcome):
                self.make_wait(outcome=outcome)
                result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
                self.assert_preserved_new_input()

    def test_exact_native_rejection_can_requeue_old_input_without_changing_new_attempt(self):
        self.make_wait(outcome='not_applied')
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'], [{'id':self.old_id, 'decision':'not_delivered'}])
        self.assertEqual(self.receipt(self.old_id)['status'], 'pending')
        self.assert_preserved_new_input()

    def test_unreadable_history_keeps_both_receipts(self):
        self.make_wait()
        self.history_error = TimeoutError('fixture history timeout')
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting')
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()

    def test_missing_turn_id_is_not_acceptance(self):
        self.make_wait()
        self.native_turns[0].pop('id')
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting')
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()

    def test_invalid_native_turn_id_is_not_acceptance(self):
        for turn_id in (37, '', None):
            with self.subTest(turn_id=turn_id):
                self.make_wait()
                self.native_turns[0]['id'] = turn_id
                result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
                self.assert_preserved_new_input()

    def test_finished_restart_unsent_marker_does_not_prove_old_input_absence(self):
        self.make_wait(native=False)
        self.native_turns = [{'id':'other-turn', 'clientUserMessageId':'other-input'}]
        agent = self.runtime.agent(self.a['id'])
        self.agent_update(agent, restartRecovery={'stage':'finished', 'epoch':agent['epoch'],
            'accountKey':'default', 'threadId':self.tid, 'autoWake':True,
            'startAttempt':{'id':'old-unsent', 'epoch':agent['epoch'], 'accountKey':'default',
                            'events':[self.old_id], 'submitted':False}})
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting')
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()

    def test_history_is_bounded_and_positive_proof_stops_pagination(self):
        self.make_wait()
        self.next_cursor = 'another-page'
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved')
        self.assertEqual(sum(method == 'thread/turns/list' for method, _ in self.server.calls), 1)
        self.assert_preserved_new_input()

    def test_absent_input_cannot_read_more_than_sixteen_history_pages(self):
        self.make_wait(native=False)
        original = self.server.call
        page_count = 0
        def call(method, params, timeout=10):
            nonlocal page_count
            result = original(method, params, timeout)
            if method == 'thread/turns/list':
                page_count += 1
                result['nextCursor'] = 'page-' + str(page_count)
            return result
        self.server.call = call
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting')
        self.assertEqual(page_count, 16)
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()

    def test_multiple_pending_ids_and_new_arrival_stay_queued(self):
        self.make_wait()
        self.runtime.send(self.a['id'], 'Keep the second pending input.', message_id='second-pending')
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.a['id'], db)
            agent['startAttempt']['events'].append('second-pending')
            agent['contextRepairWait']['events'].append('second-pending')
            self.runtime.put(db, 'agents', agent)
            self.attempt_before = copy.deepcopy(agent['startAttempt'])
        second = self.receipt('second-pending')
        self.before_history = lambda:self.runtime.send(self.a['id'], 'Keep the later input.',
                                                      message_id='later-pending')
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved')
        self.assert_preserved_new_input()
        self.assertEqual(self.receipt('second-pending'), second)
        self.assertEqual(self.receipt('later-pending')['status'], 'pending')
        self.assertIsNone(self.receipt('later-pending')['turn_id'])

    def test_old_input_proof_preserves_unknown_task_receipt(self):
        self.make_wait()
        task = {'id':self.a['id'] + ':unknown-command', 'agent':self.a['id'],
                'itemId':'unknown-command', 'turnId':'unknown-turn', 'type':'commandExecution',
                'kind':'command', 'status':'unknown', 'created':1}
        self.put('tasks', task)
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'resolved')
        self.assert_preserved_new_input()
        with self.runtime.db() as db:
            stored = json.loads(db.execute('SELECT record FROM runtime_tasks WHERE id=?',
                                          (task['id'],)).fetchone()[0])
        self.assertEqual(stored, task)
        self.assertEqual(self.receipt(self.new_id)['status'], 'pending')
        self.assertFalse(any(method in {'turn/start', 'command/exec'} for method, _ in self.server.calls))

    def test_total_deadline_prevents_another_native_read(self):
        self.make_wait()
        with patch('codex_native_input_projection.accepted_turns', return_value=[]), \
                patch.object(repair.time, 'monotonic', side_effect=[0, 1, 21]):
            result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting')
        self.assertEqual([method for method, _ in self.server.calls], ['thread/read'])
        self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
        self.assert_preserved_new_input()

    def test_changed_agent_connection_server_or_attempt_rejects_history_result(self):
        for change in ('epoch', 'threadId', 'accountKey', 'connection', 'server', 'attempt', 'stop'):
            with self.subTest(change=change):
                self.make_wait()
                def mutate():
                    if change == 'connection':
                        self.runtime.connection_ids['default'] = 'changed-connection'
                    elif change == 'server':
                        self.runtime.servers['default'] = object()
                    elif change == 'stop':
                        self.runtime.stop(self.a['id'])
                    else:
                        with self.runtime.lock, self.runtime.db() as db:
                            agent = self.runtime.agent(self.a['id'], db)
                            if change == 'attempt':
                                agent['startAttempt']['id'] = 'changed-attempt'
                            else:
                                agent[change] = agent['epoch'] + 1 if change == 'epoch' else 'changed-scope'
                            self.runtime.put(db, 'agents', agent)
                self.before_history = mutate
                result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertNotEqual(self.receipt(self.old_id)['status'], 'delivered')
                self.assertFalse(any(method == 'turn/start' for method, _ in self.server.calls))
                self.runtime.servers['default'] = self.server
                self.runtime.connection_ids['default'] = 'fixture-connection'
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(self.a['id'], db)
                    agent.update(epoch=self.a['epoch'], accountKey='default', threadId=self.tid,
                                 autoWake=True, nativeFailureHold=None, status='queued', inFlight=False)
                    self.runtime.put(db, 'agents', agent)

    def test_changed_old_receipt_or_new_pending_receipt_rejects_history_result(self):
        for change in ('old_metadata', 'old_status', 'new_status', 'new_text'):
            with self.subTest(change=change):
                self.make_wait()
                def mutate():
                    with self.runtime.db() as db:
                        if change == 'old_metadata':
                            db.execute("UPDATE runtime_event_meta SET record='{}' WHERE id=?", (self.old_id,))
                        elif change == 'old_status':
                            db.execute("UPDATE runtime_events SET status='reserved' WHERE id=?", (self.old_id,))
                        elif change == 'new_status':
                            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id=?", (self.new_id,))
                        else:
                            db.execute("UPDATE runtime_events SET text='changed input' WHERE id=?", (self.new_id,))
                self.before_history = mutate
                result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertNotEqual(self.receipt(self.old_id)['status'], 'delivered')
                self.assertEqual(self.runtime.agent(self.a['id'])['startAttempt'], self.attempt_before)
                self.assertTrue(self.runtime.agent(self.a['id']).get('contextRepairWait'))

    def test_stopped_or_held_agent_does_not_read_native_history(self):
        for change in ({'autoWake':False}, {'status':'paused'}, {'nativeFailureHold':True}, {'deletedAt':1}):
            with self.subTest(change=change):
                self.make_wait()
                agent = self.runtime.agent(self.a['id'])
                self.agent_update(agent, **change)
                result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertEqual(self.server.calls, [])
                self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
                self.agent_update(self.runtime.agent(self.a['id']), deletedAt=None)

    def test_exact_old_scope_and_native_thread_are_required(self):
        for scope in ('manifest', 'native', 'response'):
            with self.subTest(scope=scope):
                self.make_wait(metadata={'contextManifest':{'epoch':['another-thread', 1]}}
                    if scope == 'manifest' else {'native':{'agent':self.a['id'], 'epoch':self.a['epoch'],
                        'accountKey':'another-account', 'threadId':self.tid}} if scope == 'native' else {})
                self.native_thread = 'another-thread' if scope == 'response' else self.tid
                result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
                self.assertEqual(result['status'], 'waiting')
                self.assertEqual(self.receipt(self.old_id)['status'], 'uncertain')
                self.assert_preserved_new_input()


if __name__ == '__main__':
    suite = unittest.TestSuite(HistoricalInputWait(name) for name in HistoricalInputWait.__dict__
                              if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
