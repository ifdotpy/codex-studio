#!/usr/bin/env python3
"""Lost terminal notifications, exact identities, and non-destructive recovery."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import queue
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
from codex_turn_recovery import START_PACE_LIMIT, recent_account_starts
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class RecoveryServer(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.native = None
        self.read_error = None
        self.before_apply = None
        self.read_entered = threading.Event()
        self.read_gate = None

    def call(self, method, params, timeout=60):
        if method == 'thread/turns/list':
            self.calls.append((method, params))
            offset = int(params.get('cursor', '0'))
            turns = self.native.get('turns', [])
            end = offset + params['limit']
            return {'data': turns[offset:end], 'nextCursor': str(end) if end < len(turns) else None}
        if method == 'thread/read':
            self.calls.append((method, params))
            self.read_entered.set()
            if self.read_gate:
                assert self.read_gate.wait(3)
            if self.read_error:
                raise self.read_error
            result = dict(self.native)
            # Model the native server's state transition too: authoritative
            # history says the active turn has already reached a terminal state.
            active = self.active_turns.get(params['threadId'])
            terminal = next((turn for turn in result.get('turns', [])
                             if active and turn.get('id') == active.get('id')),
                            None)
            if (result.get('status', {}).get('type') != 'active'
                    and terminal and terminal.get('status') in {'completed', 'failed', 'interrupted'}):
                self.active_turns.pop(params['threadId'], None)
            if not params['includeTurns']:
                result.pop('turns', None)
            return {'thread': result}
        return super().call(method, params, timeout)

    def after_events(self, callback):
        if self.before_apply:
            self.before_apply()
        callback()


class TurnRecoveryContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), RecoveryServer)
        self.server = self.runtime.connect()
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Work'})
        self.key = a['id']
        fixture.eventually(lambda: bool(self.runtime.agent(self.key).get('turnId')))
        fixture.eventually(lambda: bool((self.runtime.agent(self.key).get('startAttempt') or {}).get('turnId')))
        self.a = self.runtime.agent(self.key)
        self.turn = self.a['turnId']
        self.server.native = {'id': self.a['threadId'], 'status': {'type': 'idle'}, 'turns': [
            {'id': self.turn, 'status': 'completed', 'items': [
                {'id': 'answer', 'type': 'agentMessage', 'text': 'Full final answer', 'phase': 'final_answer'}]}]}

    def tearDown(self):
        if self.server.read_gate:
            self.server.read_gate.set()
        self.runtime.close()
        self.temp.cleanup()

    def lose_start_receipt(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            attempt = a['startAttempt']
            for field in ('turnId', 'observedTurnId'):
                attempt.pop(field, None)
            attempt.update(executionOutcome='unknown', created=time.time() - 180)
            a.update(status='starting', turnId=None, inFlight=True,
                     error='turn/start response timed out; outcome unknown')
            self.runtime.put(db, 'agents', a)
            for key in attempt['events']:
                db.execute("UPDATE runtime_events SET status='uncertain',turn_id=NULL,error=? WHERE id=?",
                           (a['error'], key))
            self.server.native['turns'][0]['clientUserMessageId'] = attempt['events'][0]
        return a

    def test_unknown_start_receipt_recovers_completion_without_resubmission(self):
        # This test owns the explicit reconciliation call. Keep the periodic
        # scheduler from racing it after the fixture ages the start receipt.
        with patch.object(self.runtime, 'queue_turn_recovery'):
            a = self.lose_start_receipt()
            before = sum(method == 'turn/start' for method, _ in self.server.calls)
            result = self.runtime.reconcile_turn(self.key)
        self.assertEqual(result['status'], 'reconciled')
        current = self.runtime.agent(self.key)
        self.assertEqual(current['status'], 'completed')
        self.assertIsNone(current['error'])
        self.assertFalse(current['inFlight'])
        self.assertEqual(current['lastAnswer'], 'Full final answer')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status,turn_id FROM runtime_events WHERE id=?',
                                       (a['startAttempt']['events'][0],)).fetchone()[:], ('delivered', self.turn))
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), before)

    def test_late_start_reply_clears_timeout_and_binds_input(self):
        # This test owns delivery of the late receipt. Prevent periodic recovery
        # from observing the aged fixture and completing the simulated turn first.
        with patch.object(self.runtime, 'queue_turn_recovery'):
            a = self.lose_start_receipt()
            before = sum(method == 'turn/start' for method, _ in self.server.calls)
            self.runtime.start_accepted(self.key, a['startAttempt'], {'turn': {'id': self.turn}})
            current = self.runtime.agent(self.key)
            self.assertEqual((current['status'], current['turnId'], current['error']),
                             ('running', self.turn, None))
            self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), before)

    def test_observed_turn_clears_stale_timeout_error(self):
        # This test owns the notification. Keep periodic reconciliation from
        # completing the aged synthetic turn before the notification is applied.
        with patch.object(self.runtime, 'queue_turn_recovery'):
            self.lose_start_receipt()
            self.server.notify({'method': 'turn/started', 'params': {
                'threadId': self.a['threadId'], 'turn': {'id': self.turn, 'status': 'inProgress'}}})
            current = self.runtime.agent(self.key)
            self.assertEqual((current['status'], current['turnId'], current['error']),
                             ('running', self.turn, None))

    def test_dispatch_clears_saved_timeout_after_turn_was_observed(self):
        self.lose_start_receipt()
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(self.key, db)
            current.update(status='running', turnId=self.turn, startAttempt=None)
            self.runtime.put(db, 'agents', current)
        self.runtime.dispatch()
        self.assertIsNone(self.runtime.agent(self.key)['error'])

    def test_unknown_start_preparing_receipt_does_not_confirm_delivery(self):
        a = self.lose_start_receipt()
        self.server.native['turns'][0]['startOutcome'] = 'preparing'
        result = self.runtime.reconcile_turn(self.key)
        self.assertEqual(result['status'], 'unconfirmed')
        self.assertEqual(self.runtime.agent(self.key)['startAttempt'], a['startAttempt'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                                       (a['startAttempt']['events'][0],)).fetchone()[0], 'uncertain')

    def test_scheduler_checks_unknown_start_with_no_turn_identity(self):
        self.lose_start_receipt()
        with self.runtime.db() as db:
            agents = self.runtime.scheduler_agents(db)
        self.runtime.queue_turn_recovery(agents, force_id=self.key)
        fixture.eventually(lambda: self.runtime.agent(self.key)['status'] == 'completed')

    def test_unknown_start_absent_from_history_preserves_original_reservation(self):
        a = self.lose_start_receipt()
        self.server.native['turns'] = []
        result = self.runtime.reconcile_turn(self.key)
        self.assertEqual(result['status'], 'unconfirmed')
        self.assertEqual(self.runtime.agent(self.key)['startAttempt'], a['startAttempt'])
        self.assertEqual(self.runtime.agent(self.key)['status'], 'starting')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                                       (a['startAttempt']['events'][0],)).fetchone()[0], 'uncertain')

    def test_absent_input_after_native_child_replacement_restores_exact_batch(self):
        # Inspect restored input before automatic recovery or delivery.
        with patch.object(self.runtime, 'dispatch_candidates', return_value=None):
            a = self.lose_start_receipt()
            self.server.native['turns'] = []
            self.server.supervisor_mode = True
            self.server.proc = SimpleNamespace(root=Path(self.temp.name), handle='account:default', generation=2)
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(self.key, db)
                current['startAttempt']['supervisorIdentity'] = {
                    'stateDir': str(Path(self.temp.name).resolve()), 'handle': 'account:default', 'generation': 1}
                current['startAttempt']['connectionId'] = 'old-connection'
                self.runtime.put(db, 'agents', current)
            before = sum(method == 'turn/start' for method, _ in self.server.calls)
            result = self.runtime.reconcile_turn(self.key)
            self.assertEqual(result['status'], 'input_restored')
            current = self.runtime.agent(self.key)
            self.assertEqual((current['status'], current['inFlight'], current['error']), ('queued', False, None))
            self.assertNotIn('startAttempt', current)
            self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), before)
            with self.runtime.db() as db:
                self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                                           (a['startAttempt']['events'][0],)).fetchone()[0], 'pending')

    def test_absent_input_does_not_restore_when_native_child_is_active(self):
        self.lose_start_receipt()
        self.server.native.update(turns=[], status={'type': 'active'})
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=Path(self.temp.name), handle='account:default', generation=2)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(self.key, db)
            current['startAttempt']['supervisorIdentity'] = {
                'stateDir': str(Path(self.temp.name).resolve()), 'handle': 'account:default', 'generation': 1}
            self.runtime.put(db, 'agents', current)
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertEqual(self.runtime.agent(self.key)['status'], 'starting')

    def test_absent_input_stays_reserved_when_native_evidence_is_unavailable(self):
        a = self.lose_start_receipt()
        self.server.native['turns'] = []
        self.server.read_error = TimeoutError('thread/read response timed out; outcome unknown')
        self.server.supervisor_mode = True
        self.server.proc = SimpleNamespace(root=Path(self.temp.name), handle='account:default', generation=2)
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(self.key, db)
            current['startAttempt']['supervisorIdentity'] = {
                'stateDir': str(Path(self.temp.name).resolve()), 'handle': 'account:default', 'generation': 1}
            self.runtime.put(db, 'agents', current)
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertEqual(self.runtime.agent(self.key)['startAttempt']['id'], a['startAttempt']['id'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                                       (a['startAttempt']['events'][0],)).fetchone()[0], 'uncertain')

    def test_old_unknown_start_becomes_a_hold_and_late_reply_still_binds(self):
        # This test owns the reconciliation sequence; the periodic scheduler
        # must not race the explicit hold and late-receipt assertions below.
        with patch.object(self.runtime, 'queue_turn_recovery'):
            a = self.lose_start_receipt()
            self.server.native['turns'] = []
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(self.key, db)
                current['startAttempt']['created'] = time.time() - 700
                self.runtime.put(db, 'agents', current)
            result = self.runtime.reconcile_turn(self.key)
            self.assertEqual(result['status'], 'held')
            current = self.runtime.agent(self.key)
            self.assertEqual((current['status'], current['inFlight']), ('interrupted', False))
            self.assertEqual(current['startOutcomeHold']['stage'], 'held')
            self.assertIn('Start outcome unknown', current['error'])
            self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
            with self.runtime.db() as db:
                self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                                           (a['startAttempt']['events'][0],)).fetchone()[0], 'uncertain')
            self.runtime.start_accepted(self.key, current['startAttempt'], {'turn': {'id': self.turn}})
            current = self.runtime.agent(self.key)
            self.assertEqual((current['status'], current['inFlight'], current['error']), ('running', True, None))
            self.assertNotIn('startOutcomeHold', current)

    def test_old_unknown_start_waits_for_callback_backlog(self):
        self.lose_start_receipt()
        self.server.native['turns'] = []
        self.server.callbacks = queue.Queue()
        self.server.callbacks.put('older notification')
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(self.key, db)
            current['startAttempt']['created'] = time.time() - 700
            self.runtime.put(db, 'agents', current)
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertNotIn('startOutcomeHold', self.runtime.agent(self.key))

    def test_old_unknown_start_waits_for_supervisor_ack(self):
        # Commits wake automatic recovery. This fixture owns two manual probes.
        with patch.object(self.runtime, 'queue_turn_recovery', return_value=None):
            self.lose_start_receipt()
            self.server.native['turns'] = []
            self.server.supervisor_mode = True
            class Journal:
                def __init__(self):
                    self.acknowledged = 2
                    self.actions = []
                def call(self, action):
                    self.actions.append(action)
                    return {'sequence': 3, 'acknowledged': self.acknowledged, 'backpressure': False}
            journal = Journal()
            self.server.proc = journal
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(self.key, db)
                current['startAttempt']['created'] = time.time() - 700
                self.runtime.put(db, 'agents', current)
            self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
            self.assertNotIn('startOutcomeHold', self.runtime.agent(self.key))
            journal.acknowledged = 3
            self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'held')
            self.assertEqual(journal.actions, ['status', 'status'])

    def test_start_pace_counts_only_recent_unresolved_codex_starts(self):
        now = time.time()
        attempts = [{'accountKey': 'same', 'provider': 'codex', 'autoWake': True,
                     'inFlight': True, 'startAttempt': {'id': str(i), 'created': now - 5}}
                    for i in range(START_PACE_LIMIT)]
        self.assertEqual(recent_account_starts(attempts, now), {'same': START_PACE_LIMIT})
        attempts[0]['startAttempt']['observedTurnId'] = 'turn'
        attempts[1]['startAttempt']['created'] = now - 100
        attempts[2]['provider'] = 'claude'
        self.assertEqual(recent_account_starts(attempts, now), {'same': 1})

    def test_dispatch_keeps_new_input_queued_when_account_start_cap_is_full(self):
        with self.runtime.lock:
            with self.runtime.db() as db:
                current = self.runtime.agent(self.key, db)
                current.update(status='queued', inFlight=False, turnId=None, startAttempt=None)
                self.runtime.put(db, 'agents', current)
                self.runtime.enqueue(db, current, 'user', 'Next instruction', 'paced-input')
            before = sum(method == 'turn/start' for method, _ in self.server.calls)
            with patch('codex_turn_recovery.recent_account_starts', return_value={'default': START_PACE_LIMIT}):
                self.runtime.dispatch()
            current = self.runtime.agent(self.key)
            self.assertIsNone(current.get('startAttempt'))
            self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), before)
            with self.runtime.db() as db:
                self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='paced-input'").fetchone()[0],
                                 'pending')

    def test_unknown_start_read_cannot_restore_an_agent_stopped_during_the_probe(self):
        self.lose_start_receipt()
        self.server.before_apply = lambda: self.runtime.stop(self.key, descendants=False)
        result = self.runtime.reconcile_turn(self.key)
        self.assertEqual(result['status'], 'superseded')
        self.assertFalse(self.runtime.agent(self.key)['autoWake'])

    def test_lost_completion_repairs_partial_text_and_is_idempotent(self):
        self.server.notify({'method': 'item/agentMessage/delta', 'params': {
            'threadId': self.a['threadId'], 'turnId': self.turn, 'itemId': 'answer', 'delta': 'par'}})
        result = self.runtime.reconcile_turn(self.key)
        self.assertEqual(result['outcome'], 'completed')
        a = self.runtime.agent(self.key)
        self.assertFalse(a['inFlight'])
        self.assertIsNone(a['turnId'])
        self.assertEqual(a['lastAnswer'], 'Full final answer')
        self.assertEqual(a['turnRecovery']['source'], 'native_thread_read')
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'skipped')
        with self.runtime.db() as db:
            row = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?', (self.key + ':answer',)).fetchone()[0])
            self.assertFalse(row['streaming'])
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_completed_turns').fetchone()[0], 1)

    def test_scheduler_roster_reaches_orphan_busy_turn_recovery(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a.update(status='running', inFlight=True, turnId=None, startAttempt=None,
                     threadId='orphan-thread', activity={'at': time.time() - 180}, lastEvent=None)
            self.runtime.put(db, 'agents', a)
            agents = self.runtime.scheduler_agents(db)
        self.assertIn(self.key, {agent['id'] for agent in agents})
        submitted = []
        with patch.object(self.runtime.recovery_pool, 'submit',
                          side_effect=lambda *args: submitted.append(args)):
            self.runtime.queue_turn_recovery(agents)
        self.assertEqual(len(submitted), 1)
        self.assertEqual(submitted[0][1], self.key)

    def test_completed_turn_on_later_page_recovers_once(self):
        self.server.native['turns'] = [
            {'id': 'newer-' + str(i), 'status': 'completed', 'items': []} for i in range(31)
        ] + self.server.native['turns']
        result = self.runtime.reconcile_turn(self.key)
        if result.get('status') == 'skipped':
            # The background scheduler may have applied this exact recovery first.
            fixture.eventually(lambda: (self.runtime.agent(self.key).get('turnRecovery') or {}).get('turnId') == self.turn)
        else:
            self.assertEqual((result.get('status'), result.get('outcome')), ('reconciled', 'completed'))
        self.assertEqual((self.runtime.agent(self.key).get('turnRecovery') or {}).get('outcome'), 'completed')
        self.assertEqual(self.runtime.agent(self.key)['lastAnswer'], 'Full final answer')
        pages = [p for m, p in self.server.calls if m == 'thread/turns/list']
        self.assertEqual([p.get('cursor') for p in pages], [None, '10', '20', '30'])
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'skipped')

    def test_unloaded_interrupted_thread_clears_preparation_cache(self):
        self.assertIn(self.key, self.runtime.loaded)
        self.server.native['status']['type'] = 'notLoaded'
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.assertEqual(self.runtime.reconcile_turn(self.key)['outcome'], 'interrupted')
        self.assertNotIn(self.key, self.runtime.loaded)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'interrupted')
        self.assertFalse(any(name == 'turn/interrupt' for name, _ in self.server.calls))

    def test_failed_turn_preserves_native_error(self):
        self.server.native['turns'][0].update(status='failed', error={'message': 'native failure'})
        self.runtime.reconcile_turn(self.key)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'failed')
        self.assertEqual(self.runtime.agent(self.key)['error'], {'message': 'native failure'})

    def test_active_turn_never_reads_full_history_or_changes_state(self):
        self.server.native['status']['type'] = 'active'
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'active_or_unknown')
        self.assertEqual([p['includeTurns'] for n, p in self.server.calls if n == 'thread/read'], [False])
        self.assertEqual(self.runtime.agent(self.key)['turnId'], self.turn)

    def test_read_timeout_is_not_completion_or_mutation_retry(self):
        before = list(self.server.calls)
        self.server.read_error = TimeoutError('unknown outcome')
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])
        self.assertEqual([n for n, _ in self.server.calls[len(before):]], ['thread/read'])

    def test_missing_exact_turn_and_unknown_status_are_not_completion(self):
        for turn_id, status in [('other-turn', 'completed'), (self.turn, 'inProgress')]:
            self.server.native['turns'][0].update(id=turn_id, status=status)
            self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
            self.assertTrue(self.runtime.agent(self.key)['inFlight'])

    def test_native_thread_identity_must_match(self):
        self.server.native['id'] = 'foreign-thread'
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])

    def test_queued_notifications_are_applied_before_identity_check(self):
        def newer():
            with self.runtime.lock, self.runtime.db() as db:
                a = self.runtime.agent(self.key, db)
                a['turnId'] = 'new-turn'
                self.runtime.put(db, 'agents', a)
        self.server.before_apply = newer
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'superseded')
        self.assertEqual(self.runtime.agent(self.key)['turnId'], 'new-turn')

    def test_stop_and_account_connection_replacement_win(self):
        for field, value in [('epoch', 999), ('accountKey', 'foreign'), ('deletedAt', 1)]:
            with self.subTest(field=field):
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                def changed():
                    with self.runtime.lock, self.runtime.db() as db:
                        a = self.runtime.agent(self.key, db); a[field] = value
                        self.runtime.put(db, 'agents', a)
                self.server.before_apply = changed
                self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'superseded')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', self.a)
        self.server.before_apply = lambda: self.runtime.connection_ids.update(default='replacement')
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'superseded')

    def test_monitor_and_detached_command_are_not_stopped(self):
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {'id': 'monitor', 'agent': self.key, 'status': 'running'})
            self.runtime.put(db, 'tasks', {'id': self.key + ':command', 'agent': self.key, 'turnId': self.turn,
                'kind': 'command', 'processId': 'live-process', 'status': 'running'})
        self.runtime.reconcile_turn(self.key)
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, 'monitors')[0]['status'], 'running')
            self.assertEqual(self.runtime.records(db, 'tasks')[0]['status'], 'running')
        self.assertFalse(any(n in {'command/exec/terminate', 'turn/interrupt'} for n, _ in self.server.calls))

    def test_existing_pending_message_delivered_once(self):
        self.runtime.send(self.key, 'Next task', message_id='next-task')
        self.runtime.reconcile_turn(self.key)
        fixture.eventually(lambda: self.runtime.agent(self.key).get('turnId') not in (None, self.turn))
        self.runtime.reconcile_turn(self.key)  # Native evidence only names the old turn.
        starts = [p for n, p in self.server.calls if n == 'turn/start']
        self.assertEqual(len(starts), 2)
        self.assertEqual([p.get('clientUserMessageId') for p in starts].count('next-task'), 1)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE id='next-task'").fetchone()[0], 1)

    def test_storage_failure_can_recover_later_without_replaying_commands(self):
        import sqlite3
        original = self.runtime.notification
        self.runtime.notification = lambda *args: (_ for _ in ()).throw(sqlite3.OperationalError('disk I/O error'))
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])
        self.runtime.notification = original
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'reconciled')
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'turn/start']), 1)

    def test_replacement_connection_invalidates_old_preparation_cache(self):
        import concurrent.futures
        self.assertIn(self.key, self.runtime.loaded)
        self.runtime.connection_ids['default'] = 'replacement'
        future = self.runtime.prepare_locked(self.runtime.agent(self.key))
        self.assertIsInstance(future, concurrent.futures.Future)
        future.result(timeout=3)
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'thread/resume']), 1)
        self.assertEqual(self.runtime.preparations[self.key]['connectionId'], 'replacement')

    def test_scheduler_recovers_without_another_user_message(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a['activity'] = {'phase': 'writing', 'at': time.time() - 130}
            a['lastEvent'] = '2020-01-01T00:00:00Z'
            self.runtime.put(db, 'agents', a)
        self.runtime.changed.set()
        fixture.eventually(lambda: not self.runtime.agent(self.key)['inFlight'])
        self.assertEqual(self.runtime.agent(self.key)['status'], 'completed')
        self.assertEqual(len([1 for method, _ in self.server.calls if method == 'turn/start']), 1)



    def orphan(self):
        self.assertEqual(self.runtime.reconcile_turn(self.key)['outcome'], 'completed')
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a.update(status='running', inFlight=True, turnId=None, steerRejectedTurnId='',
                     activity={'phase': 'thinking', 'at': time.time() - 130},
                     lastEvent='2020-01-01T00:00:00Z')
            a.pop('startAttempt', None)
            self.runtime.put(db, 'agents', a)
            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                       ('orphan-input', self.key, 'user', 'Continue.', 'pending', time.time(), a['epoch'], None, None))

    def test_orphan_busy_flag_clears_when_native_idle_and_input_starts_once(self):
        self.orphan()
        starts = len([1 for method, _ in self.server.calls if method == 'turn/start'])
        self.runtime.changed.set()
        fixture.eventually(lambda: self.runtime.agent(self.key).get('turnRecovery', {}).get('outcome') == 'idle')
        fixture.eventually(lambda: len([1 for method, _ in self.server.calls if method == 'turn/start']) == starts + 1)
        a = self.runtime.agent(self.key)
        self.assertNotIn('steerRejectedTurnId', a)
        for _ in range(3):
            self.runtime.dispatch()
        self.assertEqual(len([1 for method, _ in self.server.calls if method == 'turn/start']), starts + 1)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='orphan-input'").fetchone()[0],
                             'delivered')

    def test_orphan_busy_flag_waits_for_active_or_unprocessed_native_turn(self):
        self.orphan()
        self.server.native['status']['type'] = 'active'
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'active_or_unknown')
        self.server.native['status']['type'] = 'idle'
        self.server.native['turns'].insert(0, {'id': 'unprocessed-turn', 'status': 'completed', 'items': []})
        self.assertEqual(self.runtime.reconcile_turn(self.key)['status'], 'unconfirmed')
        a = self.runtime.agent(self.key)
        self.assertTrue(a['inFlight'])
        self.assertEqual(a['steerRejectedTurnId'], '')

    def test_scheduler_probes_one_at_a_time_without_holding_runtime_lock(self):
        self.server.native['status']['type'] = 'active'
        self.server.read_gate = threading.Event()
        with self.runtime.lock:
            self.runtime.queue_turn_recovery([self.a], force_id=self.key)
            self.runtime.queue_turn_recovery([self.a], force_id=self.key)
        self.assertTrue(self.server.read_entered.wait(2))
        self.assertTrue(self.runtime.lock.acquire(timeout=.2))
        self.runtime.lock.release()
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'thread/read']), 1)
        self.server.read_gate.set()
        fixture.eventually(lambda: not self.runtime._turn_recovery_busy)
        self.runtime.queue_turn_recovery([self.a])
        self.assertEqual(len([n for n, _ in self.server.calls if n == 'thread/read']), 1)


if __name__ == '__main__':
    unittest.main()
