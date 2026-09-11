#!/usr/bin/env python3
"""Transport recovery restores verified completion without replay or pause bypass."""
import importlib.util
from pathlib import Path
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('connection_fixture', Path(__file__).with_name('connection-recovery-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_connection_recovery import recover, tick
from codex_session_names import identity as name_identity


class AutomaticRecoveryContract(fixture.ConnectionRecoveryContract):
    # Reuse the manual API contract as a compatibility check.
    def authorize(self):
        previous = {key: self.a.get(key) for key in ('epoch', 'accountKey', 'threadId', 'turnId')}
        previous.update(autoWake=True, connectionId='lost-connection', at=time.time())
        self.a = self.update(disconnectRecovery=previous)

    def test_completed_turn_restores_only_known_pending_input(self):
        self.authorize()
        with self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            self.runtime.enqueue(db, agent, 'user', 'A separate pending instruction', 'next-input')
            db.execute("UPDATE runtime_events SET status='pending' WHERE id='next-input'")
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'reconciled')
        actor = self.runtime.agent(self.key)
        self.assertTrue(actor['autoWake'])
        self.assertEqual(actor['status'], 'queued')
        self.assertEqual(actor['lastAnswer'], 'Full final answer')
        self.assertTrue(actor['connectionRecovery']['completionDelivered'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='next-input'").fetchone()[0], 'pending')
        self.read_calls_only()
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'superseded')

    def test_disconnect_records_permission_once_and_preserves_explicit_stop(self):
        self.update(status='running', inFlight=True, autoWake=True)
        connection = self.runtime.connection_ids['default']
        self.runtime.disconnected('default', connection)
        actor = self.runtime.agent(self.key)
        self.assertTrue(actor['disconnectRecovery']['autoWake'])
        self.assertEqual(actor['disconnectRecovery']['connectionId'], connection)
        self.assertFalse(actor['autoWake'])
        before = dict(actor['disconnectRecovery'])
        self.runtime.disconnected('default', connection)
        self.assertEqual(self.runtime.agent(self.key)['disconnectRecovery'], before)
        self.runtime.stop(self.key, descendants=False)
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
        self.assertEqual(self.runtime.agent(self.key)['status'], 'paused')

    def test_disconnect_during_explicit_stop_preserves_pause(self):
        self.update(status='paused', inFlight=True, autoWake=False, error='Stopped by user')
        self.runtime.disconnected('default', self.runtime.connection_ids['default'])
        actor = self.runtime.agent(self.key)
        self.assertEqual(actor['status'], 'paused')
        self.assertEqual(actor['error'], 'Stopped by user')
        self.assertFalse(actor['inFlight'])
        self.assertNotIn('disconnectRecovery', actor)
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
        self.assertEqual(self.server.calls, [])

    def test_terminal_failure_or_interruption_does_not_resume(self):
        self.authorize()
        for status in ('failed', 'interrupted'):
            with self.subTest(status=status):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                    db.execute('DELETE FROM runtime_completed_turns')
                self.server.native['turns'][0]['status'] = status
                self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'reconciled')
                self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        self.read_calls_only()

    def test_unknown_tool_process_or_message_blocks_automatic_delivery(self):
        self.authorize()
        cases = [('tasks', {'id': 'unknown', 'agent': self.key, 'status': 'lost'}),
                 ('monitors', {'id': 'unknown', 'agent': self.key, 'status': 'lost'}),
                 ('tool_requests', {'id': 'unknown', 'agent': self.key, 'stage': 'failed', 'outcome': 'unknown'}),
                 ('events', None)]
        for table, record in cases:
            with self.subTest(table=table):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                    db.execute('DELETE FROM runtime_completed_turns')
                    if record:
                        self.runtime.put(db, table, record)
                    else:
                        db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                                   ('unknown', self.key, 'user', 'May have been delivered', 'uncertain', time.time(), self.a['epoch'], 'lost-turn', None))
                self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'reconciled')
                self.assertFalse(self.runtime.agent(self.key)['autoWake'])
                with self.runtime.db() as db:
                    if record:
                        self.assertEqual(self.runtime.records(db, table)[0], record)
                    else:
                        self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='unknown'").fetchone()[0], 'uncertain')
                    db.execute(f'DELETE FROM runtime_{table}')
        self.read_calls_only()

    def test_legacy_or_stale_permission_never_restores_auto_wake(self):
        self.authorize()
        marker = self.a['disconnectRecovery']
        cases = [None, {**marker, 'autoWake': False}, {**marker, 'epoch': -1},
                 {**marker, 'accountKey': 'other'}, {**marker, 'turnId': 'other'}]
        for previous in cases:
            with self.subTest(previous=previous):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', {**self.a, 'disconnectRecovery': previous})
                    db.execute('DELETE FROM runtime_completed_turns')
                self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'reconciled')
                self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        self.read_calls_only()

    def test_scheduler_checks_legacy_disconnect_in_background_and_backs_off(self):
        self.server.read_error = TimeoutError('Transient native read failure')
        with patch('codex_connection_recovery.time.time', return_value=100):
            tick(self.runtime, [self.runtime.agent(self.key)])
            fixture.fixture.fixture.eventually(lambda: not self.runtime._connection_recovery_busy)
            calls = list(self.server.calls)
            self.assertTrue(calls)
            tick(self.runtime, [self.runtime.agent(self.key)])
            self.assertEqual(self.server.calls, calls)
        with patch('codex_connection_recovery.time.time', return_value=116):
            tick(self.runtime, [self.runtime.agent(self.key)])
            fixture.fixture.fixture.eventually(lambda: not self.runtime._connection_recovery_busy)
            self.assertEqual(len(self.server.calls), 2 * len(calls))
        self.assertEqual(self.runtime.agent(self.key), self.a)
        self.read_calls_only()

    def test_explicit_account_disconnect_blocks_automatic_checks(self):
        self.authorize()
        with patch.object(self.runtime.accounts, 'get', return_value={'disconnected': True}):
            tick(self.runtime, [self.runtime.agent(self.key)])
            self.assertFalse(getattr(self.runtime, '_connection_recovery_busy', False))
            self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
        self.assertEqual(self.server.calls, [])
        self.assertEqual(self.runtime.agent(self.key), self.a)

    def test_scheduler_has_one_probe_and_pause_wins_while_native_read_waits(self):
        self.authorize()
        self.server.read_gate = threading.Event()
        try:
            tick(self.runtime, [self.runtime.agent(self.key)])
            self.assertTrue(self.server.read_entered.wait(2))
            calls = list(self.server.calls)
            tick(self.runtime, [self.runtime.agent(self.key)])
            self.assertEqual(self.server.calls, calls)
            self.update(status='paused', epoch=self.a['epoch'] + 1, error='Stopped by user')
            self.server.read_gate.set()
            fixture.fixture.fixture.eventually(lambda: not self.runtime._connection_recovery_busy)
            self.assertEqual(self.runtime.agent(self.key)['status'], 'paused')
            self.assertFalse(self.runtime.agent(self.key)['autoWake'])
        finally:
            self.server.read_gate.set()

    def test_dispatch_reconnects_same_account_and_only_reads_original_thread(self):
        native = self.server.native
        previous = self.server
        self.update(status='running', inFlight=True, autoWake=True)
        self.runtime.disconnected('default', self.runtime.connection_ids['default'])
        def replacement(*args):
            server = fixture.fixture.RecoveryServer(*args)
            server.native = native
            return server
        self.runtime.factory = replacement
        self.update(nativeNameSynced=name_identity(self.runtime.agent(self.key)))
        self.runtime.dispatch()
        fixture.fixture.fixture.eventually(lambda: not self.runtime._connection_recovery_busy)
        self.server = self.runtime.servers['default']
        self.assertIsNot(self.server, previous)
        self.assertTrue(previous.closed)
        actor = self.runtime.agent(self.key)
        self.assertEqual(actor['threadId'], 'native-thread')
        self.assertEqual(actor['status'], 'completed')
        self.assertTrue(actor['autoWake'])
        self.assertTrue(actor['connectionRecovery']['completionDelivered'])
        self.read_calls_only()

    def test_automatic_apply_rechecks_permission_and_account_transfer(self):
        self.authorize()
        for change in ({'epoch': 23, 'status': 'paused', 'error': 'Stopped by user'},
                       {'accountTransferId': 'transfer'}, {'accountKey': 'other'},
                       {'disconnectRecovery': None}):
            with self.subTest(change=change):
                with self.runtime.db() as db:
                    self.runtime.put(db, 'agents', self.a)
                self.server.before_apply = lambda: self.update(**change)
                self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
                self.assertEqual(self.runtime.agent(self.key), {**self.a, **change})
        self.read_calls_only()

    def test_parent_receives_exact_completed_result_once(self):
        parent = self.update(autoWake=True, status='waiting', error=None,
                             threadId='parent-thread', turnId=None)
        worker = self.runtime.create({'name': 'Worker', 'prompt': 'Review', 'role': 'reviewer'}, parent=self.key, defer=True)
        self.key = worker['id']
        self.disconnect(self.key)
        self.a = self.runtime.agent(self.key)
        self.authorize()
        self.server.calls.clear()
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'reconciled')
        self.assertEqual(recover(self.runtime, self.key, automatic=True)['status'], 'superseded')
        with self.runtime.db() as db:
            rows = list(db.execute("SELECT id,text,status FROM runtime_events WHERE agent=? AND kind='child_result'", (parent['id'],)))
        self.assertEqual(len(rows), 1)
        self.assertIn('Full final answer', rows[0][1])
        self.assertEqual(rows[0][2], 'pending')
        self.read_calls_only()


if __name__ == '__main__':
    unittest.main(verbosity=2)
