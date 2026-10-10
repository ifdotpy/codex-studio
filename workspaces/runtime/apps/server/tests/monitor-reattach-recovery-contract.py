#!/usr/bin/env python3
"""A surviving child cannot restore a standalone monitor's lost RPC Future."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import copy
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
import uuid
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime
from codex_monitor_recovery import persist_monitor_result, recover_monitor_results, _path
spec = importlib.util.spec_from_file_location('restore_fixture', SERVER_TESTS_ROOT / 'supervisor-restore-selection-contract.py')
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
spec = importlib.util.spec_from_file_location('runtime_fixture', SERVER_TESTS_ROOT / 'runtime-contract.py')
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)

REASON = 'Native command reattached; discard the provisional disconnect notice.'
UNKNOWN = 'Server restarted. Command outcome unknown; not rerun.'


class MonitorReattachRecovery(unittest.TestCase):
    setUp = f.SupervisorRestoreSelectionContract.setUp
    tearDown = f.SupervisorRestoreSelectionContract.tearDown
    agent = f.SupervisorRestoreSelectionContract.agent
    record = f.SupervisorRestoreSelectionContract.record
    store = f.SupervisorRestoreSelectionContract.store
    stored = f.SupervisorRestoreSelectionContract.stored
    restore = f.SupervisorRestoreSelectionContract.restore

    def ghost(self, key='ghost', *, owner=None, **values):
        actor = copy.deepcopy(owner or self.owner)
        actor.update(status='waiting', autoWake=True, inFlight=False,
                     restartRecovery=None, turnId=None)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', actor)
        monitor = self.record(key, owner=actor, status='running', finished=None,
                              error=None, exitCode=None, command='never-repeat',
                              reattachRecovery=None, **values)
        monitor['operation']['connectionId'] = 'old-backend-connection'
        self.store('monitors', monitor)
        payload = {'id': key, 'status': 'lost', 'exitCode': None, 'error': UNKNOWN,
                   'command': monitor['command'], 'tail': monitor['tail']}
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('monitor:' + key, actor['id'], 'monitor_exit', json.dumps(payload),
                 'cancelled', 123, actor['epoch'], None, REASON))
        return monitor

    def event(self, key='ghost'):
        with self.runtime.read_db() as db:
            return dict(db.execute('SELECT * FROM runtime_events WHERE id=?', ('monitor:' + key,)).fetchone())

    def test_original_unknown_wake_is_not_cancelled_by_native_reattach(self):
        monitor = self.record('no-waiter', status='lost', finished=None, exitCode=None, error=UNKNOWN)
        monitor['operation']['connectionId'] = 'old-backend-connection'
        self.store('monitors', monitor)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime._monitor_exit_event(db, self.owner, monitor)
        before = self.event(monitor['id'])
        self.restore()
        self.assertEqual(self.stored('monitors', monitor['id']), monitor)
        self.assertEqual(self.event(monitor['id']), before)
        self.assertEqual(before['status'], 'pending')
        self.assertIsNone(json.loads(before['text'])['exitCode'])

    def null_notice(self, monitor, *, proof='restart'):
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(monitor['agent'], db)
            scope = {'epoch': actor['epoch'], 'accountKey': actor.get('accountKey', 'default'),
                     'threadId': actor['threadId'], 'turnId': 'restored-native-turn'}
            if proof == 'restart':
                actor['restartRecovery'] = {**scope, 'stage': 'reattached', 'autoWake': True,
                                            'at': 120, 'reattachedAt': 130}
                actor['supervisorRestore'] = {'status': 'not_restored', 'reason': 'agent_state_changed'}
            elif proof == 'finished':
                actor['restartRecovery'] = {**scope, 'stage': 'finished', 'autoWake': True,
                                            'at': 120, 'reattachedAt': 130, 'reconciledAt': 150}
            else:
                actor['restartRecovery'] = None
                actor['supervisorRestore'] = {**scope, 'status': 'restored',
                    'reason': 'live_handle_resumed', 'at': 130}
            self.runtime.put(db, 'agents', actor)
            db.execute('UPDATE runtime_events SET error=NULL WHERE id=?', ('monitor:' + monitor['id'],))

    def test_null_reason_live_ghost_uses_exact_historical_restore_proof(self):
        for proof in ('restart', 'finished', 'supervisor'):
            with self.subTest(proof=proof):
                monitor = self.ghost('null-' + proof)
                self.null_notice(monitor, proof=proof)
                before = self.event(monitor['id'])
                self.assertIn(monitor['id'], self.runtime.reconcile_monitor_reattach())
                after = self.event(monitor['id'])
                self.assertEqual((after['id'], after['text'], after['epoch'], after['created']),
                                 (before['id'], before['text'], before['epoch'], before['created']))
                self.assertEqual(after['status'], 'pending')
                self.assertEqual(self.stored('monitors', monitor['id'])['status'], 'lost')
                self.assertIsNone(self.stored('monitors', monitor['id'])['exitCode'])

    def test_null_reason_rejects_wrong_receipt_scope_time_payload_and_stop(self):
        cases = ('epoch', 'account', 'thread', 'turn', 'time', 'stage', 'payload', 'event-turn',
                 'notice-before-command', 'stopped', 'historical-lost')
        for name in cases:
            with self.subTest(name=name):
                monitor = self.ghost('null-invalid-' + name)
                self.null_notice(monitor)
                with self.runtime.lock, self.runtime.db() as db:
                    actor = self.runtime.agent(monitor['agent'], db)
                    receipt = actor['restartRecovery']
                    if name == 'epoch': receipt['epoch'] += 1
                    elif name == 'account': receipt['accountKey'] = 'other-account'
                    elif name == 'thread': receipt['threadId'] = 'other-thread'
                    elif name == 'turn': receipt['turnId'] = None
                    elif name == 'time': receipt['reattachedAt'] = 122
                    elif name == 'stage': receipt['stage'] = 'pending'
                    elif name == 'payload': db.execute("UPDATE runtime_events SET text=json_set(text,'$.error','Stopped by user') WHERE id=?", ('monitor:' + monitor['id'],))
                    elif name == 'event-turn': db.execute('UPDATE runtime_events SET turn_id=? WHERE id=?', ('received-turn', 'monitor:' + monitor['id']))
                    elif name == 'notice-before-command': monitor['created'] = 124
                    elif name == 'stopped': actor.update(status='paused', autoWake=False, epoch=actor['epoch'] + 1)
                    elif name == 'historical-lost': monitor.update(status='lost', reattachRecovery=None)
                    self.runtime.put(db, 'agents', actor)
                    self.runtime.put(db, 'monitors', monitor)
                before = self.event(monitor['id'])
                self.assertNotIn(monitor['id'], self.runtime.reconcile_monitor_reattach())
                self.assertEqual(self.event(monitor['id']), before)
                self.assertEqual(self.stored('monitors', monitor['id']), monitor)

    def test_invalid_null_history_before_valid_ghost_does_not_starve_recovery(self):
        for index in range(40):
            monitor = self.ghost('old-history-' + str(index).zfill(2))
            self.null_notice(monitor)
            monitor.update(status='lost', reattachRecovery=None)
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.put(db, 'monitors', monitor)
        monitor = self.ghost('zz-valid-live-ghost')
        self.null_notice(monitor)
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [monitor['id']])
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE status='pending'").fetchone()[0], 1)

    def test_null_notice_ghost_restart_stamps_only_prior_running_transition(self):
        monitor = self.ghost('null-restart')
        self.null_notice(monitor)
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(monitor['agent'], db)
            actor.update(status='running', inFlight=True, turnId=self.owner['turnId'])
            self.runtime.put(db, 'agents', actor)
        before = self.event(monitor['id'])
        self.restart()
        stored = self.stored('monitors', monitor['id'])
        self.assertGreaterEqual(stored['reattachRecovery']['cancelledNoticeLostAt'], before['created'])
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])
        self.restore()
        self.runtime.retry_monitor_results()
        self.assertEqual(self.event(monitor['id'])['status'], 'pending')
        self.assertEqual(self.event(monitor['id'])['text'], before['text'])
        self.assertIsNone(self.stored('monitors', monitor['id'])['exitCode'])
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])

    def test_null_notice_exact_file_after_restart_preserves_original_identity_and_hold(self):
        monitor = self.ghost('null-file')
        self.null_notice(monitor)
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(monitor['agent'], db)
            actor.update(status='running', inFlight=True, turnId=self.owner['turnId'])
            self.runtime.put(db, 'agents', actor)
        persist_monitor_result(self.runtime.root, monitor['id'], {
            'code': 0, 'error': None, 'operation': monitor['operation'], 'finished': 456})
        self.restart()
        event = self.event(monitor['id'])
        self.assertEqual((event['id'], event['status'], event['created']), ('monitor:null-file', 'pending', 123))
        self.assertEqual(json.loads(event['text'])['exitCode'], 0)
        self.assertFalse(self.stored('agents', self.owner['id'])['autoWake'])
        self.assertEqual(self.stored('agents', self.owner['id'])['status'], 'interrupted')
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events').fetchone()[0], 1)

    def test_current_ghost_reopens_exact_notice_once_and_queues_owner(self):
        monitor = self.ghost(inputs={'same-write': {'status': 'unknown', 'eof': True}})
        before = self.event()
        self.runtime.retry_monitor_results()
        after = self.event()
        self.assertEqual((after['id'], after['text'], after['epoch'], after['created']),
                         (before['id'], before['text'], before['epoch'], before['created']))
        self.assertEqual((after['status'], after['error']), ('pending', None))
        stored = self.stored('monitors', monitor['id'])
        self.assertEqual((stored['status'], stored['exitCode'], stored['finished']), ('lost', None, None))
        self.assertEqual(stored['error'], UNKNOWN)
        self.assertEqual(stored['inputs'], monitor['inputs'])
        self.assertEqual(self.stored('agents', self.owner['id'])['status'], 'queued')
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])
        self.assertEqual(self.event(), after)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events').fetchone()[0], 1)

    def test_exact_file_result_wins_under_original_event_identity(self):
        monitor = self.ghost()
        persist_monitor_result(self.runtime.root, monitor['id'], {
            'code': 0, 'error': None, 'operation': monitor['operation'], 'finished': 456})
        with patch.object(Path, 'glob', side_effect=AssertionError('No directory scan for bounded keys')):
            self.runtime.retry_monitor_results()
        stored = self.stored('monitors', monitor['id'])
        self.assertEqual((stored['status'], stored['exitCode'], stored['finished']), ('completed', 0, 456))
        payload = json.loads(self.event()['text'])
        self.assertEqual((payload['status'], payload['exitCode'], payload['error']), ('completed', 0, None))
        self.assertEqual((self.event()['id'], self.event()['status'], self.event()['created']),
                         ('monitor:ghost', 'pending', 123))
        self.assertFalse(_path(self.runtime.root, monitor['id']).exists())
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events').fetchone()[0], 1)

    def test_pending_exact_result_defers_unknown_and_then_wins(self):
        monitor = self.ghost(cancelRequested=True)
        self.runtime.pending_monitor_results = {}
        self.runtime.pending_monitor_results[monitor['id']] = {
            'code': 19, 'error': None, 'operation': monitor['operation'],
            'finished': 456, 'retryAt': time.monotonic() + 60, 'attempts': 0}
        before = self.event()
        self.runtime.retry_monitor_results()
        self.assertEqual(self.stored('monitors', monitor['id'])['status'], 'running')
        self.assertEqual(self.event(), before)
        self.runtime.pending_monitor_results[monitor['id']]['retryAt'] = 0
        self.runtime.retry_monitor_results()
        stored = self.stored('monitors', monitor['id'])
        self.assertEqual((stored['status'], stored['exitCode'], stored['finished']), ('cancelled', 19, 456))
        self.assertEqual(json.loads(self.event()['text'])['exitCode'], 19)
        self.assertEqual(self.runtime.pending_monitor_results, {})

    def test_scope_and_stop_guards_keep_cancelled_receipt_closed(self):
        cases = ('same-connection', 'missing-connection', 'offline', 'stopped', 'deleted',
                 'owner-epoch', 'operation-epoch', 'operation-owner', 'operation-account',
                 'event-owner', 'event-epoch', 'event-identity', 'event-status', 'event-reason',
                 'finished', 'exit-code', 'rule', 'no-auto-wake', 'null-account')
        for name in cases:
            with self.subTest(name=name):
                monitor = self.ghost(name)
                with self.runtime.lock, self.runtime.db() as db:
                    actor = self.runtime.agent(self.owner['id'], db)
                    if name == 'same-connection': monitor['operation']['connectionId'] = 'fixture-connection'
                    elif name == 'missing-connection': monitor['operation']['connectionId'] = None
                    elif name == 'offline': self.runtime.offline_accounts.add('default')
                    elif name == 'stopped': actor.update(status='paused', autoWake=False)
                    elif name == 'deleted': actor['deletedAt'] = 5
                    elif name == 'owner-epoch': actor['epoch'] += 1
                    elif name == 'operation-epoch': monitor['operation']['epoch'] += 1
                    elif name == 'operation-owner': monitor['operation']['agent'] = 'other-owner'
                    elif name == 'operation-account': monitor['operation']['accountKey'] = 'other-account'
                    elif name == 'event-owner': db.execute('UPDATE runtime_events SET agent=? WHERE id=?', ('other-owner', 'monitor:' + name))
                    elif name == 'event-epoch': db.execute('UPDATE runtime_events SET epoch=epoch+1 WHERE id=?', ('monitor:' + name,))
                    elif name == 'event-identity': db.execute("UPDATE runtime_events SET text=json_set(text,'$.id','other-monitor') WHERE id=?", ('monitor:' + name,))
                    elif name == 'event-status': db.execute("UPDATE runtime_events SET status='delivered' WHERE id=?", ('monitor:' + name,))
                    elif name == 'event-reason': db.execute("UPDATE runtime_events SET error='Stopped by user' WHERE id=?", ('monitor:' + name,))
                    elif name == 'finished': monitor['finished'] = 789
                    elif name == 'exit-code': monitor['exitCode'] = 0
                    elif name == 'rule': monitor['ruleId'] = 'rule-identity'
                    elif name == 'no-auto-wake': actor['autoWake'] = False
                    elif name == 'null-account': actor['accountKey'] = None
                    self.runtime.put(db, 'agents', actor)
                    self.runtime.put(db, 'monitors', monitor)
                before = self.event(name)
                self.assertNotIn(name, self.runtime.reconcile_monitor_reattach())
                self.assertEqual(self.stored('monitors', name), monitor)
                self.assertEqual(self.event(name), before)
                self.runtime.offline_accounts.discard('default')
                # Each case has its own original epoch and stopped state.
                self.owner = self.agent('fixture-owner')

    def test_stop_between_candidate_read_and_claim_wins(self):
        monitor = self.ghost()
        original = self.runtime.read_db
        @contextmanager
        def stop_after_read():
            with original() as db:
                yield db
            with self.runtime.lock, self.runtime.db() as db:
                actor = self.runtime.agent(self.owner['id'], db)
                actor.update(status='paused', autoWake=False, epoch=actor['epoch'] + 1)
                self.runtime.put(db, 'agents', actor)
        with patch.object(self.runtime, 'read_db', stop_after_read):
            self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])
        self.assertEqual(self.stored('monitors', monitor['id'])['status'], 'running')
        self.assertEqual(self.event()['status'], 'cancelled')

    def test_rollback_keeps_cancelled_notice_and_exact_file(self):
        monitor = self.ghost()
        persist_monitor_result(self.runtime.root, monitor['id'], {
            'code': 0, 'error': None, 'operation': monitor['operation'], 'finished': 456})
        before = self.event()
        original = self.runtime.db
        @contextmanager
        def rollback():
            with original() as db:
                yield db
                raise sqlite3.OperationalError('injected rollback')
        with patch.object(self.runtime, 'db', rollback):
            with self.assertRaisesRegex(sqlite3.OperationalError, 'injected rollback'):
                self.runtime.reconcile_monitor_reattach()
        self.assertEqual(self.event(), before)
        self.assertEqual(self.stored('monitors', monitor['id'])['status'], 'running')
        self.assertTrue(_path(self.runtime.root, monitor['id']).exists())
        self.runtime.retry_monitor_results()
        self.assertEqual(self.stored('monitors', monitor['id'])['exitCode'], 0)
        self.assertFalse(_path(self.runtime.root, monitor['id']).exists())

    def test_bounded_keys_ignore_unrelated_files_and_preserve_path_validation(self):
        monitor = self.ghost()
        wrong = _path(self.runtime.root, monitor['id'])
        wrong.parent.mkdir()
        wrong.write_text(json.dumps({'version': 1, 'key': 'different-key', 'code': 0,
            'error': None, 'operation': monitor['operation'], 'finished': 456}))
        unrelated = _path(self.runtime.root, 'unrelated')
        unrelated.write_text('{invalid')
        with self.runtime.lock, self.runtime.db() as db, \
                patch.object(Path, 'glob', side_effect=AssertionError('No directory scan')):
            result = recover_monitor_results(self.runtime, db, keys=[monitor['id'], monitor['id'], 'absent'])
        self.assertEqual(result['restored'], [])
        self.assertEqual(len(result['warnings']), 1)
        self.assertIn('filename', result['warnings'][0]['error'])
        self.assertEqual(unrelated.read_text(), '{invalid')
        self.assertTrue(wrong.exists())

    def test_selection_is_indexed_bounded_and_excludes_terminal_history(self):
        for index in range(34):
            self.ghost('active-' + str(index).zfill(2))
        self.store('monitors', *[self.record('history-' + str(index), status='completed',
            reattachRecovery=None, tail='history-payload', finished=456) for index in range(4000)])
        original, queries = self.runtime.read_db, []
        @contextmanager
        def observed():
            with original() as db:
                db.set_trace_callback(lambda query: queries.append(query) if query.startswith('WITH connections') else None)
                yield db
        with patch.object(self.runtime, 'read_db', observed):
            first = self.runtime.reconcile_monitor_reattach()
        self.assertEqual(len(first), 16)
        self.assertEqual(len(queries), 1)
        with original() as db:
            plan = [row[3] for row in db.execute('EXPLAIN QUERY PLAN ' + queries[0])]
        self.assertTrue(any('SEARCH m USING INDEX runtime_monitor_status' in item for item in plan), plan)
        self.assertFalse(any('SCAN m' in item for item in plan), plan)
        second, third = self.runtime.reconcile_monitor_reattach(), self.runtime.reconcile_monitor_reattach()
        self.assertEqual((len(second), len(third)), (16, 2))
        self.assertEqual(len(set(first + second + third)), 34)
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])

    def restart(self):
        self.runtime.close()
        with patch.object(Runtime, 'schedule', lambda _runtime: None):
            self.runtime = Runtime(Path(self.temp.name), f.NoNativeServer)
        self.addCleanup(self.runtime.close)
        self.runtime.connection_ids['default'] = 'fixture-connection'

    def test_lost_ghost_after_restart_waits_for_owner_restore_and_then_wakes_once(self):
        monitor = self.ghost()
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.owner['id'], db)
            actor.update(status='running', inFlight=True, turnId=self.owner['turnId'])
            self.runtime.put(db, 'agents', actor)
        before = self.event()
        self.restart()
        self.assertEqual(self.stored('monitors', monitor['id'])['status'], 'lost')
        self.assertFalse(self.stored('agents', self.owner['id'])['autoWake'])
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])
        self.assertEqual(self.event(), before)
        self.restore()
        self.runtime.retry_monitor_results()
        after = self.event()
        self.assertEqual((after['id'], after['text'], after['epoch'], after['created']),
                         (before['id'], before['text'], before['epoch'], before['created']))
        self.assertEqual((after['status'], after['error']), ('pending', None))
        self.assertIsNone(self.stored('monitors', monitor['id'])['exitCode'])
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])

    def test_stopped_owner_after_restart_never_reopens_ghost_wake(self):
        monitor = self.ghost()
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.owner['id'], db)
            actor.update(status='running', inFlight=True, turnId=self.owner['turnId'])
            self.runtime.put(db, 'agents', actor)
        self.restart()
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.owner['id'], db)
            actor.update(status='paused', autoWake=False, epoch=actor['epoch'] + 1)
            self.runtime.put(db, 'agents', actor)
        before = self.event()
        self.restore()
        self.runtime.retry_monitor_results()
        self.assertEqual(self.event(), before)
        self.assertEqual(self.stored('agents', self.owner['id'])['status'], 'paused')
        self.assertEqual(self.stored('monitors', monitor['id'])['status'], 'lost')

    def test_lost_ghost_disconnect_timestamp_does_not_infer_native_exit(self):
        monitor = self.ghost()
        monitor.update(status='lost', finished=456)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'monitors', monitor)
        self.runtime.retry_monitor_results()
        stored = self.stored('monitors', monitor['id'])
        self.assertEqual((stored['status'], stored['exitCode'], stored['finished']), ('lost', None, 456))
        self.assertEqual(self.event()['status'], 'pending')
        self.assertEqual(json.loads(self.event()['text'])['status'], 'lost')

    def test_startup_definitive_file_replaces_exact_cancelled_notice_and_preserves_hold(self):
        monitor = self.ghost()
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.owner['id'], db)
            actor.update(status='running', inFlight=True, turnId=self.owner['turnId'])
            self.runtime.put(db, 'agents', actor)
        persist_monitor_result(self.runtime.root, monitor['id'], {
            'code': 0, 'error': None, 'operation': monitor['operation'], 'finished': 456})
        self.restart()
        stored, event = self.stored('monitors', monitor['id']), self.event()
        self.assertEqual((stored['status'], stored['exitCode']), ('completed', 0))
        self.assertEqual((event['status'], event['created']), ('pending', 123))
        self.assertEqual((json.loads(event['text'])['status'], json.loads(event['text'])['exitCode']), ('completed', 0))
        self.assertFalse(self.stored('agents', self.owner['id'])['autoWake'])
        self.assertEqual(self.stored('agents', self.owner['id'])['status'], 'interrupted')
        self.assertEqual(self.runtime.reconcile_monitor_reattach(), [])
        self.restore()
        self.runtime.retry_monitor_results()
        self.assertEqual(self.event(), event)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events').fetchone()[0], 1)

    def test_exact_cancelled_notice_result_preserves_stop_and_other_event_states(self):
        for state in ('cancelled', 'pending', 'reserved', 'dispatching', 'uncertain', 'delivered'):
            with self.subTest(state=state):
                key = 'proof-' + state
                monitor = self.ghost(key)
                monitor.update(status='completed', exitCode=0, finished=456)
                with self.runtime.lock, self.runtime.db() as db:
                    actor = self.runtime.agent(self.owner['id'], db)
                    actor.update(status='paused', autoWake=False)
                    self.runtime.put(db, 'agents', actor)
                    self.runtime.put(db, 'monitors', monitor)
                    db.execute('UPDATE runtime_events SET status=? WHERE id=?', (state, 'monitor:' + key))
                    self.runtime._monitor_exit_event(db, actor, monitor)
                event = self.event(key)
                self.assertEqual(event['status'], state)
                if state == 'cancelled':
                    self.assertEqual(json.loads(event['text'])['status'], 'completed')
                    self.assertIsNone(event['error'])
                self.assertEqual(self.stored('agents', self.owner['id'])['status'], 'paused')

    def held_rule(self, checks=1, *, notified=None):
        rule = {'id': 'same-rule', 'name': 'Same watch', 'agent': self.owner['id'],
                'epoch': self.owner['epoch'], 'checks': checks, 'status': 'paused'}
        rule['restartCheck'] = {'epoch': rule['epoch'], 'checks': checks,
            'monitorId': str(uuid.uuid5(uuid.NAMESPACE_URL, 'rule:' + rule['id'] + ':' + str(checks)))}
        if notified is not None:
            rule['restartHoldNotified'] = notified
        return rule

    def test_rule_hold_marker_changes_after_real_explicit_resume(self):
        self.runtime._fast_delivery_enabled = False
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.owner['id'], db)
            actor.update(status='waiting', autoWake=True, restartRecovery=None)
            self.runtime.put(db, 'agents', actor)
        rule = self.runtime.rules_action({'action': 'save', 'agent': self.owner['id'],
            'name': 'Repeated hold', 'kind': 'interval', 'intervalSeconds': 3600,
            'command': 'never-run'})
        for check in (1, 2):
            with self.runtime.lock, self.runtime.db() as db:
                current = json.loads(db.execute('SELECT record FROM runtime_rules WHERE id=?', (rule['id'],)).fetchone()[0])
                current.update(status='active', inFlight=True, checks=check)
                self.runtime.put(db, 'rules', current)
                self.runtime.setup_rules(db)
                self.runtime.report_unresolved_rule_checks(db)
                saved = json.loads(db.execute('SELECT record FROM runtime_rules WHERE id=?', (rule['id'],)).fetchone()[0])
                self.assertEqual(saved['restartHoldNotified'], saved['restartCheck'])
            with self.runtime.read_db() as db:
                notices = db.execute("SELECT id,status FROM runtime_events WHERE kind='rule_hold'").fetchall()
            self.assertEqual(len(notices), check)
            self.assertTrue(all(notice['status'] == 'pending' for notice in notices))
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.report_unresolved_rule_checks(db)
            if check == 1:
                resumed = self.runtime.rules_action({'action': 'resume', 'agent': self.owner['id'], 'id': rule['id']})
                self.assertEqual(resumed['status'], 'active')
                self.assertNotIn('restartCheck', resumed)
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE kind='rule_hold'").fetchone()[0], 2)

    def test_legacy_rule_hold_true_migrates_without_event_or_owner_requeue(self):
        self.runtime._fast_delivery_enabled = False
        rule = self.held_rule(notified=True)
        event_id = 'rule-hold:same-rule:3:1:' + self.owner['id']
        actor = self.agent(self.owner['id'], status='completed', autoWake=True, restartRecovery=None)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', actor)
            self.runtime.put(db, 'rules', rule)
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                (event_id, actor['id'], 'rule_hold', '{"saved":true}', 'delivered', 123, actor['epoch'], 'old-turn', None))
            self.runtime.report_unresolved_rule_checks(db)
        with self.runtime.read_db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_rules WHERE id=?', (rule['id'],)).fetchone()[0])
            events = db.execute("SELECT * FROM runtime_events WHERE kind='rule_hold'").fetchall()
        self.assertEqual(saved['restartHoldNotified'], rule['restartCheck'])
        self.assertEqual(len(events), 1)
        self.assertEqual((events[0]['id'], events[0]['status'], events[0]['created'], events[0]['text']),
                         (event_id, 'delivered', 123, '{"saved":true}'))
        self.assertEqual(self.stored('agents', actor['id'])['status'], 'completed')

    def test_stopped_rule_owner_cannot_wake_self_or_parent_for_old_check(self):
        self.runtime._fast_delivery_enabled = False
        parent = self.agent('hold-parent', status='waiting', autoWake=True, restartRecovery=None)
        self.store('agents', parent)
        actor = self.agent(self.owner['id'], epoch=4, status='paused', autoWake=False,
                           parentId=parent['id'], rootId=parent['id'], restartRecovery=None)
        rule = self.held_rule()
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', actor)
            self.runtime.put(db, 'rules', rule)
            self.runtime.report_unresolved_rule_checks(db)
        with self.runtime.read_db() as db:
            events = db.execute("SELECT status FROM runtime_events WHERE kind='rule_hold'").fetchall()
        self.assertEqual([event['status'] for event in events], ['cancelled', 'cancelled'])
        self.assertEqual(self.stored('agents', actor['id'])['status'], 'paused')
        self.assertEqual(self.stored('agents', parent['id'])['status'], 'waiting')

    def test_startup_defers_fast_delivery_until_native_restore_finishes(self):
        actor = self.agent(self.owner['id'], status='waiting', autoWake=True,
                           inFlight=False, turnId=None, restartRecovery=None)
        monitor = self.record('startup-gate', owner=actor, status='running', finished=None, exitCode=None)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', actor)
            self.runtime.put(db, 'monitors', monitor)
        self.runtime.close()
        scheduled, starts, restores = [], [], []
        original_restore = Runtime._restore_startup_supervisor_handles
        def schedule(runtime, *args):
            scheduled.append((bool(restores), runtime.__dict__.get('_fast_delivery_enabled'), args))
        def start(runtime, *args):
            starts.append(bool(restores))
            raise AssertionError('This fixture cannot start a native turn')
        def restore(runtime):
            self.assertEqual(scheduled, [])
            self.assertEqual(starts, [])
            self.assertFalse(runtime.__dict__.get('_fast_delivery_enabled', True))
            with runtime.read_db() as db:
                event = db.execute('SELECT status FROM runtime_events WHERE id=?', ('monitor:startup-gate',)).fetchone()
                self.assertEqual(event['status'], 'pending')
            original_restore(runtime)
            restores.append(True)
        with patch.object(Runtime, 'schedule', lambda _runtime: None),                 patch.object(Runtime, 'schedule_fast_dispatch', schedule),                 patch.object(Runtime, 'start', start),                 patch.object(Runtime, '_restore_startup_supervisor_handles', restore):
            self.runtime = Runtime(Path(self.temp.name), f.NoNativeServer)
        self.addCleanup(self.runtime.close)
        self.assertEqual(restores, [True])
        self.assertEqual((scheduled, starts), ([], []))
        self.assertTrue(self.runtime._fast_delivery_enabled)
        self.assertEqual(self.stored('agents', actor['id'])['status'], 'queued')

    def test_startup_creates_lost_wake_without_native_reattach(self):
        actor = self.agent(self.owner['id'], status='running', autoWake=True, inFlight=True)
        monitor = self.record('startup', owner=actor, status='running', finished=None, exitCode=None)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'agents', actor)
            self.runtime.put(db, 'monitors', monitor)
        self.runtime.close()
        with patch.object(Runtime, 'schedule', lambda _runtime: None):
            self.runtime = Runtime(Path(self.temp.name), f.NoNativeServer)
        self.addCleanup(self.runtime.close)
        stored = self.stored('monitors', monitor['id'])
        event = self.event(monitor['id'])
        self.assertEqual((stored['status'], stored['exitCode']), ('lost', None))
        self.assertEqual((event['id'], event['status']), ('monitor:startup', 'pending'))
        self.assertEqual(json.loads(event['text'])['error'], UNKNOWN)


class MonitorWakeIntegration(unittest.TestCase):
    def test_ghost_wakes_child_once_and_final_result_reaches_parent(self):
        with tempfile.TemporaryDirectory(prefix='studio-monitor-wake-') as directory:
            with patch.object(Runtime, 'schedule', lambda _runtime: None):
                runtime = Runtime(Path(directory), native.FakeServer)
            try:
                runtime._fast_delivery_enabled = False
                parent = runtime.create({'name': 'Parent', 'cwd': directory, 'prompt': 'Wait'})
                child = runtime.create({'name': 'Child', 'prompt': 'Review', 'role': 'reviewer'}, parent['id'])
                runtime.dispatch()
                native.eventually(lambda: runtime.agent(child['id'])['status'] == 'running')
                actor = runtime.agent(child['id'])
                server = runtime.servers[actor['accountKey']]
                key = 'integration-ghost'
                operation = {'agent': actor['id'], 'epoch': actor['epoch'],
                             'accountKey': actor['accountKey'], 'connectionId': 'old-backend'}
                monitor = {'id': key, 'agent': actor['id'], 'epoch': actor['epoch'], 'created': 1,
                           'status': 'running', 'exitCode': None, 'finished': None,
                           'operation': operation, 'command': 'never-repeat', 'tail': ''}
                payload = json.dumps({'id': key, 'status': 'lost', 'exitCode': None, 'error': UNKNOWN})
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, 'monitors', monitor)
                    db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                        ('monitor:' + key, actor['id'], 'monitor_exit', payload, 'cancelled',
                         123, actor['epoch'], None, REASON))
                server.complete(actor['threadId'], actor['turnId'], 'Result before the lost notice')
                self.assertEqual(runtime.agent(child['id'])['status'], 'waiting')
                with runtime.read_db() as db:
                    self.assertFalse(db.execute("SELECT 1 FROM runtime_events WHERE kind='child_result'").fetchone())
                before = len([params for method, params in server.calls
                              if method == 'turn/start' and params['threadId'] == actor['threadId']])
                next_turn_started = threading.Event()
                original_notify = server._notify

                def notify_after_next_turn(message):
                    original_notify(message)
                    params = message.get('params', {})
                    turn = params.get('turn', {})
                    if (message.get('method') == 'turn/started' and
                            params.get('threadId') == actor['threadId'] and
                            turn.get('id') != actor['turnId']):
                        next_turn_started.set()

                server._notify = notify_after_next_turn
                runtime.retry_monitor_results()
                self.assertEqual(runtime.agent(child['id'])['status'], 'queued')
                runtime.dispatch()
                self.assertTrue(next_turn_started.wait(30),
                                'recovered monitor receipt did not start the next turn')
                self.assertEqual(runtime.agent(child['id'])['status'], 'running')
                self.assertNotEqual(runtime.agent(child['id'])['turnId'], actor['turnId'])
                continued = runtime.agent(child['id'])
                runtime.dispatch()
                starts = [params for method, params in server.calls
                          if method == 'turn/start' and params['threadId'] == actor['threadId']]
                self.assertEqual(len(starts), before + 1)
                self.assertIn('monitor_exit', json.dumps(starts[-1]))
                self.assertIn(UNKNOWN, json.dumps(starts[-1]))
                self.assertFalse(any(method.startswith('command/exec') for method, _ in server.calls))
                server.complete(continued['threadId'], continued['turnId'], 'Verified final child result')
                with runtime.read_db() as db:
                    results = db.execute("SELECT text FROM runtime_events WHERE kind='child_result'").fetchall()
                    notice = db.execute('SELECT status FROM runtime_events WHERE id=?', ('monitor:' + key,)).fetchone()
                self.assertEqual(len(results), 1)
                self.assertEqual(json.loads(results[0][0])['result'], 'Verified final child result')
                self.assertEqual(notice['status'], 'delivered')
            finally:
                runtime.close()


if __name__ == '__main__':
    unittest.main()
