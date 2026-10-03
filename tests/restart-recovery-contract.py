#!/usr/bin/env python3
"""Restart preserves admitted work without replaying unknown operations."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import concurrent.futures
import importlib.util
import json
import os
import queue
import subprocess
import sys
import threading
import time
import uuid
from types import SimpleNamespace
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('connection-recovery-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_connection_recovery import recover, tick as connection_recovery_tick
from codex_restart_recovery import capture, restore, settle_reconciled


class RestartContract(fixture.ConnectionRecoveryContract):
    def make_worker(self):
        parent = self.runtime.create({'name': 'Parent', 'cwd': self.temp.name, 'prompt': ''},
                                     draft=True, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            worker = self.runtime.agent(self.key, db)
            worker.update(isLead=False, name='Worker', parentId=parent['id'], rootId=parent['id'])
            self.runtime.put(db, 'agents', worker)
            parent_record = self.runtime.agent(parent['id'], db)
            parent_record.update(status='completed', autoWake=True)
            self.runtime.put(db, 'agents', parent_record)
        return parent['id']

    def restart(self, *, supervisor=False, **values):
        self.update(status='running', autoWake=True, inFlight=True, error=None, **values)
        native = copy.deepcopy(self.server.native)
        self.runtime.close()
        env = {'CODEX_AGENTS_SUPERVISOR_MODE': '1'} if supervisor else {}
        with patch.dict(os.environ, env):
            self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
            self.runtime.supervisor_mode = os.environ.get('CODEX_AGENTS_SUPERVISOR_MODE') == '1'
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.server.supervisor_mode = self.runtime.supervisor_mode
        self.server.pending = {}
        self.server.native = native
        self.server.calls.clear()
        return self.runtime.agent(self.key)

    def test_real_restart_restores_completed_delivery(self):
        a = self.restart()
        self.assertEqual(a['disconnectRecovery']['source'], 'restart')
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'reconciled')
        a = self.runtime.agent(self.key)
        self.assertTrue(a['autoWake'])
        self.assertEqual(a['lastAnswer'], 'Full final answer')
        self.assertEqual(a['status'], 'completed')
        self.read_calls_only()

    def test_interrupted_turn_continues_once_across_second_restart(self):
        parent_id = self.make_worker()
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.restart()
        with self.runtime.lock, self.runtime.db() as db:
            scoped = self.runtime.scheduler_agents(db)
        self.assertIn(self.key, {agent['id'] for agent in scoped})
        self.assertEqual(self.runtime.agent(self.key)['restartRecovery']['stage'], 'pending')
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND kind='child_result'",
                                        (parent_id,)).fetchone()[0], 0)
        result = recover(self.runtime, self.key, automatic=True)
        self.assertTrue(result['continued'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND kind='child_result'",
                                        (parent_id,)).fetchone()[0], 0)
        a = self.runtime.agent(self.key)
        self.assertEqual(a['status'], 'queued')
        self.assertTrue(a['autoWake'])
        key = a['restartRecovery']['eventId']
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        with self.runtime.db() as db:
            events = db.execute("SELECT id,status FROM runtime_events WHERE id=?", (key,)).fetchall()
        self.assertEqual([(r[0],r[1]) for r in events], [(key,'pending')])
        self.assertEqual(self.runtime.agent(self.key)['status'], 'queued')

    def test_supervisor_restart_adopts_exact_active_turn_then_delivers_completion(self):
        parent_id = self.make_worker()
        self.server.native['status']['type'] = 'active'
        self.server.native['turns'][0]['status'] = 'inProgress'
        self.restart(supervisor=True)
        self.assertTrue(self.runtime.supervisor_mode)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['restartRecovery']['at'] = time.time() - 31
            self.runtime.put(db, 'agents', agent)
        self.assertEqual(self.runtime.agent(self.key)['restartRecovery']['stage'], 'pending')

        result = recover(self.runtime, self.key, automatic=True)

        self.assertEqual(result, {'status': 'adopted', 'turnId': 'lost-turn'})
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['restartRecovery']['stage'], 'continued')
        self.assertEqual(agent['turnId'], 'lost-turn')
        self.assertTrue(agent['inFlight'])
        self.assertTrue(agent['autoWake'])
        self.assertEqual(agent['status'], 'running')
        self.assertFalse(any(method in {'turn/start', 'turn/resume'} for method, _ in self.server.calls))

        self.server.native['turns'][0]['status'] = 'completed'
        self.runtime.notification({'method': 'turn/completed', 'params': {'threadId': 'native-thread',
            'turn': {'id': 'lost-turn', 'status': 'completed'}}}, 'default',
            self.runtime.connection_ids['default'])
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['restartRecovery']['stage'], 'finished')
        self.assertEqual(agent['status'], 'completed')
        with self.runtime.db() as db:
            rows = db.execute("SELECT id,text FROM runtime_events WHERE agent=? AND kind='child_result'",
                              (parent_id,)).fetchall()
        self.assertEqual(len(rows), 1)
        self.assertFalse(any(method in {'turn/start', 'turn/resume'} for method, _ in self.server.calls))

    def test_only_exact_live_reattach_restores_turn_monitors_and_background_tasks(self):
        self.update(status='running', autoWake=True, inFlight=True, turnId='lost-turn')
        with self.runtime.db() as db:
            self.runtime.put(db, 'tasks', {'id':'active-task','agent':self.key,'epoch':self.a['epoch'],
                'turnId':'lost-turn','kind':'command','status':'running','processId':'child-process'})
            self.runtime.put(db, 'monitors', {'id':'active-monitor','agent':self.key,'epoch':self.a['epoch'],
                'turnId':'lost-turn','status':'running','operation':{'accountKey':'default'}})
        agent = self.restart(supervisor=True)
        self.assertEqual(agent['status'], 'interrupted')
        def record(table, key):
            with self.runtime.db() as db:
                return json.loads(db.execute(f'SELECT record FROM runtime_{table} WHERE id=?',
                                             (key,)).fetchone()[0])
        self.assertEqual(record('monitors','active-monitor')['status'], 'lost')
        self.assertEqual(record('tasks','active-task')['status'], 'lost')

        self.runtime.supervisor_reattached('default', self.runtime.connection_ids['default'], False)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'interrupted')
        self.assertEqual(record('monitors','active-monitor')['status'], 'lost')
        self.assertEqual(record('tasks','active-task')['status'], 'lost')

        self.runtime.supervisor_reattached('default', self.runtime.connection_ids['default'], True)
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'running')
        self.assertTrue(agent['inFlight'])
        self.assertEqual(agent['turnId'], 'lost-turn')
        self.assertEqual(agent['restartRecovery']['stage'], 'reattached')
        self.assertEqual(record('monitors','active-monitor')['status'], 'running')
        self.assertEqual(record('tasks','active-task')['status'], 'running')
        self.assertFalse(any(method in {'turn/start','turn/resume'} for method, _ in self.server.calls))

    def test_startup_opens_pending_accounts_before_runtime_is_exposed_and_records_each_result(self):
        self.update(status='running', autoWake=True, inFlight=True, turnId='lost-turn')
        self.runtime.close()
        with patch.dict(os.environ, {'CODEX_AGENTS_SUPERVISOR_MODE': '1'}):
            self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        claude_agent = copy.deepcopy(self.runtime.agent(self.key))
        claude_agent.update(id=str(uuid.uuid4()), name='Claude account turn', accountKey='claude-local',
                            threadId='claude-thread', turnId='claude-turn', status='interrupted',
                            autoWake=False, inFlight=False,
                            restartRecovery={'stage': 'pending', 'autoWake': True, 'epoch': claude_agent['epoch'],
                                'accountKey': 'claude-local', 'threadId': 'claude-thread',
                                'turnId': 'claude-turn'})
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', claude_agent)
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'interrupted')
        self.assertEqual(agent['supervisorRestore']['status'], 'pending')

        opened = []
        def open_account(account_key):
            with self.runtime.db() as db:
                current = [a for a in self.runtime.records(db, 'agents')
                           if a.get('accountKey', 'default') == account_key
                           and (a.get('restartRecovery') or {}).get('stage') == 'pending']
            opened.append((account_key, {a['id']: a['status'] for a in current}))
            connection = 'startup-' + account_key
            self.runtime.connection_ids[account_key] = connection
            self.runtime.offline_accounts.discard(account_key)
            self.runtime.supervisor_reattached(account_key, connection, True)
        with patch.object(self.runtime, 'connect', side_effect=open_account):
            self.runtime._restore_startup_supervisor_handles()

        self.assertEqual([row[0] for row in opened], ['claude-local', 'default'])
        self.assertTrue(all(set(row[1].values()) == {'interrupted'} for row in opened))
        restored = self.runtime.agent(self.key)
        self.assertEqual(restored['status'], 'running')
        self.assertEqual(restored['restartRecovery']['stage'], 'reattached')
        self.assertEqual(restored['supervisorRestore']['status'], 'restored')
        self.assertEqual(restored['supervisorRestore']['reason'], 'live_handle_resumed')
        claude_restored = self.runtime.agent(claude_agent['id'])
        self.assertEqual(claude_restored['status'], 'running')
        self.assertEqual(claude_restored['supervisorRestore']['status'], 'restored')
        self.assertFalse(any(method in {'turn/start', 'turn/resume'} for method, _ in self.server.calls))

    def test_startup_failed_handle_open_leaves_turn_interrupted_with_reason(self):
        self.update(status='running', autoWake=True, inFlight=True, turnId='lost-turn')
        self.runtime.close()
        with patch.dict(os.environ, {'CODEX_AGENTS_SUPERVISOR_MODE': '1'}):
            self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)

        with patch.object(self.runtime, 'connect', side_effect=RuntimeError('no compatible supervisor')):
            self.runtime._restore_startup_supervisor_handles()

        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'interrupted')
        self.assertEqual(agent['supervisorRestore']['status'], 'not_restored')
        self.assertEqual(agent['supervisorRestore']['reason'], 'account_handle_open_failed')
        self.assertEqual(agent['restartRecovery']['stage'], 'pending')

    def test_startup_timeout_closes_runtime_without_cancelling_restore_or_exposing_api(self):
        self.update(status='running', autoWake=True, inFlight=True, turnId='lost-turn')
        self.runtime.close()
        pending = concurrent.futures.Future()
        observed = []
        def connect(runtime, _account):
            observed.append(runtime)
            return SimpleNamespace(supervisor_reattach_future=pending)
        with patch.dict(os.environ, {'CODEX_AGENTS_SUPERVISOR_MODE': '1'}), \
                patch.object(fixture.Runtime, 'connect', connect), \
                patch.object(pending, 'result', side_effect=concurrent.futures.TimeoutError):
            with self.assertRaisesRegex(RuntimeError, 'restore did not finish before startup'):
                fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.assertEqual(len(observed), 1)
        self.assertTrue(observed[0].closed)
        self.assertFalse(pending.done())
        self.assertFalse(pending.cancelled())
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'interrupted')
        self.assertFalse(self.runtime.agent(self.key)['autoWake'])

    def test_reattach_rechecks_connection_and_close_after_runtime_lock_wait(self):
        for change in ('connection', 'closed'):
            with self.subTest(change=change):
                expected = self.restart(supervisor=True)
                connection = self.runtime.connection_ids['default']
                checked = threading.Event()
                finished = threading.Event()
                original = self.runtime.connection_current
                errors = []

                def observe(account, identity):
                    value = original(account, identity)
                    checked.set()
                    return value

                def restore():
                    try:
                        self.runtime.supervisor_reattached('default', connection, True)
                    except Exception as error:
                        errors.append(error)
                    finally:
                        finished.set()

                with patch.object(self.runtime, 'connection_current', side_effect=observe):
                    with self.runtime.lock:
                        worker = threading.Thread(target=restore)
                        worker.start()
                        self.assertTrue(checked.wait(2))
                        if change == 'connection':
                            self.runtime.connection_ids['default'] = 'replacement-connection'
                        else:
                            self.runtime.closed = True
                    try:
                        self.assertTrue(finished.wait(2))
                        self.assertEqual(errors, [])
                        self.assertEqual(self.runtime.agent(self.key), expected)
                    finally:
                        self.runtime.closed = False
                        self.runtime.connection_ids['default'] = connection
                        worker.join(timeout=2)

    def test_restart_uncertainty_hold_is_reserved_for_unconfirmed_native_identity(self):
        self.restart()
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['restartRecovery']['at'] = time.time() - 31
            self.runtime.put(db, 'agents', agent)
        self.server.read_error = RuntimeError('native read unavailable')
        result = recover(self.runtime, self.key, automatic=True)
        self.assertEqual(result['status'], 'unconfirmed')
        connection_recovery_tick(self.runtime, [self.runtime.agent(self.key)])
        self.assertEqual(self.runtime.agent(self.key)['restartRecovery']['stage'], 'held')
        self.assertIn('could not confirm', self.runtime.agent(self.key)['restartRecovery']['reason'])

    def test_codex_disconnected_notice_is_cleared_only_after_same_child_reattach(self):
        self.update(status='running', autoWake=True, inFlight=True, turnId='lost-turn')
        connection = self.runtime.connection_ids['default']
        self.runtime.disconnected('default', connection)
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'interrupted')
        self.assertIn('Codex disconnected', agent['error'])

        self.runtime.supervisor_reattached('default', connection, False)
        self.assertEqual(self.runtime.agent(self.key)['status'], 'interrupted')
        replacement = 'reattached-connection'
        self.runtime.connection_ids['default'] = replacement
        self.runtime.offline_accounts.discard('default')
        self.runtime.supervisor_reattached('default', replacement, True)
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'running')
        self.assertTrue(agent['inFlight'])
        self.assertEqual(agent['turnId'], 'lost-turn')
        self.assertIsNone(agent['error'])

    def test_turn_finished_during_restart_uses_normal_completion_and_dispatches_pending_input(self):
        parent_id = self.make_worker()
        with self.runtime.db() as db:
            self.runtime.enqueue(db, self.runtime.agent(self.key, db), 'user', 'Follow-up', 'follow-up')
        self.server.native['status']['type'] = 'idle'
        self.server.native['turns'][0]['status'] = 'completed'
        self.restart()
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='pending' WHERE id='follow-up'")

        result = recover(self.runtime, self.key, automatic=True)

        self.assertEqual(result['status'], 'reconciled')
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['restartRecovery']['stage'], 'finished')
        self.assertEqual(agent['status'], 'queued')
        self.assertTrue(agent['autoWake'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='follow-up'").fetchone()[0],
                             'pending')
            rows = db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND kind='child_result'",
                              (parent_id,)).fetchone()[0]
        self.assertEqual(rows, 1)
        self.assertFalse(any(method in {'turn/start', 'turn/resume'} for method, _ in self.server.calls))

    def test_old_restart_turn_completion_preserves_newer_active_turn(self):
        parent_id = self.make_worker()
        self.server.native['status']['type'] = 'active'
        self.server.native['turns'][0]['status'] = 'completed'
        self.restart(supervisor=True)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent.update(status='running', autoWake=True, inFlight=True, turnId='newer-turn')
            self.runtime.put(db, 'agents', agent)
        self.assertEqual(self.runtime.agent(self.key)['turnId'], 'newer-turn')

        result = recover(self.runtime, self.key, automatic=True)

        self.assertEqual(result, {'status': 'reconciled', 'turnId': 'lost-turn', 'outcome': 'completed'})
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['restartRecovery']['stage'], 'finished')
        self.assertEqual(agent['turnId'], 'newer-turn')
        self.assertEqual(agent['status'], 'running')
        self.assertTrue(agent['inFlight'])
        self.assertTrue(agent['autoWake'])
        with self.runtime.db() as db:
            rows = db.execute("SELECT id FROM runtime_events WHERE agent=? AND kind='child_result'",
                              (parent_id,)).fetchall()
        self.assertEqual(len(rows), 1)
        self.assertFalse(any(method in {'turn/start', 'turn/resume'} for method, _ in self.server.calls))

    def test_completion_callback_after_restart_keeps_saved_wake_permission(self):
        self.make_worker()
        self.restart()
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'interrupted')
        self.assertFalse(agent['autoWake'])
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['restartRecovery']['stage'] = 'superseded'
            self.runtime.put(db, 'agents', agent)

        self.runtime.notification({'method': 'turn/completed', 'params': {'threadId': 'native-thread',
            'turn': {'id': 'lost-turn', 'status': 'completed'}}}, 'default',
            self.runtime.connection_ids['default'])

        agent = self.runtime.agent(self.key)
        self.assertNotEqual(agent['status'], 'paused')
        self.assertTrue(agent['autoWake'])
        self.assertEqual(agent['restartRecovery']['stage'], 'finished')

    def test_unreadable_restart_turn_is_held_after_bounded_wait(self):
        parent_id = self.make_worker()
        self.restart()
        self.server.read_error = RuntimeError('native read unavailable')
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['restartRecovery']['at'] = time.time() - 31
            self.runtime.put(db, 'agents', agent)

        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'unconfirmed')
        connection_recovery_tick(self.runtime, [self.runtime.agent(self.key)])

        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['restartRecovery']['stage'], 'held')
        self.assertIn('could not confirm', agent['restartRecovery']['reason'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND kind='child_result'",
                                        (parent_id,)).fetchone()[0], 1)

    def test_restart_permission_recovers_paused_agent_when_epoch_stays_current(self):
        self.make_worker()
        self.server.native['status']['type'] = 'active'
        self.server.native['turns'][0]['status'] = 'inProgress'
        self.restart(supervisor=True)
        self.update(status='paused', autoWake=False)

        result = recover(self.runtime, self.key, automatic=True)

        agent = self.runtime.agent(self.key)
        self.assertEqual(result['status'], 'adopted')
        self.assertEqual(agent['restartRecovery']['stage'], 'continued')
        self.assertEqual(agent['status'], 'running')
        self.assertTrue(agent['autoWake'])

    def test_restore_marked_paused_restart_superseded_but_exact_authority_recovers(self):
        self.make_worker()
        self.server.native['status']['type'] = 'active'
        self.server.native['turns'][0]['status'] = 'inProgress'
        self.restart(supervisor=True)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent.update(status='paused', autoWake=False)
            agent['restartRecovery']['stage'] = 'superseded'
            self.runtime.put(db, 'agents', agent)

        result = recover(self.runtime, self.key, automatic=True)

        self.assertEqual(result['status'], 'adopted')
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['restartRecovery']['stage'], 'continued')
        self.assertEqual(agent['status'], 'running')
        self.assertTrue(agent['autoWake'])

    def test_restart_permission_reconciles_completed_agent_after_callback_paused_it(self):
        self.make_worker()
        self.server.native['status']['type'] = 'idle'
        self.server.native['turns'][0]['status'] = 'completed'
        self.restart()
        self.update(status='paused', autoWake=False, inFlight=False, turnId=None)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['restartRecovery']['stage'] = 'superseded'
            self.runtime.put(db, 'agents', agent)

        result = recover(self.runtime, self.key, automatic=True)

        self.assertEqual(result['status'], 'reconciled')
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['restartRecovery']['stage'], 'finished')
        self.assertEqual(agent['lastCompletedTurn'], 'lost-turn')
        self.assertTrue(agent['autoWake'])

    def test_later_explicit_stop_supersedes_restart_receipt_and_stays_paused(self):
        self.make_worker()
        self.restart()
        self.update(status='paused', autoWake=False, inFlight=False, epoch=1)

        connection_recovery_tick(self.runtime, [self.runtime.agent(self.key)])

        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'paused')
        self.assertFalse(agent['autoWake'])
        self.assertEqual(agent['restartRecovery']['stage'], 'superseded')

    def test_disconnect_receipt_keeps_restart_continuation_permission(self):
        self.update(status='running', autoWake=True, inFlight=True, turnId='lost-turn')
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.runtime.disconnected('default', self.runtime.connection_ids['default'])
        disconnected = self.runtime.agent(self.key)
        self.assertFalse(disconnected['autoWake'])
        self.assertTrue(disconnected['disconnectRecovery']['autoWake'])
        native = copy.deepcopy(self.server.native)
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.server.native = native
        self.server.calls.clear()
        recovered = self.runtime.agent(self.key)
        self.assertEqual(recovered['restartRecovery']['stage'], 'pending')
        self.assertTrue(recovered['restartRecovery']['autoWake'])
        self.assertTrue(recover(self.runtime, self.key, automatic=True)['continued'])
        self.assertTrue(self.runtime.agent(self.key)['autoWake'])

    def test_unsubmitted_input_restores_exact_batch(self):
        with self.runtime.db() as db:
            a=self.runtime.agent(self.key,db)
            self.runtime.enqueue(db,a,'user','Never submitted','reserved-input')
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id='reserved-input'")
        self.restart(startAttempt={'id':'attempt','epoch':self.a['epoch'],'accountKey':'default',
                                   'submitted':False,'events':['reserved-input']})
        a=self.runtime.agent(self.key)
        self.assertEqual(a['status'],'queued')
        self.assertTrue(a['autoWake'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='reserved-input'").fetchone()[0],'pending')

    def failed_preparation(self):
        attempt = {'id': 'failed-preparation-attempt', 'epoch': self.a['epoch'],
                   'accountKey': 'default', 'threadId': 'native-thread',
                   'events': ['failed-preparation-input'], 'submitted': False}
        error = 'Cannot verify the existing supervisor child; native outcome remains unknown'
        with self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            self.runtime.enqueue(db, agent, 'user', 'Keep this instruction',
                                 'failed-preparation-input')
            agent.update(status='failed', autoWake=True, inFlight=False, turnId=None,
                         threadId='native-thread', error=error, startAttempt=attempt)
            self.runtime.put(db, 'agents', agent)
        return attempt, error

    def test_disconnect_preserves_eligible_failed_preparation_attempt(self):
        attempt, error = self.failed_preparation()
        self.runtime.disconnected('default', self.runtime.connection_ids['default'])
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'failed')
        self.assertEqual(agent['error'], error)
        self.assertEqual(agent['startAttempt'], attempt)

    def test_restart_preserves_eligible_failed_preparation_attempt(self):
        attempt, error = self.failed_preparation()
        self.runtime.close()
        # Restore the pending receipt for the abrupt-exit state under test.
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='pending',error=NULL,turn_id=NULL "
                       "WHERE id='failed-preparation-input' AND agent=? AND epoch=?",
                       (self.key, attempt['epoch']))
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'failed')
        self.assertEqual(agent['error'], error)
        self.assertEqual(agent['startAttempt'], attempt)
        with self.runtime.db() as db:
            self.assertEqual(tuple(db.execute(
                "SELECT status,turn_id FROM runtime_events WHERE id='failed-preparation-input'"
            ).fetchone()), ('pending', None))

        result = recover(self.runtime, self.key)

        self.assertEqual(result, {'status': 'input_restored', 'attemptId': attempt['id']})
        agent = self.runtime.agent(self.key)
        self.assertEqual(agent['status'], 'queued')
        self.assertTrue(agent['autoWake'])
        self.assertNotIn('startAttempt', agent)
        self.assertEqual(agent['connectionRecovery']['outcome'], 'input_restored')
        repeated = recover(self.runtime, self.key)
        self.assertNotEqual(repeated.get('status'), 'input_restored')
        with self.runtime.db() as db:
            rows = db.execute("SELECT id,status,turn_id FROM runtime_events "
                              "WHERE id='failed-preparation-input'").fetchall()
        self.assertEqual([tuple(row) for row in rows],
                         [('failed-preparation-input', 'pending', None)])

    def test_scenario_4_disconnect_preserves_first_preparation_receipt(self):
        with self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            self.runtime.enqueue(db, a, 'user', 'Original instruction', 'preparation-input')
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id='preparation-input'")
            a.update(threadId=None, turnId=None, status='starting', inFlight=True,
                autoWake=True, startAttempt={'id':'preparation-attempt','epoch':a['epoch'],
                    'accountKey':'default','threadId':None,'events':['preparation-input'],
                    'submitted':False})
            self.runtime.put(db, 'agents', a)
        self.runtime.disconnected('default', self.runtime.connection_ids['default'])
        disconnected = self.runtime.agent(self.key)
        self.assertEqual(disconnected['disconnectRecovery']['startAttempt']['id'],
                         'preparation-attempt')
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.server.pending = {}
        a = self.runtime.agent(self.key)
        self.assertEqual(a['status'], 'queued')
        self.assertTrue(a['autoWake'])
        self.assertEqual(a['restartRecovery']['stage'], 'input_restored')
        self.assertEqual(a['connectionRecovery']['attemptId'], 'preparation-attempt')
        with self.runtime.db() as db:
            self.assertEqual(tuple(db.execute(
                "SELECT status,turn_id FROM runtime_events WHERE id='preparation-input'").fetchone()),
                ('pending', None))
        with patch('codex_native_tools.gate', return_value=True):
            self.runtime.dispatch()
        deadline = time.monotonic() + 5
        starts = []
        while time.monotonic() < deadline:
            starts = [params for method, params in self.server.calls
                      if method == 'turn/start' and params.get('clientUserMessageId') == 'preparation-input']
            if starts:
                break
            time.sleep(.02)
        self.assertEqual(len(starts), 1)

    def test_scenario_4_disconnect_after_thread_preparation_keeps_one_input(self):
        with self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            self.runtime.enqueue(db, a, 'user', 'Original instruction', 'prepared-input')
            db.execute("UPDATE runtime_events SET status='reserved' WHERE id='prepared-input'")
            a.update(threadId='prepared-thread', turnId=None, status='starting', inFlight=True,
                autoWake=True, startAttempt={'id':'prepared-attempt','epoch':a['epoch'],
                    'accountKey':'default','threadId':'prepared-thread',
                    'events':['prepared-input'],'submitted':False})
            self.runtime.put(db, 'agents', a)
        self.runtime.disconnected('default', self.runtime.connection_ids['default'])
        self.runtime.close()
        self.runtime = fixture.Runtime(Path(self.temp.name), fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.server.pending = {}
        self.server.callbacks = queue.Queue()
        self.server.clock_replies = queue.Queue()
        self.server.native = {'id':'prepared-thread','status':{'type':'idle'},'turns':[]}
        a = self.runtime.agent(self.key)
        self.assertEqual(a['restartRecovery']['stage'], 'input_restored')
        self.assertEqual(a['connectionRecovery']['attemptId'], 'prepared-attempt')
        with patch('codex_native_tools.gate', return_value=True):
            self.runtime.dispatch()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            starts = [params for method, params in self.server.calls if method == 'turn/start']
            if starts:
                break
            time.sleep(.02)
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['clientUserMessageId'], 'prepared-input')
        with self.runtime.db() as db:
            rows = db.execute("SELECT id,status FROM runtime_events WHERE id='prepared-input'").fetchall()
        self.assertEqual(len(rows), 1)
        self.assertIn(rows[0]['status'], {'dispatching', 'delivered'})

    def test_scenario_5_reconciles_exact_accepted_input_identity(self):
        with self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            self.runtime.enqueue(db, a, 'user', 'Original instruction', 'exact-native-input')
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id='exact-native-input'")
        self.server.native['turns'].append({'id':'accepted-turn','clientUserMessageId':'exact-native-input',
            'status':'completed','items':[{'id':'input-item','type':'userMessage','clientId':'exact-native-input',
                'content':[{'type':'text','text':'Original instruction'}]}]})
        self.restart(threadId='native-thread', turnId=None, startAttempt={
            'id':'accepted-attempt','epoch':self.a['epoch'],'accountKey':'default',
            'threadId':'native-thread','events':['exact-native-input'],'submitted':True})
        a = self.runtime.agent(self.key)
        self.assertEqual(a['restartRecovery']['stage'], 'held')
        self.assertEqual(a['contextRepairWait']['events'], ['exact-native-input'])
        self.runtime.dispatch()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.runtime.agent(self.key).get('restartRecovery', {}).get('stage') == 'finished':
                break
            time.sleep(.02)
        self.assertEqual(self.runtime.agent(self.key)['restartRecovery']['stage'], 'finished')
        with self.runtime.db() as db:
            self.assertEqual(tuple(db.execute(
                "SELECT status,turn_id FROM runtime_events WHERE id='exact-native-input'").fetchone()),
                ('delivered','accepted-turn'))

    def test_scenario_5_proves_input_was_not_accepted_before_restart(self):
        with self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            self.runtime.enqueue(db, a, 'user', 'Original instruction', 'not-accepted-input')
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id='not-accepted-input'")
        self.server.native['turns'] = [{'id':'older-turn','clientUserMessageId':'older-input',
            'status':'completed','items':[{'id':'older-user','type':'userMessage','clientId':'older-input',
                'content':[{'type':'text','text':'Earlier instruction'}]}]}]
        self.restart(threadId='native-thread', turnId=None, startAttempt={
            'id':'not-accepted-attempt','epoch':self.a['epoch'],'accountKey':'default',
            'threadId':'native-thread','events':['not-accepted-input'],'submitted':True})
        self.runtime.dispatch()
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            with self.runtime.db() as db:
                status = db.execute("SELECT status FROM runtime_events WHERE id='not-accepted-input'").fetchone()[0]
            if status == 'pending':
                break
            time.sleep(.02)
        with self.runtime.db() as db:
            self.assertEqual(db.execute(
                "SELECT status FROM runtime_events WHERE id='not-accepted-input'").fetchone()[0], 'pending')
        self.assertEqual(self.runtime.agent(self.key)['restartRecovery']['stage'], 'finished')

    def test_submitted_input_without_turn_is_held(self):
        parent_id = self.make_worker()
        self.update(turnId=None)
        self.restart(startAttempt={'id':'attempt','epoch':self.a['epoch'],'accountKey':'default',
                                   'submitted':True,'events':['possibly-submitted']})
        a=self.runtime.agent(self.key)
        self.assertFalse(a['autoWake'])
        self.assertEqual(a['restartRecovery']['stage'],'held')
        with self.runtime.db() as db:
            rows = db.execute("SELECT id,text FROM runtime_events WHERE agent=? AND kind='child_result'",
                              (parent_id,)).fetchall()
        self.assertEqual(len(rows), 1)
        payload = json.loads(rows[0]['text'])
        self.assertEqual(payload['status'], 'interrupted')
        self.assertIn('no confirmed turn identity', payload['reason'])

    def test_explicit_pause_after_capture_is_not_undone(self):
        a=self.update(status='running',autoWake=True,inFlight=True)
        capture(a)
        with self.runtime.db() as db:self.runtime.put(db,'agents',a)
        self.runtime.stop(self.key,descendants=False)
        self.runtime.close()
        self.runtime=fixture.Runtime(Path(self.temp.name),fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        a=self.runtime.agent(self.key)
        self.assertEqual(a['status'],'paused')
        self.assertFalse(a['autoWake'])

    def test_lost_command_continues_once_and_names_it(self):
        self.server.native['turns'][0]['status']='interrupted'
        with self.runtime.db() as db:
            self.runtime.put(db,'tasks',{'id':'lost-command','agent':self.key,'status':'running','kind':'command',
                                         'command':'make deploy-preview'})
        self.restart()
        self.assertTrue(recover(self.runtime,self.key,automatic=True)['continued'])
        self.assertNotIn('continued',recover(self.runtime,self.key,automatic=True))
        a=self.runtime.agent(self.key)
        self.assertTrue(a['autoWake'])
        self.assertEqual(a['restartRecovery']['stage'], 'continued')
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db,'tasks')[0]['status'],'lost')
            rows=db.execute("SELECT text FROM runtime_events WHERE id=?",(a['restartRecovery']['eventId'],)).fetchall()
        self.assertEqual(len(rows),1)
        self.assertIn('make deploy-preview',rows[0][0])

    def test_unknown_input_delivery_blocks_continuation(self):
        self.server.native['turns'][0]['status']='interrupted'
        self.restart()
        with self.runtime.db() as db:
            a=self.runtime.agent(self.key,db)
            db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
                       ('unknown-input',self.key,'user','Maybe sent','uncertain',0,a['epoch'],None,None))
        self.assertNotIn('continued',recover(self.runtime,self.key,automatic=True))
        self.assertFalse(self.runtime.agent(self.key)['autoWake'])

    def test_manual_reconciliation_releases_restart_gate_without_followup(self):
        from codex_context_repair import _local_idle
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.restart()
        self.assertEqual(recover(self.runtime, self.key)['status'], 'reconciled')
        a = self.runtime.agent(self.key)
        self.assertEqual(a['restartRecovery']['stage'], 'finished')
        # Reproduce a saved receipt from before the fix.
        a['restartRecovery']['stage'] = 'pending'
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', a)
            self.assertEqual(_local_idle(self.runtime, db, a, None), [])
        current = self.runtime.agent(self.key)
        self.assertEqual(current['restartRecovery']['stage'], 'finished')
        self.assertFalse(current['autoWake'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE id LIKE 'restart:%'").fetchone()[0], 0)
        self.read_calls_only()

    def test_unconfirmed_or_different_restart_receipts_keep_gate(self):
        self.restart()
        recover(self.runtime, self.key)
        a = self.runtime.agent(self.key)
        a['restartRecovery']['stage'] = 'pending'
        for field, value in [('threadId', 'other'), ('epoch', 99), ('turnId', 'new-turn'), ('inFlight', True)]:
            with self.subTest(field=field):
                changed = copy.deepcopy(a)
                changed[field] = value
                self.assertFalse(settle_reconciled(changed))
        for section, field, value in [
            ('connectionRecovery', 'turnId', 'other'),
            ('connectionRecovery', 'at', 0),
            ('connectionRecovery', 'source', 'unverified'),
            ('connectionRecovery', 'outcome', 'unknown'),
            ('restartRecovery', 'stage', 'held'),
            ('disconnectRecovery', 'threadId', 'other'),
        ]:
            with self.subTest(section=section, field=field):
                changed = copy.deepcopy(a)
                changed[section][field] = value
                self.assertFalse(settle_reconciled(changed))

    def test_later_completed_turn_does_not_reopen_reconciled_restart(self):
        from codex_context_repair import _local_idle
        self.server.native['turns'][0]['status'] = 'interrupted'
        self.restart()
        recover(self.runtime, self.key)
        a = self.runtime.agent(self.key)
        a['restartRecovery']['stage'] = 'pending'
        a.update(status='completed', lastCompletedTurn='later-turn',
                 lastCompletedTurnStatus='completed', autoWake=True)
        before = copy.deepcopy(a)
        with self.runtime.db() as db:
            self.assertEqual(_local_idle(self.runtime, db, a, None), [])
        self.assertEqual(a['restartRecovery']['stage'], 'finished')
        self.assertEqual(a['lastCompletedTurn'], 'later-turn')
        # Another process restart must not restore the old interrupted turn.
        with self.runtime.db() as db:
            self.assertFalse(restore(db, before))
        self.assertEqual(before['restartRecovery']['stage'], 'finished')
        self.assertEqual(before['status'], 'completed')
        self.assertIsNone(before['turnId'])
        self.assertTrue(before['autoWake'])
        self.read_calls_only()

    def test_unobserved_native_command_is_named_in_continuation(self):
        self.server.native['turns'][0].update(status='interrupted',items=[
            {'id':'missed-command','type':'commandExecution','status':'inProgress','command':'git push origin'}])
        self.restart()
        self.assertTrue(recover(self.runtime,self.key,automatic=True)['continued'])
        a=self.runtime.agent(self.key)
        self.assertTrue(a['autoWake'])
        with self.runtime.db() as db:
            text=db.execute("SELECT text FROM runtime_events WHERE id=?",(a['restartRecovery']['eventId'],)).fetchone()[0]
        self.assertIn('git push origin',text)

    def test_abrupt_process_exit_restores_only_committed_unsent_input(self):
        child_root=Path(self.temp.name)/'crashed-runtime'
        script="""
