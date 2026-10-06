#!/usr/bin/env python3
"""Maintenance waits preserve accepted input and terminal unknown receipts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import copy
import importlib.util
import json
from pathlib import Path
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('actions', Path(__file__).with_name('native-action-context-repair-contract.py'))
f = importlib.util.module_from_spec(spec); spec.loader.exec_module(f)
repair, eventually = f.f.repair, f.f.f.eventually
from codex_account_transfer import transfer_store
from entity_test_support import context_repair_wait


class ContextWait(f.NativeActionRepair):
    def test_persistent_native_wait_escalates_once_to_lead(self):
        rt = self.runtime
        worker = rt.create({'name':'Repair wait worker', 'cwd':str(self.root),
            'prompt':'Keep the exact accepted input.', 'id':'a8ffea2c-7e91-4e6f-8790-20b6ed9372e1'},
            parent=self.a['id'], defer=True)
        message_id = 'repair-escalation-input'
        rt.send(worker['id'], 'Keep this exact input.', message_id=message_id)
        error = ValueError('Context repair waits for native notification delivery')
        with rt.lock, rt.db() as db:
            worker = rt.agent(worker['id'], db)
            attempt = {'id':'repair-escalation-attempt', 'events':[message_id],
                'epoch':worker['epoch'], 'accountKey':worker['accountKey'], 'submitted':False}
            worker['startAttempt'] = attempt
            error.contextRepairWait = {'scope':'native', 'source':repair._identity(worker)}
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id=?", (message_id,))
            for _ in range(7):
                self.assertTrue(repair._defer_context(rt, db, worker, error))
                worker = rt.agent(worker['id'], db)

            wait = worker['contextRepairWait']
            expected_id = 'context-repair-escalation:' + worker['id'] + ':' + attempt['id']
            self.assertEqual(wait['escalationEventId'], expected_id)
            rows = db.execute('SELECT id,status,text FROM runtime_events WHERE id=?',
                (expected_id,)).fetchall()
            self.assertEqual(len(rows), 1)
            self.assertIn('"checks": 5', rows[0]['text'])
            self.assertEqual(rows[0]['status'], 'pending')

    def test_transfer_retires_cancelled_wait_after_user_pause_without_replay(self):
        rt = self.runtime
        transfer_store(rt)
        rt.send(self.a['id'], 'Keep input cancelled by the pause', message_id='cancelled-transfer-wait')
        with rt.lock, rt.db() as db:
            agent = rt.agent(self.a['id'], db)
            attempt = {'id':'paused-attempt','events':['cancelled-transfer-wait'],
                       'epoch':agent['epoch'],'accountKey':'default','submitted':False}
            identity = repair._identity({**agent, 'startAttempt':attempt})
            agent.update(status='paused', autoWake=False, inFlight=False,
                         accountTransferId='cancelled-transfer', contextRepairWait=context_repair_wait(
                'Context repair waits for native notification delivery', source=identity,
                events=attempt['events']))
            agent.pop('startAttempt', None)
            # The live pause also advanced the epoch past the wait's events.
            agent['epoch'] += 1
            db.execute("UPDATE runtime_events SET status='cancelled' WHERE id='cancelled-transfer-wait'")
            rt.put(db, 'agents', agent)
            rt.put(db, 'account_transfers', {'id':'cancelled-transfer','leadId':agent['id'],
                'targetAccountKey':'target','status':'pending',
                'members':{agent['id']:{'phase':'waiting'}}})
        with rt.lock, rt.db() as db:
            self.assertIsNone(transfer_store(rt).local_blocker(db, agent))
            current = rt.agent(agent['id'], db)
            self.assertFalse(current.get('contextRepairWait'))
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                ('cancelled-transfer-wait',)).fetchone()[0], 'cancelled')

    def test_transfer_retires_proven_unsent_repair_wait(self):
        rt = self.runtime
        transfer_store(rt)
        rt.send(self.a['id'], 'Keep input', message_id='transfer-wait-input')
        with rt.lock, rt.db() as db:
            agent = rt.agent(self.a['id'], db)
            agent['startAttempt'] = {'id':'transfer-wait-start','events':['transfer-wait-input'],
                'epoch':agent['epoch'],'accountKey':'default','submitted':False}
            agent.update(status='starting', inFlight=True, accountTransferId='transfer-wait')
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id='transfer-wait-input'")
            rt.put(db, 'agents', agent)
            rt.put(db, 'account_transfers', {'id':'transfer-wait','leadId':agent['id'],
                'targetAccountKey':'target','status':'pending',
                'members':{agent['id']:{'phase':'waiting'}}})
        with self.assertRaisesRegex(ValueError, 'current agent operation') as caught:
            repair.repair_before_start(rt, agent)
        self.assertTrue(repair.defer_context_start(rt, agent['id'], 'transfer-wait-start', caught.exception))
        with rt.lock, rt.db() as db:
            agent = rt.agent(self.a['id'], db)
            self.assertIsNone(transfer_store(rt).local_blocker(db, agent))
        current = rt.agent(self.a['id'])
        self.assertFalse(current.get('contextRepairWait'))
        with rt.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                ('transfer-wait-input',)).fetchone()[0], 'pending')

    def setUp(self):
        super().setUp()
        submit = self.server.submit
        def call(method, params):
            if method != 'turn/start':
                return submit(method, params)
            self.server.calls.append((method, copy.deepcopy(params)))
            future = concurrent.futures.Future()
            future.set_result({'turn':{'id':'resumed-turn','status':'inProgress'}})
            return 'resumed-rpc', method, future
        self.server.submit = call

    def put(self, table, value):
        with self.runtime.db() as db:
            self.runtime.put(db, table, value)

    def monitor(self):
        self.put('monitors', {'id':'exact-active-monitor','agent':self.a['id'],'status':'running'})

    def clear_monitor(self):
        self.put('monitors', {'id':'exact-active-monitor','agent':self.a['id'],'status':'completed'})

    def uncertain_input(self, message_id, turns=None, *, unreadable=False):
        self.runtime.send(self.a['id'], 'Keep this accepted input.', message_id=message_id)
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='uncertain',error='Native response was lost' WHERE id=?", (message_id,))
        attempt = {'id':'uncertain-attempt', 'epoch':self.a['epoch'], 'threadId':self.tid,
                   'accountKey':'default', 'events':[message_id], 'submitted':False}
        self.agent_update(self.a, status='queued', inFlight=False, startAttempt=attempt,
            contextRepairWait=context_repair_wait(
                'Context repair waits for a confirmed input receipt: ' + message_id,
                source=repair._identity({**self.a,'startAttempt':attempt}), events=[message_id]))
        original = self.server.call
        def call(method, params, timeout=10):
            if method == 'thread/read':
                if unreadable:
                    raise RuntimeError('history read is offline')
                return {'thread':{'id':self.tid,'status':{'type':'idle'},'path':str(self.path)}}
            if method == 'thread/turns/list':
                return {'data':turns or [], 'nextCursor':None}
            return original(method, params, timeout)
        self.server.call = call

    def due(self):
        a = self.runtime.agent(self.a['id'])
        a['contextRepairWait']['nextCheckAt'] = 0
        self.agent_update(a, contextRepairWait=a['contextRepairWait'])
        self.runtime.dispatch()

    def test_active_monitor_does_not_block_normal_input_after_compaction(self):
        source = self.path.read_bytes()
        self.agent_update(self.a, compactions=1, contextRepair={
            'phase':'unchanged', 'source':{'threadId':self.tid},
            'compactions':0, 'checkedEventIds':[self.event['id']]})
        self.monitor()
        self.runtime.send(self.a['id'], 'Keep one exact request.', message_id='wait-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertFalse(current.get('contextRepairWait'))
        self.assertEqual(current['threadId'], self.tid)
        self.assertEqual(self.forks(), [])
        self.assertEqual(source, self.path.read_bytes())
        self.runtime.dispatch()
        starts = [p for m,p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'wait-user')
        with self.runtime.db() as db:
            monitor = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?',
                                          ('exact-active-monitor',)).fetchone()[0])
        self.assertEqual(monitor['status'], 'running')

    def test_existing_monitor_wait_resumes_same_input_without_fork(self):
        self.monitor()
        self.runtime.send(self.a['id'], 'Keep one exact request.', message_id='wait-user')
        # Reproduce the pre-update mandatory maintenance gate.
        with patch.object(repair, '_optional_monitor_repair', return_value=False):
            self.runtime.dispatch()
            eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        waiting = self.runtime.agent(self.a['id'])
        self.assertEqual(waiting['status'], 'queued')
        self.assertIn('monitors: exact-active-monitor', waiting['error'])
        attempt_id = waiting['startAttempt']['id']
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['id'], attempt_id)
        self.assertEqual(current['startAttempt']['events'], ['wait-user'])
        self.assertEqual(len(self.forks()), 0)
        self.assertEqual(current['threadId'], self.tid)
        starts = [p for m,p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'wait-user')

    def test_recover_marks_native_message_delivered_with_exact_turn(self):
        self.uncertain_input('recover-present', [{'id':'native-turn','clientUserMessageId':'recover-present'}])
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'], [{'id':'recover-present','decision':'delivered','turnId':'native-turn'}])
        with self.runtime.db() as db:
            row = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',('recover-present',)).fetchone()
        self.assertEqual(tuple(row), ('delivered','native-turn'))

    def test_restart_held_input_wait_rechecks_native_history_automatically(self):
        self.uncertain_input('restart-input', [{'id':'native-turn','clientUserMessageId':'restart-input'}])
        a = self.runtime.agent(self.a['id'])
        attempt = copy.deepcopy(a['startAttempt'])
        wait = copy.deepcopy(a['contextRepairWait'])
        wait.update(error='Native input submission has no confirmed turn identity.', events=['restart-input'])
        recovery = {'epoch':a['epoch'], 'accountKey':'default', 'threadId':self.tid,
                    'turnId':None, 'stage':'held', 'autoWake':True, 'startAttempt':attempt}
        a.update(status='interrupted', autoWake=False, inFlight=False, nativeFailureHold=True,
                 contextRepairWait=wait, restartRecovery=recovery)
        self.agent_update(a, status=a['status'], autoWake=False, inFlight=False,
                          nativeFailureHold=True, contextRepairWait=wait, restartRecovery=recovery)
        self.runtime.dispatch()
        eventually(lambda: not self.runtime.agent(self.a['id']).get('contextRepairWait'))
        current = self.runtime.agent(self.a['id'])
        self.assertTrue(current['autoWake'])
        self.assertEqual(current['turnId'], 'native-turn')
        with self.runtime.db() as db:
            receipt = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                                 ('restart-input',)).fetchone()
        self.assertEqual(tuple(receipt), ('delivered', 'native-turn'))

    def test_restart_held_absent_input_is_retried_once_with_original_identity(self):
        self.uncertain_input('restart-absent', [{'id':'other-turn','status':'interrupted',
            'items':[{'type':'userMessage','clientId':'another-input'}]}])
        self.records.extend([
            {'type':'turn_context','payload':{'turn_id':'other-turn'}},
            {'type':'event_msg','payload':{'type':'turn_aborted','turn_id':'other-turn'}},
        ])
        self.write_records()
        with self.runtime.db() as db:
            db.execute('INSERT OR IGNORE INTO runtime_completed_turns VALUES (?)',
                       (self.a['id'] + ':other-turn',))
        a = self.runtime.agent(self.a['id'])
        attempt = copy.deepcopy(a['startAttempt'])
        wait = copy.deepcopy(a['contextRepairWait'])
        wait.update(error='Native input submission has no confirmed turn identity.', events=['restart-absent'])
        recovery = {'epoch':a['epoch'], 'accountKey':'default', 'threadId':self.tid,
                    'turnId':None, 'stage':'held', 'autoWake':True, 'startAttempt':attempt}
        self.agent_update(a, status='interrupted', autoWake=False, inFlight=False,
                          contextRepairWait=wait, restartRecovery=recovery)
        self.runtime.dispatch()
        eventually(lambda: not self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.assertEqual(self.runtime.agent(self.a['id'])['status'], 'queued')
        self.assertTrue(self.runtime.agent(self.a['id'])['autoWake'])
        self.runtime.dispatch()
        try:
            eventually(lambda: any(method == 'turn/start' for method, _ in self.server.calls))
        except AssertionError:
            with self.runtime.db() as db:
                receipt = db.execute('SELECT status FROM runtime_events WHERE id=?', ('restart-absent',)).fetchone()
            self.fail(f"retry did not submit original input: agent={self.runtime.agent(self.a['id'])!r} event={receipt[0]}")
        for _ in range(2):
            self.runtime.dispatch()
        starts = [p for method, p in self.server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'restart-absent')

    def test_held_restart_marker_resolves_old_input_and_dispatches_new_wait_once(self):
        self.uncertain_input('restart-original', [
            {'id':'original-native-turn','clientUserMessageId':'restart-original'}])
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET error='Codex disconnected' WHERE id='restart-original'")
            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                ('newer-input', self.a['id'], 'user', 'Resume with this newer message.',
                 'pending', time.time(), self.a['epoch'], None, None))
        newer_attempt = {'id':'newer-attempt', 'epoch':self.a['epoch'], 'threadId':self.tid,
                         'accountKey':'default', 'events':['newer-input'], 'submitted':False}
        wait_source = {**repair._identity({**self.a, 'startAttempt':newer_attempt}),
                       'attemptId':'older-wait-attempt'}
        self.agent_update(self.a, status='queued', autoWake=True, inFlight=False,
            startAttempt=newer_attempt, error='Context repair waits for the existing native recovery receipt',
            contextRepairWait=context_repair_wait(
                'Context repair waits for the existing native recovery receipt',
                source=wait_source, events=['newer-input'], checks=1),
            restartRecovery={'epoch':self.a['epoch'], 'accountKey':'default', 'threadId':self.tid,
                'turnId':None, 'stage':'held', 'reason':'Native input submission has no confirmed turn identity.',
                'autoWake':True, 'startAttempt':None})

        history_entered, allow_history = threading.Event(), threading.Event()
        original_call = self.server.call
        def blocked_history(method, params, timeout=10):
            if method == 'thread/read':
                history_entered.set()
                if not allow_history.wait(5):
                    raise TimeoutError('fixture history read was not released')
            return original_call(method, params, timeout)
        self.server.call = blocked_history
        self.runtime.dispatch_all()
        self.assertTrue(history_entered.wait(2), 'restart history check did not start')
        with self.runtime.db() as db:
            initial_wait = self.runtime.agent(self.a['id'], db)['contextRepairWait']
        check_id = initial_wait['historyCheckId']
        for _ in range(3):
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(self.a['id'], db)
                current['contextRepairWait']['nextCheckAt'] = 0
                self.runtime.put(db, 'agents', current)
            self.runtime.dispatch_all()
            current_wait = self.runtime.agent(self.a['id'])['contextRepairWait']
            self.assertEqual(current_wait.get('historyCheckId'), check_id)
            self.assertEqual(current_wait.get('historyCheckAt'), initial_wait['historyCheckAt'])
            self.assertEqual(current_wait.get('checks'), 1)
            self.assertIsNone(current_wait.get('lastHistoryCheck'))
        allow_history.set()
        eventually(lambda: not self.runtime.agent(self.a['id']).get('contextRepairWait'))
        settled = self.runtime.agent(self.a['id'])
        self.assertEqual(settled['restartRecovery']['stage'], 'finished')
        self.assertEqual(settled['startAttempt']['id'], 'newer-attempt')
        with self.runtime.db() as db:
            old_receipt = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                                     ('restart-original',)).fetchone()
            newer_status = db.execute('SELECT status FROM runtime_events WHERE id=?',
                                      ('newer-input',)).fetchone()[0]
        self.assertEqual(tuple(old_receipt), ('delivered', 'original-native-turn'))
        self.assertEqual(newer_status, 'pending')

        with patch.object(repair, 'repair_before_start', side_effect=lambda _rt, agent: agent):
            self.runtime.dispatch()
            try:
                eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
            except AssertionError:
                with self.runtime.db() as db:
                    statuses = [tuple(row) for row in db.execute(
                        'SELECT id,status,error FROM runtime_events WHERE agent=? ORDER BY created,id',
                        (self.a['id'],)).fetchall()]
                self.fail(f"newer input did not dispatch: agent={self.runtime.agent(self.a['id'])!r} "
                          f"events={statuses!r} calls={self.server.calls!r}")
        for _ in range(2):
            self.runtime.dispatch()
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'newer-input')
        with self.runtime.db() as db:
            old_receipt = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                                     ('restart-original',)).fetchone()
            newer_receipt = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                                       ('newer-input',)).fetchone()
        self.assertEqual(tuple(old_receipt), ('delivered', 'original-native-turn'))
        self.assertEqual(tuple(newer_receipt), ('delivered', 'resumed-turn'))

    def test_held_restart_marker_without_unconfirmed_input_dispatches_pending_once(self):
        with self.runtime.db() as db:
            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                ('held-pending', self.a['id'], 'user', 'Continue after restart.',
                 'pending', time.time(), self.a['epoch'], None, None))
        attempt = {'id':'held-attempt', 'epoch':self.a['epoch'], 'accountKey':'default',
                   'events':['held-pending'], 'submitted':False}
        error = 'Context repair waits for the existing native recovery receipt'
        self.agent_update(self.a, status='queued', autoWake=True, inFlight=False, startAttempt=attempt,
            contextRepairWait=context_repair_wait(error,
                source={**repair._identity(self.a), 'attemptId':'held-attempt'},
                events=['held-pending'], checks=9,
                lastHistoryCheck='The unconfirmed input is outside the current start attempt'),
            restartRecovery={'epoch':self.a['epoch'], 'accountKey':'default', 'threadId':self.tid,
                'turnId':None, 'stage':'held', 'reason':'Native input submission has no confirmed turn identity.',
                'autoWake':True, 'startAttempt':None})
        self.runtime.dispatch_all()
        eventually(lambda: not self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.assertEqual(self.runtime.agent(self.a['id'])['restartRecovery']['stage'], 'finished')
        with patch.object(repair, 'repair_before_start', side_effect=lambda _rt, agent: agent):
            self.runtime.dispatch()
            eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        for _ in range(2):
            self.runtime.dispatch()
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual([p['clientUserMessageId'] for p in starts], ['held-pending'])

    def test_recover_requeues_absent_idle_input_once(self):
        self.uncertain_input('recover-absent', [{'id':'other-turn',
            'items':[{'type':'userMessage','clientId':'other-input'}]}])
        first = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        second = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(first['inputs'], [{'id':'recover-absent','decision':'not_delivered'}])
        self.assertEqual(second['status'], 'not_needed')
        with self.runtime.db() as db:
            row = db.execute('SELECT status FROM runtime_events WHERE id=?',('recover-absent',)).fetchone()
        self.assertEqual(row[0], 'pending')

    def test_preparing_input_is_not_an_acceptance_receipt(self):
        self.uncertain_input('preparing-input', [{'id':'preparing-turn',
            'startOutcome':'preparing', 'clientUserMessageId':'preparing-input'}])
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'][0]['decision'], 'waiting')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                ('preparing-input',)).fetchone()[0], 'uncertain')

    def test_submitted_absent_input_stays_uncertain_on_an_idle_connection(self):
        self.uncertain_input('submitted-input', [{'id':'other-turn',
            'clientUserMessageId':'another-input'}])
        a = self.runtime.agent(self.a['id'])
        attempt = {**a['startAttempt'], 'submitted':True, 'executionOutcome':'unknown'}
        wait = {**a['contextRepairWait'], 'source':repair._identity({**a, 'startAttempt':attempt})}
        self.agent_update(a, startAttempt=attempt, contextRepairWait=wait)
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'], [{'id':'submitted-input','decision':'waiting'}])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                ('submitted-input',)).fetchone()[0], 'uncertain')

    def test_exact_native_rejection_can_requeue_a_submitted_input(self):
        self.uncertain_input('rejected-input', [{'id':'rejected-turn',
            'startOutcome':'not_applied', 'clientUserMessageId':'rejected-input'}])
        a = self.runtime.agent(self.a['id'])
        attempt = {**a['startAttempt'], 'submitted':True}
        wait = {**a['contextRepairWait'], 'source':repair._identity({**a, 'startAttempt':attempt})}
        self.agent_update(a, startAttempt=attempt, contextRepairWait=wait)
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'], [{'id':'rejected-input','decision':'not_delivered'}])

    def test_newer_unsent_attempt_does_not_allow_replay_of_held_submitted_input(self):
        self.uncertain_input('old-submitted', [{'id':'other-turn','clientUserMessageId':'another-input'}])
        a = self.runtime.agent(self.a['id'])
        held = {**a['startAttempt'], 'submitted':True}
        newer = {**a['startAttempt'], 'id':'newer-attempt', 'events':['newer-input'], 'submitted':False}
        recovery = {'epoch':a['epoch'], 'accountKey':'default', 'threadId':self.tid,
                    'turnId':None, 'stage':'held', 'autoWake':True, 'startAttempt':held}
        self.agent_update(a, startAttempt=newer, restartRecovery=recovery)
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'], [{'id':'old-submitted','decision':'waiting'}])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                ('old-submitted',)).fetchone()[0], 'uncertain')

    def test_accepted_retry_receipt_takes_precedence_over_preparation(self):
        self.uncertain_input('accepted-retry', [
            {'id':'accepted-turn','startOutcome':'accepted','clientUserMessageId':'accepted-retry'},
            {'id':'old-turn','startOutcome':'preparing','clientUserMessageId':'accepted-retry'}])
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'], [{'id':'accepted-retry','decision':'delivered','turnId':'accepted-turn'}])

    def test_unreadable_history_keeps_input_uncertain_and_waiting(self):
        self.uncertain_input('recover-offline', unreadable=True)
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['status'], 'waiting')
        self.assertIn('offline', result['reason'])
        with self.runtime.db() as db:
            row = db.execute('SELECT status FROM runtime_events WHERE id=?',('recover-offline',)).fetchone()
        self.assertEqual(row[0], 'uncertain')
        self.assertTrue(self.runtime.agent(self.a['id']).get('contextRepairWait'))

    def test_native_client_id_confirms_delivered_input(self):
        self.uncertain_input('recover-client', [{'id':'native-turn','status':'completed',
            'items':[{'id':'native-item','type':'userMessage','clientId':'recover-client','content':[]}]}])
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'][0]['decision'], 'delivered')
        with self.runtime.db() as db:
            row = db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?', ('recover-client',)).fetchone()
        self.assertEqual(tuple(row), ('delivered', 'native-turn'))

    def test_missing_identity_keeps_input_uncertain(self):
        self.uncertain_input('unsupported-id', [{'id':'native-turn','status':'completed',
            'items':[{'id':'unsupported-id','type':'userMessage','content':[]}]}])
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'][0]['decision'], 'waiting')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                ('unsupported-id',)).fetchone()[0], 'uncertain')

    def test_recovery_does_not_restore_completed_turn_activity(self):
        self.uncertain_input('recover-completed',
            [{'id':'native-turn','status':'completed','clientUserMessageId':'recover-completed'}])
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)', (self.a['id'] + ':native-turn',))
        result = repair.recover_unconfirmed_inputs(self.runtime, self.a['id'])
        self.assertEqual(result['inputs'][0]['decision'], 'delivered')
        current = self.runtime.agent(self.a['id'])
        self.assertFalse(current['inFlight'])
        self.assertNotEqual(current['status'], 'running')
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(self.a['id'], db)
            current.update(status='running', inFlight=True, turnId='native-turn')
            self.runtime.put(db, 'agents', current)
        self.assertEqual(self.runtime.reconcile_turn(self.a['id'])['status'], 'reconciled')
        current = self.runtime.agent(self.a['id'])
        self.assertFalse(current['inFlight'])
        self.assertIsNone(current['turnId'])
        self.assertEqual(current['status'], 'completed')

    def command(self, **changes):
        record = {'id':'exact-command', 'agent':self.a['id'], 'kind':'command',
                  'type':'commandExecution', 'status':'running', 'processId':'27427'}
        record.update(changes)
        self.put('tasks', record)
        return record

    def test_active_command_does_not_block_normal_input_or_change_its_receipt(self):
        command = self.command()
        source = self.path.read_bytes()
        self.runtime.send(self.a['id'], 'Read the existing process result.', message_id='command-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertFalse(current.get('contextRepairWait'))
        self.assertEqual(current['threadId'], self.tid)
        self.assertEqual(self.forks(), [])
        self.assertEqual(source, self.path.read_bytes())
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_tasks WHERE id=?', (command['id'],)).fetchone()[0])
        self.assertEqual(saved, command)
        self.runtime.dispatch()
        starts = [p for m,p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'command-user')

    def test_existing_command_wait_resumes_same_attempt(self):
        self.command()
        self.runtime.send(self.a['id'], 'Continue with the existing command.', message_id='command-user')
        with patch.object(repair, '_optional_monitor_repair', return_value=False):
            self.runtime.dispatch()
            eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        waiting = self.runtime.agent(self.a['id'])
        self.assertIn('tasks: exact-command', waiting['error'])
        attempt_id = waiting['startAttempt']['id']
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['id'], attempt_id)
        self.assertEqual(current['threadId'], self.tid)
        self.assertEqual(self.forks(), [])
        self.assertEqual(len([m for m,p in self.server.calls if m == 'turn/start']), 1)

    def test_optional_command_path_preserves_unknown_and_tool_holds(self):
        self.runtime.send(self.a['id'], 'Preserve uncertain operations.', message_id='unknown-command-user')
        # Claim the start synchronously to inspect the same pre-submission guard.
        self.command(status='unknown')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        for changes in ({'status':'unknown'}, {'status':'pending'}, {'processId':None}, {'kind':'tool'}):
            with self.subTest(changes=changes):
                self.command(**changes)
                self.due()
                self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))
                self.assertEqual(self.forks(), [])
        self.command()
        self.put('requests', {'id':'approval','agent':self.a['id'],'status':'pending',
                              'method':'item/commandExecution/requestApproval'})
        self.due()
        self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))

    def test_optional_monitor_path_preserves_uncertain_input_gate(self):
        self.legacy_uncertain()
        self.monitor()
        self.runtime.send(self.a['id'], 'Preserve all receipts.', message_id='uncertain-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.due()
        self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))
        self.assertEqual(self.forks(), [])

    def test_optional_monitor_path_preserves_other_local_blockers(self):
        self.monitor()
        self.runtime.send(self.a['id'], 'Wait for required approval.', message_id='approval-user')
        self.put('requests', {'id':'approval','agent':self.a['id'],'status':'pending',
                              'method':'item/commandExecution/requestApproval'})
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.due()
        self.assertFalse(any(m == 'turn/start' for m,p in self.server.calls))
        self.assertEqual(self.forks(), [])

    def test_async_question_survives_fork_and_answer_uses_current_thread_once(self):
        question = {'id':'question-exact','agent':self.a['id'],'epoch':self.a['epoch'],
            'accountKey':self.a['accountKey'],'method':'agent/asyncQuestion','status':'pending',
            'params':{'threadId':self.tid,'questions':[{'id':'color','question':'Choose the color.'}]}}
        self.put('requests',question)
        self.runtime.send(self.a['id'],'Continue while the question is pending.',message_id='question-continue')
        self.runtime.dispatch()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='resumed-turn')
        a=self.runtime.agent(self.a['id'])
        with self.runtime.db() as db:
            saved=json.loads(db.execute('SELECT record FROM runtime_requests WHERE id=?',(question['id'],)).fetchone()[0])
        self.assertEqual(saved,question)
        self.assertNotEqual(a['threadId'],self.tid)
        self.agent_update(a,status='idle',inFlight=False,turnId=None)
        answer={'answers':{'color':{'answers':['Green']}}}
        self.assertEqual(self.runtime.answer(question['id'],answer),{'status':'answered'})
        self.assertTrue(self.runtime.answer(question['id'],answer)['replayed'])
        self.runtime.dispatch()
        eventually(lambda:len([m for m,p in self.server.calls if m=='turn/start'])==2)
        answers=[p for m,p in self.server.calls if m=='turn/start' and p.get('clientUserMessageId')=='question-exact:answer']
        self.assertEqual(len(answers),1)
        self.assertEqual(answers[0]['threadId'],a['threadId'])
        self.assertIn('Green',json.dumps(answers[0]['input']))
        self.assertEqual(len(self.forks()),1)

    def test_native_blocking_requests_still_prevent_repair(self):
        for method in ('item/tool/requestUserInput','item/commandExecution/requestApproval','unknown/request'):
            with self.subTest(method=method):
                record={'id':'blocking-exact','agent':self.a['id'],'method':method,'status':'pending'}
                self.put('requests',record)
                with self.assertRaisesRegex(ValueError,'requests: blocking-exact'):
                    repair.repair_idle(self.runtime,self.a['id'])
                self.assertEqual(self.forks(),[])

    def test_failed_tool_response_preserves_unknown_outcome(self):
        record = {'id':'failed-tool','agent':self.a['id'],'stage':'failed','outcome':'unknown',
                  'finished':1,'result':{'success':False,'contentItems':[{'type':'inputText','text':'Unknown command watch'}]}}
        self.put('tool_requests', record)
        self.assertEqual(repair.repair_idle(self.runtime, self.a['id'])['contextRepair']['phase'], 'completed')
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(record['id'],db), record)

    def test_native_history_timeout_defers_exact_start_then_resumes_once(self):
        from codex_runtime import ResponseTimeout
        original = self.server.call
        unavailable = [True]
        def call(method, params, timeout=10):
            if unavailable[0] and method == 'thread/items/list':
                raise ResponseTimeout('thread/items/list response timed out; outcome unknown')
            return original(method, params, timeout)
        self.server.call = call
        self.runtime.send(self.a['id'], 'Preserve accepted input.', message_id='read-timeout-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        a = self.runtime.agent(self.a['id'])
        self.assertEqual(a['status'], 'queued')
        self.assertFalse(a['inFlight'])
        self.assertFalse(a['startAttempt']['submitted'])
        attempt_id = a['startAttempt']['id']
        self.assertIn('native history read (thread/items/list)', a['error'])
        self.assertEqual(self.forks(), [])
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        self.assertEqual(self.runtime.agent(self.a['id'])['contextRepairWait']['checks'], 2)
        self.assertEqual(self.runtime.agent(self.a['id']).get('contextRepairHistory', []), [])
        unavailable[0] = False
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        a = self.runtime.agent(self.a['id'])
        self.assertEqual(a['startAttempt']['id'], attempt_id)
        self.assertEqual(a['startAttempt']['events'], ['read-timeout-user'])
        self.assertEqual(len(self.forks()), 1)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'turn/start']), 1)

    def test_interrupted_receipt_requires_exact_old_turn_native_terminal_item(self):
        record = {'id':'missing-receipt','agent':self.a['id'],'stage':'interrupted','outcome':'unknown',
                  'threadId':self.tid,'accountKey':self.a['accountKey'],'turnId':'old-turn','callId':'old-call'}
        self.put('tool_requests', record)
        original = self.server.call
        native_status = ['inProgress']
        def call(method, params, timeout=10):
            if method == 'thread/items/list' and params['turnId'] == 'old-turn':
                self.server.calls.append((method,copy.deepcopy(params)))
                return {'data':[{'turnId':'old-turn','item':{'type':'dynamicToolCall','id':'old-call',
                                 'status':native_status[0],'success':False}}]}
            return original(method,params,timeout)
        self.server.call = call
        with self.assertRaisesRegex(ValueError, 'exact tool receipt: missing-receipt'):
            repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(self.forks(), [])
        native_status[0] = 'failed'
        repair.repair_idle(self.runtime,self.a['id'])
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(record['id'],db), record)
        self.assertTrue(any(p.get('turnId') == 'old-turn' for m,p in self.server.calls if m == 'thread/items/list'))

    def test_foreign_receipt_never_uses_same_call_id_on_current_thread(self):
        record = {'id':'foreign-receipt','agent':self.a['id'],'stage':'interrupted','outcome':'unknown',
                  'threadId':'old-native','accountKey':'other-account','turnId':'turn','callId':'same-call'}
        self.put('tool_requests', record)
        self.server.items = [{'item':{'type':'dynamicToolCall','id':'same-call','status':'failed','success':False}}]
        with self.assertRaisesRegex(ValueError, 'original account/thread receipt'):
            repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(self.forks(), [])
        self.agent_update(self.a, accountHistory=[{'threadId':'old-native','accountKey':'other-account'}])
        repair.repair_idle(self.runtime,self.a['id'])
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.tool_request(record['id'],db), record)

    def test_paged_native_items_reject_pending_on_later_page(self):
        original = self.server.call
        terminal = [False]
        def call(method,params,timeout=10):
            if method == 'thread/items/list':
                self.server.calls.append((method,copy.deepcopy(params)))
                if not params.get('cursor'):
                    return {'data':[{'turnId':'turn','item':{'id':str(i),'type':'reasoning'}} for i in range(1000)], 'nextCursor':'page-2'}
                self.assertEqual(params['cursor'],'page-2')
                return {'data':[{'turnId':'turn','item':{'id':'last-tool','type':'dynamicToolCall',
                    'status':'failed' if terminal[0] else 'inProgress','success':False}}]}
            return original(method,params,timeout)
        self.server.call = call
        with self.assertRaisesRegex(ValueError, 'complete native tool receipts'):
            repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(self.forks(), [])
        terminal[0] = True
        repair.repair_idle(self.runtime,self.a['id'])
        self.assertEqual(len(self.forks()), 1)

    def test_missing_terminal_tail_defers_without_unloading_or_forking(self):
        terminal = self.records.pop()
        self.write_records()
        self.runtime.send(self.a['id'], 'Preserve the latest turn.', message_id='tail-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('contextRepairWait'))
        a = self.runtime.agent(self.a['id'])
        self.assertIn('saved terminal turn: turn', a['error'])
        attempt_id = a['startAttempt']['id']
        self.assertFalse(a['startAttempt']['submitted'])
        self.assertEqual(self.forks(), [])
        self.assertFalse(any(m == 'thread/unsubscribe' for m,p in self.server.calls))
        self.records.append(terminal)
        self.write_records()
        self.due()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        self.assertEqual(self.runtime.agent(a['id'])['startAttempt']['id'], attempt_id)
        self.assertEqual(len(self.forks()), 1)

    def test_exact_unchanged_repair_preparation_failure_is_recovered(self):
        error = 'Thread preparation belongs to an earlier agent state'
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('unchanged-user',self.a['id'],'user','Keep this','failed',2,self.a['epoch'],None,error))
        a = self.agent_update(self.a,status='failed',error=error,inFlight=False,startAttempt={
            'id':'unchanged-attempt','submitted':False,'events':['unchanged-user'],
            'epoch':self.a['epoch'],'accountKey':self.a['accountKey']})
        receipt = {'id':'unchanged-operation','phase':'unchanged','source':repair._identity(a),
                   'settings':self.runtime.preparation_settings(a),'snapshot':{'eventIds':[]}}
        a = self.agent_update(a,contextRepair=receipt)
        with self.runtime.lock,self.runtime.db() as db:
            repair.recover_context_failures(self.runtime,db,[a])
        self.assertTrue(self.runtime.agent(a['id'])['contextRepairWait']['historicalFailureRecovered'])
        self.due()
        eventually(lambda:self.runtime.agent(a['id']).get('turnId')=='resumed-turn')
        self.assertEqual(self.runtime.agent(a['id'])['startAttempt']['id'],'unchanged-attempt')

    def test_old_source_cleanup_unknown_never_repeats_fork_or_input(self):
        original = self.server.submit
        cleanup = concurrent.futures.Future()
        def submit(method, params):
            if method == 'thread/unsubscribe':
                self.server.calls.append((method, copy.deepcopy(params)))
                self.assertEqual(params['threadId'], self.tid)
                self.assertNotEqual(self.runtime.agent(self.a['id'])['threadId'], self.tid)
                return 'source-cleanup-rpc', method, cleanup
            return original(method, params)
        self.server.submit = submit
        self.runtime.send(self.a['id'], 'Keep the successful fork.', message_id='cleanup-user')
        self.runtime.dispatch()
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'resumed-turn')
        eventually(lambda: (self.runtime.agent(self.a['id'])['contextRepair'].get('sourceCleanup') or {}).get('requestId'))
        cleanup.set_exception(RuntimeError('Cleanup disconnected; outcome unknown'))
        a = self.runtime.agent(self.a['id'])
        self.assertEqual(a['contextRepair']['phase'], 'completed')
        self.assertEqual(a['contextRepair']['sourceCleanup']['phase'], 'unknown')
        self.assertEqual(a['turnId'], 'resumed-turn')
        self.runtime.dispatch()
        self.assertEqual(len(self.forks()), 1)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'thread/start']), 0)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'turn/start']), 1)
        self.assertEqual(len([m for m,p in self.server.calls if m == 'thread/unsubscribe']), 1)

    def legacy_uncertain(self, count=1):
        rows = [('legacy-offline-'+str(i),self.a['id'],'agent_message','Preserve unknown receipt '+str(i),
                 'uncertain',2,self.a['epoch'],None,'Codex app-server is offline') for i in range(count)]
        with self.runtime.db() as db:
            db.executemany('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',rows)
            db.execute('INSERT OR IGNORE INTO runtime_completed_turns VALUES (?)',(self.a['id']+':turn',))
        self.records[-1]['payload']['completed_at'] = 3
        self.write_records()
        return rows

    def test_21_historical_offline_receipts_stay_unknown_and_are_not_replayed(self):
        original = self.legacy_uncertain(21)
        self.runtime.send(self.a['id'],'Submit only the new input.',message_id='new-after-offline')
        self.runtime.dispatch()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='resumed-turn')
        a = self.runtime.agent(self.a['id'])
        proof = a['contextRepair']['historicalInputProof']
        self.assertEqual(len(proof['events']),21)
        self.assertEqual(proof['deliveryOutcome'],'unknown')
        self.assertEqual(proof['terminalTurnId'],'turn')
        with self.runtime.db() as db:
            for row in original:
                self.assertEqual(tuple(db.execute('SELECT * FROM runtime_events WHERE id=?',(row[0],)).fetchone()),row)
        starts = [p for m,p in self.server.calls if m=='turn/start']
        self.assertEqual(len(starts),1)
        self.assertNotIn('Preserve unknown receipt',json.dumps(starts))
        self.assertEqual(a['startAttempt']['events'],['new-after-offline'])

    def test_current_or_identified_uncertain_input_still_blocks(self):
        rows = self.legacy_uncertain()
        a = self.agent_update(self.a,startAttempt={'id':'safe-attempt','submitted':False,'events':[]})
        for mutation in ('metadata','current','other-error','known-turn','reserved','dispatching'):
            with self.subTest(mutation=mutation):
                with self.runtime.db() as db:
                    db.execute('DELETE FROM runtime_event_meta WHERE id=?',(rows[0][0],))
                    db.execute("UPDATE runtime_events SET error=?,turn_id=NULL,status='uncertain' WHERE id=?",('Codex app-server is offline',rows[0][0]))
                    if mutation=='metadata':
                        db.execute('INSERT INTO runtime_event_meta VALUES (?,?)',(rows[0][0],json.dumps({'native':{
                            'connectionId':self.runtime.connection_ids['default'],'threadId':self.tid}})))
                    elif mutation=='other-error':
                        db.execute('UPDATE runtime_events SET error=? WHERE id=?',('Native response timed out; outcome unknown',rows[0][0]))
                    elif mutation=='known-turn':
                        db.execute('UPDATE runtime_events SET turn_id=? WHERE id=?',('turn',rows[0][0]))
                    elif mutation in {'reserved','dispatching'}:
                        db.execute('UPDATE runtime_events SET status=? WHERE id=?',(mutation,rows[0][0]))
                a=self.agent_update(a,startAttempt={'id':'safe-attempt','submitted':False,
                    'events':[rows[0][0]] if mutation=='current' else []})
                with self.assertRaisesRegex(ValueError,'confirmed input receipt'):
                    repair.repair_idle(self.runtime,a['id'])
                self.assertEqual(self.forks(),[])

    def test_historical_receipts_need_later_saved_and_observed_terminal_without_events(self):
        rows = self.legacy_uncertain()
        a=self.agent_update(self.a,startAttempt={'id':'safe-attempt','submitted':False,'events':[]})
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_event_meta VALUES (?,?)',(self.event['id'],json.dumps({'modelEventProjection':1})))
            db.execute('DELETE FROM runtime_completed_turns')
        with self.assertRaisesRegex(ValueError,'exact terminal callback receipt'):
            repair.repair_idle(self.runtime,a['id'])
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)',(a['id']+':turn',))
        self.records[-1]['payload']['completed_at']=1
        self.write_records()
        with self.assertRaisesRegex(ValueError,'later confirmed native terminal'):
            repair.repair_idle(self.runtime,a['id'])
        self.records[-1]['payload']['completed_at']=3
        self.write_records()
        result=repair.repair_idle(self.runtime,a['id'])
        self.assertEqual(result['contextRepair']['phase'],'unchanged')
        self.assertEqual(result['contextRepair']['historicalInputProof']['events'][0]['id'],rows[0][0])
        count=len(self.server.calls)
        repair.repair_idle(self.runtime,a['id'])
        self.assertEqual(len(self.server.calls),count)
        self.assertEqual(self.forks(),[])

    def test_existing_failed_exact_batch_is_recovered_without_new_ids(self):
        error = 'Context repair waits for commands, monitors, and tool receipts'
        ids = ['old-user-' + str(i) for i in range(19)]
        with self.runtime.db() as db:
            for key in ids:
                db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                    (key,self.a['id'],'user','Original '+key,'failed',2,self.a['epoch'],None,error))
        self.agent_update(self.a,status='failed',error=error,inFlight=False,startAttempt={
            'id':'old-unsent','submitted':False,'events':ids,'epoch':self.a['epoch'],'accountKey':self.a['accountKey']})
        self.runtime.dispatch()
        waiting = self.runtime.agent(self.a['id'])
        self.assertEqual(waiting['status'],'queued')
        self.assertTrue(waiting['contextRepairWait']['historicalFailureRecovered'])
        self.assertEqual(waiting['startAttempt']['id'],'old-unsent')
        self.due()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='resumed-turn')
        current=self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['events'],ids)
        self.assertEqual(current['startAttempt']['id'],'old-unsent')
        self.assertEqual(len([m for m,p in self.server.calls if m=='turn/start']),1)

    def test_migration_preserves_user_pause_and_submission_uncertainty(self):
        error='Context repair waits for complete native tool receipts'
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('unsent',self.a['id'],'user','Keep this','failed',2,self.a['epoch'],None,error))
        base={'id':'held','submitted':False,'events':['unsent'],'epoch':self.a['epoch'],'accountKey':self.a['accountKey']}
        for fields in ({'autoWake':False},{'startAttempt':{**base,'submitted':True}}, {'startAttempt':{**base,'epoch':99}}):
            a=self.agent_update(self.a,status='failed',error=error,inFlight=False,autoWake=True,startAttempt=base)
            a=self.agent_update(a,**fields)
            with self.runtime.lock,self.runtime.db() as db:
                repair.recover_context_failures(self.runtime,db,[a])
            self.assertFalse(self.runtime.agent(a['id']).get('contextRepairWait'))
            self.assertEqual(self.runtime.agent(a['id'])['status'],'failed')

    def test_structured_provider_error_does_not_block_other_recovery(self):
        other = self.runtime.agent(self.a['id'])
        other.update(id='provider-limited', status='failed', error={
            'message':'Usage limit reached', 'codexErrorInfo':'usageLimitExceeded'},
            nativeFailureHold=True, inFlight=False)
        self.put('agents', other)
        error = 'Context repair waits for complete native tool receipts'
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('mixed-unsent',self.a['id'],'user','Preserve this input','failed',2,self.a['epoch'],None,error))
        a = self.agent_update(self.a,status='failed',error=error,inFlight=False,startAttempt={
            'id':'mixed-attempt','submitted':False,'events':['mixed-unsent'],
            'epoch':self.a['epoch'],'accountKey':self.a['accountKey']})
        with self.runtime.lock,self.runtime.db() as db:
            repair.recover_context_failures(self.runtime,db,[other,a])
        self.assertEqual(self.runtime.agent(other['id'])['error'], other['error'])
        self.assertTrue(self.runtime.agent(other['id'])['nativeFailureHold'])
        self.assertTrue(self.runtime.agent(a['id'])['contextRepairWait']['historicalFailureRecovered'])
        self.due()
        eventually(lambda:self.runtime.agent(a['id']).get('turnId')=='resumed-turn')
        self.assertEqual(len([m for m,p in self.server.calls if m=='turn/start']),1)

    def test_native_action_wait_keeps_receipt_and_resumes_once(self):
        self.monitor()
        result=self.runtime.native_action(self.a['id'],'review','wait-review')
        self.assertEqual(result['outcome']['status'],'pending')
        current=self.runtime.agent(self.a['id'])
        attempt=current['startAttempt']['id']
        self.assertEqual(current['status'],'queued')
        self.clear_monitor();self.due()
        eventually(lambda:self.runtime.agent(self.a['id']).get('turnId')=='review-turn')
        replay=self.runtime.native_action(self.a['id'],'review','wait-review')
        self.assertEqual(replay['outcome']['status'],'acknowledged')
        self.assertEqual(self.runtime.agent(self.a['id'])['startAttempt']['id'],attempt)
        self.assertEqual(len([m for m,p in self.server.calls if m=='review/start']),1)

    def test_superseded_action_wait_retires_only_exact_unsent_receipt(self):
        self.monitor()
        result=self.runtime.native_action(self.a['id'],'review','superseded-review')
        self.assertEqual(result['outcome']['status'],'pending')
        a=self.runtime.agent(self.a['id'])
        a=self.agent_update(a,epoch=a['epoch']+1,error='Keep newer error')
        with self.runtime.lock,self.runtime.db() as db:
            repair.claim_context_wait(self.runtime,db,a)
            outcome=json.loads(db.execute('SELECT outcome FROM runtime_native_action_receipts WHERE id=?',('superseded-review',)).fetchone()[0])
        self.assertTrue(outcome['notSubmitted'])
        self.assertEqual(outcome['status'],'failed')
        self.assertEqual(self.runtime.agent(a['id'])['error'],'Keep newer error')
        self.assertFalse(any(m=='review/start' for m,p in self.server.calls))


if __name__=='__main__':
    suite=unittest.TestSuite(ContextWait(name) for name in ContextWait.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
