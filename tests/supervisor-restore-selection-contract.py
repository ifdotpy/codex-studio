#!/usr/bin/env python3
"""Supervisor restore reads current receipts without decoding terminal history."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
from contextlib import contextmanager
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime


class NoNativeServer:
    calls = 0

    def __init__(self, *_args, **_kwargs):
        type(self).calls += 1
        raise AssertionError('This fixture cannot create a native transport')


class SupervisorRestoreSelectionContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-supervisor-selection-')
        self.addCleanup(self.temp.cleanup)
        self.native_before = NoNativeServer.calls
        with patch.object(Runtime, 'schedule', lambda _runtime: None):
            self.runtime = Runtime(Path(self.temp.name), NoNativeServer)
        self.addCleanup(self.runtime.close)
        self.runtime.connection_ids['default'] = 'fixture-connection'
        self.owner = self.agent('fixture-owner')
        self.store('agents', self.owner)

    def tearDown(self):
        self.assertEqual(NoNativeServer.calls, self.native_before)

    def agent(self, key, *, account='default', **values):
        agent = {'id': key, 'name': key, 'rootId': key, 'isLead': True,
            'parentId': None, 'threadId': key + '-thread', 'turnId': key + '-turn',
            'status': 'interrupted', 'autoWake': False, 'inFlight': False,
            'epoch': 3, 'turnEpoch': 3, 'events': 0, 'created': 1,
            'model': 'fixture-model', 'effort': 'medium', 'provider': 'codex',
            'role': 'orchestrator', 'cwd': self.temp.name, 'prompt': 'Fixture',
            'tokensUsed': 0, 'tokenBudget': None, 'tail': '',
            'error': 'Server restarted during a turn.', 'accountKey': account,
            'restartRecovery': {'stage': 'pending', 'autoWake': True, 'epoch': 3,
                'accountKey': account, 'threadId': key + '-thread', 'turnId': key + '-turn'}}
        agent.update(values)
        return agent

    def record(self, key, *, owner=None, status='lost', receipt_status='running', **values):
        owner = owner or self.owner
        record = {'id': key, 'agent': owner['id'], 'epoch': owner['epoch'],
            'turnId': owner['turnId'], 'status': status, 'created': 1, 'finished': 40,
            'error': 'Native outcome unknown.', 'kind': 'command',
            'processId': key + '-process', 'tail': 'Fixture output',
            'operation': {'id': key + '-operation', 'accountKey': owner.get('accountKey', 'default'),
                'agent': owner['id'], 'epoch': owner['epoch'], 'connectionId': 'fixture-connection'},
            'reattachRecovery': {'accountKey': owner.get('accountKey', 'default'),
                'epoch': owner['epoch'], 'status': receipt_status,
                'error': 'Saved native warning', 'finished': None}}
        record.update(values)
        return record

    def store(self, table, *records):
        with self.runtime.db() as db:
            db.executemany(f'INSERT INTO runtime_{table}(id,record) VALUES (?,?)',
                           [(record['id'], json.dumps(record)) for record in records])

    def stored(self, table, key):
        with self.runtime.read_db() as db:
            return json.loads(db.execute(f'SELECT record FROM runtime_{table} WHERE id=?',
                                         (key,)).fetchone()[0])

    def restore(self, account='default', connection='fixture-connection', resumed=True):
        self.runtime.supervisor_reattached(account, connection, resumed)

    @contextmanager
    def observe_selection(self):
        queries, decoded_history = [], []
        original_db, original_loads = self.runtime.notification_db, json.loads

        def selection_frame():
            frame = sys._getframe(1)
            while frame and frame.f_code.co_filename != Runtime.supervisor_reattached.__code__.co_filename:
                frame = frame.f_back
            return frame and frame.f_code.co_name in {'supervisor_reattached', '_record_supervisor_restore', 'records'}

        def observed_query(query):
            if query.startswith('SELECT record FROM runtime_') and selection_frame():
                queries.append(query)

        @contextmanager
        def observed_db():
            with original_db() as db:
                db.set_trace_callback(observed_query)
                try:
                    yield db
                finally:
                    db.set_trace_callback(None)

        def observed_loads(value, *args, **kwargs):
            if isinstance(value, str) and '"restoreSelectionHistory": true' in value:
                # Runtime.put separately refreshes the bounded monitor UI window.
                # Count the restore selectors, not that existing projection.
                if selection_frame():
                    decoded_history.append(1)
            return original_loads(value, *args, **kwargs)

        with patch.object(self.runtime, 'notification_db', observed_db), \
                patch('codex_runtime.json.loads', side_effect=observed_loads):
            yield queries, decoded_history

    def test_restore_skips_historical_payloads_and_uses_existing_status_indexes(self):
        active_task, active_monitor = self.record('active-task'), self.record('active-monitor')
        self.store('tasks', active_task)
        self.store('monitors', active_monitor)
        other = self.agent('other-account-owner', account='other-account', restoreSelectionHistory=True)
        self.store('agents', other)
        for table in ('tasks', 'monitors'):
            history = [self.record(f'{table}-history-{index}', status='completed',
                reattachRecovery=None, restoreSelectionHistory=True,
                tail='Fixture history ' * 12) for index in range(4000)]
            history.append(self.record(table + '-other-account', owner=other,
                                       restoreSelectionHistory=True))
            history.append(self.record(table + '-terminal-receipt', receipt_status='completed',
                                       restoreSelectionHistory=True))
            self.store(table, *history)
        history_agents = [self.agent(f'history-agent-{index}', status='completed',
            restartRecovery=None, restoreSelectionHistory=True) for index in range(100)]
        self.store('agents', *history_agents)

        with self.observe_selection() as (queries, decoded_history):
            self.restore()

        self.assertEqual(decoded_history, [], 'Restore must not decode historical or unrelated payloads')
        with self.runtime.read_db() as db:
            for table, index in (('tasks', 'runtime_task_status'),):
                selections = [query for query in queries
                              if query.startswith(f'SELECT record FROM runtime_{table}')]
                self.assertEqual(len(selections), 1)
                plan = [row[3] for row in db.execute('EXPLAIN QUERY PLAN ' + selections[0])]
                self.assertTrue(any('SEARCH' in step and index in step for step in plan), plan)
                self.assertFalse(any(f'SCAN runtime_{table}' in step for step in plan), plan)
        self.assertEqual(self.stored('tasks', active_task['id'])['status'], 'running')
        self.assertFalse(any(query.startswith('SELECT record FROM runtime_monitors') for query in queries))
        self.assertEqual(self.stored('monitors', active_monitor['id']), active_monitor)
        self.assertEqual(self.stored('agents', other['id']), other)

    def test_terminal_records_with_stale_receipts_cannot_become_active(self):
        for table in ('tasks', 'monitors'):
            records = [self.record(table + '-' + status, status=status,
                exitCode=0, finished=41) for status in ('completed', 'failed', 'cancelled', 'interrupted')]
            self.store(table, *records)
            self.restore()
            for record in records:
                self.assertEqual(self.stored(table, record['id']), record)

    def test_task_receipts_restore_and_monitor_unknown_notices_remain(self):
        records = []
        for table in ('tasks', 'monitors'):
            for status in ('running', 'starting', 'approval'):
                record = self.record(table + '-' + status, receipt_status=status)
                if status == 'approval':
                    record['reattachRecovery']['finished'] = 17
                    record['reattachRecovery']['error'] = None
                self.store(table, record)
                records.append((table, record))
        monitor = next(record for table, record in records if table == 'monitors')
        with self.runtime.db() as db:
            for key, status, outcome in (
                ('monitor:' + monitor['id'], 'pending', 'lost'),
                ('monitor:monitors-starting', 'delivered', 'lost'),
                ('monitor:monitors-approval', 'pending', 'completed')):
                db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                    (key, self.owner['id'], 'monitor_exit', json.dumps({'status': outcome}),
                     status, 1, self.owner['epoch'], None, None))

        self.restore()
        restored_owner = self.stored('agents', self.owner['id'])
        self.assertEqual((restored_owner['status'], restored_owner['inFlight'], restored_owner['autoWake']),
                         ('running', True, True))
        self.assertEqual(restored_owner['turnId'], self.owner['turnId'])
        self.assertEqual(restored_owner['epoch'], self.owner['epoch'])
        self.assertEqual(restored_owner['restartRecovery']['stage'], 'reattached')
        self.assertEqual((restored_owner['agentMode'], restored_owner['agentModeRevision'],
                          restored_owner['agentModeSupported']), ('multi', 0, True))
        for table, record in records:
            if table == 'monitors':
                self.assertEqual(self.stored(table, record['id']), record)
                continue
            expected = copy.deepcopy(record)
            receipt = expected.pop('reattachRecovery')
            expected.update(status=receipt['status'], error=receipt['error'])
            if receipt['finished'] is None:
                expected.pop('finished')
            else:
                expected['finished'] = receipt['finished']
            self.assertEqual(self.stored(table, record['id']), expected)
        with self.runtime.read_db() as db:
            states = dict(db.execute('SELECT id,status FROM runtime_events'))
        self.assertEqual(states, {'monitor:monitors-running': 'pending',
            'monitor:monitors-starting': 'delivered', 'monitor:monitors-approval': 'pending'})

    def test_owner_epoch_account_and_deleted_guards_leave_receipts_unchanged(self):
        blocked = [self.agent('stopped-owner', epoch=4, status='paused'),
                   self.agent('deleted-owner', deletedAt=2),
                   self.agent('changed-account-owner', account='other-account')]
        records = []
        self.store('agents', *blocked)
        for table in ('tasks', 'monitors'):
            for owner in blocked:
                record = self.record(table + '-' + owner['id'], owner=owner)
                record['reattachRecovery'].update(accountKey='default', epoch=3)
                self.store(table, record)
                records.append((table, record))
        self.restore()
        for table, record in records:
            self.assertEqual(self.stored(table, record['id']), record)
        stopped = self.stored('agents', blocked[0]['id'])
        self.assertEqual((stopped['status'], stopped['autoWake'], stopped['epoch']), ('paused', False, 4))

    def test_wrong_receipt_account_and_non_active_receipt_leave_current_records_unchanged(self):
        records = []
        for table in ('tasks', 'monitors'):
            for name in ('account', 'receipt-status', 'missing-receipt'):
                record = self.record(table + '-' + name)
                if name == 'account':
                    record['reattachRecovery']['accountKey'] = 'other-account'
                elif name == 'receipt-status':
                    record['reattachRecovery']['status'] = 'completed'
                else:
                    record.pop('reattachRecovery')
                self.store(table, record)
                records.append((table, record))
        self.restore()
        for table, record in records:
            self.assertEqual(self.stored(table, record['id']), record)

    def test_default_account_missing_key_and_disconnect_receipt_keep_existing_semantics(self):
        missing = self.agent('legacy-default')
        missing.pop('accountKey')
        missing['restartRecovery'].pop('accountKey')
        disconnected = self.agent('disconnect-owner', restartRecovery=None)
        disconnected['disconnectRecovery'] = {
            'autoWake': True, 'epoch': 3, 'accountKey': 'default',
            'threadId': disconnected['threadId'], 'turnId': disconnected['turnId']}
        explicit_null = self.agent('null-account', account=None)
        self.store('agents', missing, disconnected, explicit_null)
        self.restore()
        for agent in (missing, disconnected):
            self.assertEqual(self.stored('agents', agent['id'])['status'], 'running')
        self.assertEqual(self.stored('agents', explicit_null['id']), explicit_null)

    def test_restore_rejects_replaced_connection_and_absent_native_resume(self):
        record = self.record('unchanged-task')
        self.store('tasks', record)
        self.restore(connection='replacement-connection')
        self.assertEqual(self.stored('tasks', record['id']), record)
        self.assertEqual(self.stored('agents', self.owner['id'])['status'], 'interrupted')
        self.restore(resumed=False)
        self.assertEqual(self.stored('tasks', record['id']), record)
        self.assertEqual(self.stored('agents', self.owner['id'])['supervisorRestore']['reason'],
                         'native_handle_not_resumed')

    def test_restore_report_decodes_only_pending_agents_in_the_selected_account(self):
        other = self.agent('other-pending', account='other-account', restoreSelectionHistory=True)
        finished = self.agent('finished-agent', status='completed',
            restartRecovery={'stage': 'finished'}, restoreSelectionHistory=True)
        self.store('agents', other, finished)
        with self.observe_selection() as (_queries, decoded_history):
            self.runtime._record_supervisor_restore('default', 'not_restored', 'fixture_reason')
        self.assertEqual(decoded_history, [])
        self.assertEqual(self.stored('agents', other['id']), other)
        self.assertEqual(self.stored('agents', finished['id']), finished)
        self.assertEqual(self.stored('agents', self.owner['id'])['supervisorRestore']['reason'], 'fixture_reason')


if __name__ == '__main__':
    unittest.main()