import os,sys
from pathlib import Path
sys.path.insert(0,sys.argv[1])
from codex_runtime import Runtime
class Quiet(Runtime):
    def schedule(self): pass
r=Quiet(Path(sys.argv[2]))
a=r.create({'name':'Crash fixture','cwd':sys.argv[2],'prompt':''},draft=True,defer=True)
with r.db() as db:
    a.update(status='starting',autoWake=True,inFlight=True,
        startAttempt={'id':'before-send','epoch':a['epoch'],'accountKey':a['accountKey'],
                      'submitted':False,'events':['committed-input']})
    r.put(db,'agents',a)
    r.enqueue(db,a,'user','Saved before crash','committed-input')
    db.execute("UPDATE runtime_events SET status='reserved' WHERE id='committed-input'")
with r.db() as db:
    db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
        ('not-committed',a['id'],'user','Uncommitted','pending',0,a['epoch'],None,None))
    os._exit(9)
"""
        child=subprocess.run([sys.executable,'-B','-c',script,
            str(Path(__file__).resolve().parents[1]/'scripts'),str(child_root)],capture_output=True,text=True)
        self.assertEqual(child.returncode,9,child.stderr)
        runtime=fixture.Runtime(child_root,fixture.fixture.RecoveryServer)
        self.addCleanup(runtime.close)
        with runtime.db() as db:
            events=db.execute('SELECT id,status FROM runtime_events').fetchall()
            actor=runtime.records(db,'agents')[0]
        self.assertEqual([(r[0],r[1]) for r in events],[('committed-input','pending')])
        self.assertTrue(actor['autoWake'])
        self.assertEqual(actor['status'],'queued')

    def test_crash_capture_uses_persisted_authority(self):
        a=self.update(status='running',autoWake=True,inFlight=True)
        with self.runtime.db() as db:
            self.assertTrue(restore(db,a))
        self.assertEqual(a['restartRecovery']['stage'],'pending')
        self.assertEqual(a['turnId'],'lost-turn')
        self.assertFalse(a['autoWake'])

    def test_new_turn_replaces_previous_completed_recovery_marker(self):
        self.restart()
        recover(self.runtime,self.key,automatic=True)
        a=self.update(status='running',autoWake=True,inFlight=True,turnId='new-turn')
        with self.runtime.db() as db:self.assertTrue(restore(db,a))
        self.assertEqual(a['restartRecovery']['turnId'],'new-turn')
        self.assertEqual(a['restartRecovery']['stage'],'pending')

    def test_local_question_survives_restart_and_answer_is_durable(self):
        self.update(status='waiting',autoWake=True,inFlight=False,turnId=None,error=None)
        question={'id':'local-question','agent':self.key,'epoch':self.a['epoch'],
                  'method':'agent/asyncQuestion','status':'pending',
                  'params':{'questions':[{'id':'choice','question':'Which option?'}]}}
        with self.runtime.db() as db:self.runtime.put(db,'requests',question)
        self.runtime.close()
        self.runtime=fixture.Runtime(Path(self.temp.name),fixture.fixture.RecoveryServer)
        self.addCleanup(self.runtime.close)
        self.assertEqual(self.runtime.answer('local-question',{'answers':{'choice':{'answers':['A']}}}),{'status':'answered'})
        with self.runtime.db() as db:
            row=db.execute("SELECT status,text FROM runtime_events WHERE id='local-question:answer'").fetchone()
            self.assertEqual(row[0],'pending')
            self.assertIn('A',row[1])

    def test_account_change_invalidates_restart_authority(self):
        a=self.update(status='running',autoWake=True,inFlight=True)
        capture(a);a['accountKey']='replacement'
        with self.runtime.db() as db:self.assertFalse(restore(db,a))
        self.assertEqual(a['restartRecovery']['stage'],'superseded')


if __name__=='__main__':unittest.main()
