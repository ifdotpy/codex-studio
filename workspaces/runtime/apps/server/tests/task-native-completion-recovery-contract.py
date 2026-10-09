#!/usr/bin/env python3
"""Missed exact task results recover without another start or tool execution."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import concurrent.futures
import copy
import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import queue
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('task_recovery_fixture',
    Path(__file__).with_name('old-turn-task-completion-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_task_recovery as recovery
from codex_native_errors import NativeRpcError
from codex_native_tools import _local_idle
from codex_agent_management import _blockers
from codex_sync_entities import sync_task_window


class NativeTaskRecovery(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.addCleanup(self.runtime.close)
        self.agent = self.runtime.create({'name': 'Missed task completion',
            'cwd': str(self.root), 'prompt': 'Fixture'}, defer=True)
        self.runtime.connect()
        with self.runtime.db() as db:
            sync_task_window(db)
        self.update(threadId='fixture-thread', turnId=None, status='completed',
                    autoWake=True, inFlight=False)
        self.server = self.runtime.server
        self.pages = {}
        self.native_calls = []
        self.read_hook = None
        call = self.server.call
        def history(method, params, timeout=60):
            if method != 'thread/items/list':
                return call(method, params, timeout)
            self.assertFalse(self.runtime.lock._is_owned(), 'Native reads must release the runtime lock')
            self.assertGreater(timeout, 0)
            self.assertLessEqual(timeout, recovery.READ_SECONDS)
            self.native_calls.append(copy.deepcopy(params))
            if self.read_hook:
                self.read_hook(params)
            value = self.pages.get((params['turnId'], params.get('cursor')),
                                   {'data': [], 'nextCursor': None})
            if isinstance(value, Exception):
                raise value
            return copy.deepcopy(value)
        self.server.call = history
        self.jobs = []
        submit = self.runtime.recovery_pool.submit
        def capture(function, *args):
            if function is not recovery._run_task_recovery:
                return submit(function, *args)
            self.jobs.append((function, args))
            return concurrent.futures.Future()
        self.capture = patch.object(self.runtime.recovery_pool, 'submit', side_effect=capture)
        self.capture.start()
        self.addCleanup(self.capture.stop)
        self.clock = 1000
        self.clock_patch = patch.object(recovery.time, 'monotonic', side_effect=lambda: self.clock)
        self.clock_patch.start()
        self.addCleanup(self.clock_patch.stop)

    def update(self, **changes):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.agent['id'], db)
            agent.update(changes)
            self.runtime.put(db, 'agents', agent)
        self.agent = agent

    def task(self, key='item', kind='commandExecution', turn='old-turn', **changes):
        task = {'id': self.agent['id'] + ':' + key, 'agent': self.agent['id'],
            'itemId': key, 'turnId': turn, 'type': kind, 'status': 'running', 'created': 1,
            'kind': 'command' if kind == 'commandExecution' else 'tool',
            'processId': 'opaque-session', 'tail': 'Previous output'}
        task.update(changes)
        with self.runtime.db() as db:
            self.runtime.put(db, 'tasks', task)
            self.runtime.item(db, self.agent['id'], key, 'output',
                json.dumps({'id': key, 'type': kind, 'status': 'inProgress'}),
                kind, toolStatus='running', turnId=turn)
        return task

    def receipt(self, task, **changes):
        value = {'id': 'fixture-thread:' + task['itemId'], 'agent': self.agent['id'],
            'accountKey': 'default', 'threadId': 'fixture-thread', 'turnId': task['turnId'],
            'callId': task['itemId'], 'epoch': -1, 'stage': 'completed', 'outcome': 'applied',
            'finished': 2, 'result': {'success': True, 'contentItems': [
                {'type': 'inputText', 'text': 'The saved Studio result'}]}}
        value.update(changes)
        with self.runtime.db() as db:
            self.runtime.put(db, 'tool_requests', value)
            db.execute('INSERT OR IGNORE INTO runtime_tool_request_aliases VALUES (?,?,?)',
                       (self.agent['id'], task['itemId'], value['id']))
        return value

    def native(self, task, **changes):
        item = {'id': task['itemId'], 'type': task['type'], 'status': 'completed', 'exitCode': 0}
        item.update(changes)
        page = self.pages.setdefault((task['turnId'], None), {'data': [], 'nextCursor': None})
        page['data'].append({'turnId': task['turnId'], 'threadId': 'fixture-thread', 'item': item})
        return item

    def saved(self, table, key):
        with self.runtime.read_db() as db:
            row = db.execute(f'SELECT record FROM runtime_{table} WHERE id=?', (key,)).fetchone()
        return json.loads(row[0]) if row else None

    def run_check(self, advance=0):
        self.clock += advance
        scheduled = recovery.queue_task_recovery(self.runtime)
        if scheduled:
            function, args = self.jobs.pop(0)
            function(*args)
        return scheduled

    def absent_command(self, *, native_item=True):
        task = self.task(processId='opaque-native-session')
        if native_item:
            self.native(task, status='inProgress', processId=task['processId'])
        with self.runtime.db() as db:
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)',
                       (task['agent'] + ':' + task['turnId'],))
        self.server.pending = {}
        self.server.callbacks = queue.Queue()
        self.server.clock_replies = queue.Queue()
        self.terminals = {'data': [], 'nextCursor': None}
        self.native_status = 'idle'
        self.absence_calls = []
        self.absence_hook = None
        call = self.server.call
        def native_state(method, params, timeout=60):
            if method not in {'thread/read', 'thread/backgroundTerminals/list'}:
                return call(method, params, timeout)
            self.assertFalse(self.runtime.lock._is_owned())
            self.absence_calls.append((method, copy.deepcopy(params)))
            if self.absence_hook:
                self.absence_hook(method, params)
            if method == 'thread/read':
                return {'thread': {'id': 'fixture-thread', 'status': {'type': self.native_status}}}
            if isinstance(self.terminals, Exception):
                raise self.terminals
            return copy.deepcopy(self.terminals)
        self.server.call = native_state
        return task

    def test_absent_native_session_becomes_lost_without_exit_success_or_replay(self):
        task = self.absent_command()
        agent = copy.deepcopy(self.runtime.agent(task['agent']))
        calls = list(self.server.calls)
        with self.runtime.read_db() as db:
            events = [tuple(row) for row in db.execute('SELECT * FROM runtime_events')]
            budget = [tuple(row) for row in db.execute('SELECT * FROM runtime_budget_usage')]
            self.assertEqual(_local_idle(self.runtime, db, 'default', self.server)[1],
                             'Waiting for a command or request receipt')
        with patch('os.kill', side_effect=AssertionError('Native process IDs are opaque')):
            self.assertTrue(self.run_check())
        saved = self.saved('tasks', task['id'])
        self.assertEqual(saved['status'], 'lost')
        self.assertIn('unknown', saved['error'])
        self.assertIsNone(saved.get('exitCode'))
        self.assertEqual(saved['tail'], task['tail'])
        self.assertEqual(self.saved('items', task['id'])['toolStatus'], 'lost')
        self.assertEqual(self.runtime.agent(task['agent']), agent)
        self.assertEqual(self.server.calls, calls)
        with self.runtime.read_db() as db:
            self.assertEqual([tuple(row) for row in db.execute('SELECT * FROM runtime_events')], events)
            self.assertEqual([tuple(row) for row in db.execute('SELECT * FROM runtime_budget_usage')], budget)
            value = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='task' AND id=?",
                                         (task['id'],)).fetchone()[0])['value']
            self.assertEqual(value['status'], 'lost')
            self.assertIsNone(_local_idle(self.runtime, db, 'default', self.server)[1])
        self.assertEqual({method for method, _ in self.absence_calls},
                         {'thread/read', 'thread/backgroundTerminals/list'})

    def test_missing_native_item_uses_terminal_absence_without_claiming_completion(self):
        task = self.absent_command(native_item=False)
        self.native_status = 'notLoaded'
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'lost')

    def test_native_session_presence_uses_both_item_and_process_identity(self):
        task = self.absent_command()
        for item_id, process_id in ((task['itemId'], task['processId']),
                                    ('different-item', task['processId']),
                                    (task['itemId'], 'different-process')):
            with self.subTest(item=item_id, process=process_id):
                self.terminals = {'data': [{'itemId': item_id, 'processId': process_id}], 'nextCursor': None}
                self.assertTrue(self.run_check(30))
                self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')

    def test_absence_requires_idle_thread_complete_turn_and_valid_native_reads(self):
        task = self.absent_command()
        variants = [TimeoutError('Native terminal read timed out'), {'data': None},
                    {'data': [{}]}, {'data': [], 'nextCursor': 'repeated'}]
        for terminals in variants:
            self.terminals = terminals
            self.assertTrue(self.run_check(30))
            self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.terminals = {'data': [], 'nextCursor': None}
        self.native_status = 'active'
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.native_status = 'idle'
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_completed_turns')
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')

    def test_live_request_active_owner_and_changed_source_prevent_absence_proof(self):
        task = self.absent_command()
        self.server.pending['live-request'] = concurrent.futures.Future()
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.server.pending.clear()
        self.update(status='running', inFlight=True, turnId='new-turn')
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.update(status='completed', inFlight=False, turnId=None)
        self.absence_hook = lambda method, params: self.update(epoch=self.agent['epoch'] + 1)
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')

    def test_event_barrier_receives_completion_before_any_absence_check(self):
        task = self.absent_command()
        barrier = self.server.after_events
        def completion(callback):
            self.runtime.notification({'method': 'item/completed', 'params': {
                'threadId': 'fixture-thread', 'turnId': task['turnId'], 'item': {
                    'id': task['itemId'], 'type': 'commandExecution', 'status': 'completed', 'exitCode': 0}}})
            barrier(callback)
        self.server.after_events = completion
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')

    def test_malformed_item_history_never_proves_absence_but_unsupported_method_can(self):
        task = self.absent_command()
        for page in ({'data': None}, {'data': [] , 'nextCursor': 3},
                     TimeoutError('Native item read timed out'),
                     NativeRpcError({'code': -32000, 'message': 'Native history error'})):
            self.pages[('old-turn', None)] = page
            self.assertTrue(self.run_check(30))
            self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.pages[('old-turn', None)] = NativeRpcError({'code': -32601, 'message': 'Method not found'})
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'lost')

    def test_full_terminal_pagination_protects_live_session_on_later_page(self):
        task = self.absent_command()
        def pages(method, params):
            if method == 'thread/backgroundTerminals/list':
                self.terminals = ({'data': [], 'nextCursor': 'second'} if not params.get('cursor')
                    else {'data': [{'itemId': task['itemId'], 'processId': task['processId']}], 'nextCursor': None})
        self.absence_hook = pages
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.assertEqual([p.get('cursor') for m, p in self.absence_calls
                          if m == 'thread/backgroundTerminals/list'], [None, 'second'])
        self.absence_hook = lambda method, params: setattr(self, 'terminals',
            {'data': [], 'nextCursor': 'second'} if not params.get('cursor') else {'data': [], 'nextCursor': None})
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'lost')

    def test_absence_does_not_follow_copied_history_or_change_during_native_read(self):
        task = self.absent_command()
        self.update(accountHistory=[{'at': 2, 'accountKey': 'old-account', 'threadId': 'old-thread'}])
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.update(accountHistory=None)
        def pending(method, params):
            if method == 'thread/backgroundTerminals/list':
                self.server.pending['live-request'] = concurrent.futures.Future()
        self.absence_hook = pending
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.server.pending.clear()
        def replace(method, params):
            self.runtime.connection_ids['default'] = 'different-connection'
        self.absence_hook = replace
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')

    def test_changed_task_and_pending_events_preserve_unknown_native_command(self):
        task = self.absent_command()
        self.server.callbacks.put('pending-event')
        with patch.object(recovery.threading.Event, 'wait', return_value=False):
            self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.server.callbacks.get_nowait()
        self.server.callbacks.task_done()
        def output(method, params):
            if method == 'thread/backgroundTerminals/list':
                saved = self.saved('tasks', task['id'])
                saved['tail'] = 'New output'
                with self.runtime.db() as db:
                    self.runtime.put(db, 'tasks', saved)
        self.absence_hook = output
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')

    def test_unavailable_terminal_list_preserves_other_exact_native_results(self):
        absent = self.absent_command()
        exact = self.task('exact')
        self.native(exact)
        self.terminals = TimeoutError('Native terminal state is unavailable')
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', absent['id'])['status'], 'running')
        self.assertEqual(self.saved('tasks', exact['id'])['status'], 'completed')

    def test_absence_requires_unique_matching_native_item_identity(self):
        task = self.absent_command()
        entry = copy.deepcopy(self.pages[('old-turn', None)]['data'][0])
        variants = [[entry, entry], [{**entry, 'turnId': 'different-turn'}],
                    [{**entry, 'threadId': 'different-thread'}],
                    [{**entry, 'item': {**entry['item'], 'type': 'dynamicToolCall'}}],
                    [{**entry, 'item': {**entry['item'], 'processId': 'different-process'}}]]
        for entries in variants:
            self.pages[('old-turn', None)] = {'data': entries, 'nextCursor': None}
            self.assertTrue(self.run_check(30))
            self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')

    def test_exact_historical_command_exits_update_tasks_and_feed_without_replay(self):
        success = self.task('success')
        failure = self.task('failure')
        self.native(success, aggregatedOutput='The terminal command output')
        self.native(failure, status='failed', exitCode=128)
        self.update(turnId='new-turn', inFlight=True, status='running',
            activeTools=[{'id': 'new-item', 'type': 'commandExecution'}])
        before = copy.deepcopy(self.runtime.agent(self.agent['id']))
        calls = list(self.server.calls)
        with self.runtime.read_db() as db:
            before_events = list(db.execute('SELECT * FROM runtime_events'))
            before_budget = list(db.execute('SELECT * FROM runtime_budget_usage'))
            self.assertIn('background_tasks', {r['kind'] for r in _blockers(self.runtime, db, before)})
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', success['id'])['status'], 'completed')
        self.assertEqual(self.saved('tasks', failure['id'])['status'], 'failed')
        self.assertEqual(self.saved('tasks', failure['id'])['exitCode'], 128)
        self.assertEqual(self.saved('items', success['id'])['toolStatus'], 'completed')
        self.assertEqual(self.runtime.agent(self.agent['id']), before)
        self.assertEqual(self.server.calls, calls)
        with self.runtime.read_db() as db:
            self.assertEqual([tuple(r) for r in db.execute('SELECT * FROM runtime_events')],
                             [tuple(r) for r in before_events])
            self.assertEqual([tuple(r) for r in db.execute('SELECT * FROM runtime_budget_usage')],
                             [tuple(r) for r in before_budget])
            self.assertNotIn('background_tasks', {r['kind'] for r in _blockers(self.runtime, db, before)})
            payload = db.execute("SELECT payload FROM sync_entities WHERE collection='task' AND id=?",
                                 (success['id'],)).fetchone()
            self.assertEqual(json.loads(payload[0])['value']['status'], 'completed')

    def test_studio_success_overrides_native_delivery_failure_without_native_read(self):
        task = self.task(kind='dynamicToolCall')
        self.native(task, status='failed', success=False, error='Response delivery failed')
        self.receipt(task)
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')
        self.assertIn('The saved Studio result', self.saved('tasks', task['id'])['tail'])
        self.assertEqual(self.native_calls, [])

    def test_completed_local_receipt_recovers_while_native_account_is_offline(self):
        task = self.task(kind='dynamicToolCall')
        self.receipt(task)
        self.runtime.offline_accounts.add('default')
        self.runtime.connection_ids.pop('default')
        self.runtime.servers.pop('default')
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')
        self.assertEqual(self.native_calls, [])

    def test_exact_local_terminal_failure_recovers_only_an_explicit_false_result(self):
        failed = self.task('failed', kind='dynamicToolCall')
        rejected = self.task('rejected', kind='dynamicToolCall')
        invalid = self.task('invalid', kind='dynamicToolCall')
        self.receipt(failed, stage='failed', outcome='not_applied', result={'success': False})
        self.receipt(rejected, outcome='not_applied', result={'success': False})
        self.receipt(invalid, stage='failed', outcome='not_applied', result={'success': True})
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', failed['id'])['status'], 'failed')
        self.assertEqual(self.saved('tasks', rejected['id'])['status'], 'failed')
        self.assertEqual(self.saved('tasks', invalid['id'])['status'], 'running')
        self.assertEqual(self.native_calls, [])

    def test_local_pending_unknown_missing_result_and_duplicate_receipts_stay_running(self):
        variants = [{'stage': 'running', 'finished': None}, {'outcome': 'unknown'},
                    {'result': {}}, {'result': {'success': 'true'}}]
        for number, changes in enumerate(variants):
            task = self.task(str(number), kind='dynamicToolCall')
            self.native(task, status='failed', success=False)
            self.receipt(task, **changes)
        duplicate = self.task('duplicate', kind='dynamicToolCall')
        self.native(duplicate, status='failed', success=False)
        self.receipt(duplicate)
        self.receipt(duplicate, id='second-receipt')
        self.assertTrue(self.run_check())
        for task in [self.saved('tasks', self.agent['id'] + ':' + str(i)) for i in range(4)]:
            self.assertEqual(task['status'], 'running')
        self.assertEqual(self.saved('tasks', duplicate['id'])['status'], 'running')
        self.assertEqual(self.native_calls, [])

    def test_unknown_duplicate_wrong_type_and_wrong_turn_native_proofs_do_not_settle(self):
        unknown = self.task('unknown')
        self.native(unknown, status='inProgress')
        absent = self.task('absent')
        duplicate = self.task('duplicate')
        self.native(duplicate)
        self.native(duplicate)
        wrong_type = self.task('wrong-type')
        self.native(wrong_type, type='dynamicToolCall')
        self.assertTrue(self.run_check())
        for task in (unknown, absent, duplicate, wrong_type):
            self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.pages[('old-turn', None)]['data'][0]['turnId'] = 'other-turn'
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', wrong_type['id'])['status'], 'running')

    def test_owner_task_and_connection_changes_abort_exact_terminal_proof(self):
        for field, value in [('epoch', 19), ('accountKey', 'other'), ('threadId', 'other-thread')]:
            task = self.task('scope-' + field)
            self.native(task)
            self.assertTrue(recovery.queue_task_recovery(self.runtime))
            old = self.agent[field]
            self.update(**{field: value})
            function, args = self.jobs.pop(0)
            function(*args)
            self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
            self.update(**{field: old})
            self.clock += 30
        changed = self.task('changed')
        self.native(changed)
        def change_task(_params):
            record = self.saved('tasks', changed['id'])
            record['tail'] = 'New output during the read'
            with self.runtime.db() as db:
                self.runtime.put(db, 'tasks', record)
            self.runtime.connection_ids['default'] = 'replacement-connection'
        self.read_hook = change_task
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', changed['id'])['status'], 'running')

    def test_new_local_receipt_defers_native_terminal_until_next_exact_receipt_check(self):
        task = self.task(kind='dynamicToolCall')
        self.native(task, status='failed', success=False)
        self.read_hook = lambda _params: self.receipt(task)
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.read_hook = None
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')

    def test_timeout_and_offline_native_commands_remain_running(self):
        task = self.task()
        self.pages[('old-turn', None)] = TimeoutError('Read timed out')
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.assertFalse(self.runtime._task_recovery.busy)
        self.runtime.offline_accounts.add('default')
        self.assertFalse(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')

    def test_task_output_and_local_receipt_changes_abort_without_owner_change(self):
        task = self.task()
        self.native(task)
        def output(_params):
            record = self.saved('tasks', task['id'])
            record['tail'] = 'The output changed during the native read'
            with self.runtime.db() as db:
                self.runtime.put(db, 'tasks', record)
        self.read_hook = output
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        tool = self.task('tool', kind='dynamicToolCall')
        self.receipt(tool)
        resolve = recovery.resolve_record
        def change_receipt(state_dir, record):
            self.assertFalse(self.runtime.lock._is_owned(), 'Payload reads must release the runtime lock')
            self.receipt(tool, result={'success': False})
            return resolve(state_dir, record)
        with patch.object(recovery, 'resolve_record', side_effect=change_receipt):
            self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', tool['id'])['status'], 'running')

    def test_existing_context_wait_preparation_and_workspace_transitions_keep_ownership(self):
        task = self.task()
        self.native(task)
        for changes in ({'contextRepairWait': {'error': 'Fixture wait', 'scope': 'local'},
                         'status': 'queued', 'startAttempt': {'id': 'owned-wait', 'submitted': False}},
                        {'contextRepairWait': {'error': 'Paused input wait', 'scope': 'local'},
                         'status': 'paused', 'startAttempt': {'id': 'paused-wait', 'submitted': False}},
                        {'contextRepairWait': {'error': 'Active turn wait', 'scope': 'native'},
                         'status': 'running', 'inFlight': True, 'turnId': 'current-turn'},
                        {'status': 'queued'}, {'status': 'starting'},
                        {'workspaceOperation': 'restore'}, {'accountTransferId': 'transfer'},
                        {'contextRepair': {'phase': 'submitted'}}):
            self.update(**changes)
            self.assertFalse(self.run_check(30))
            self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
            self.update(contextRepairWait=None, status='completed', workspaceOperation=None,
                        accountTransferId=None, contextRepair=None, startAttempt=None,
                        inFlight=False, turnId=None)

    def test_completed_owner_with_stale_context_wait_recovers_only_the_exact_old_task(self):
        task = self.task(kind='dynamicToolCall')
        self.receipt(task)
        wait = {'scope': 'local', 'error': 'Context repair waits for a confirmed input receipt',
                'source': {'threadId': 'fixture-thread', 'attemptId': 'earlier-attempt'},
                'events': ['old-confirmed-input'], 'nextCheckAt': 0}
        self.update(status='completed', inFlight=False, turnId=None, startAttempt=None,
                    autoWake=False, contextRepairWait=wait)
        agent = copy.deepcopy(self.runtime.agent(self.agent['id']))
        calls = list(self.server.calls)
        with self.runtime.read_db() as db:
            events = [tuple(row) for row in db.execute('SELECT * FROM runtime_events')]
            budget = [tuple(row) for row in db.execute('SELECT * FROM runtime_budget_usage')]
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')
        self.assertEqual(self.runtime.agent(self.agent['id']), agent)
        self.assertEqual(self.runtime.agent(self.agent['id'])['contextRepairWait'], wait)
        self.assertEqual(self.server.calls, calls)
        self.assertEqual(self.native_calls, [])
        with self.runtime.read_db() as db:
            self.assertEqual([tuple(row) for row in db.execute('SELECT * FROM runtime_events')], events)
            self.assertEqual([tuple(row) for row in db.execute('SELECT * FROM runtime_budget_usage')], budget)

    def test_maintenance_cadence_and_singleflight_do_not_repeat_queries_or_jobs(self):
        task = self.task()
        self.native(task)
        self.assertTrue(recovery.queue_task_recovery(self.runtime))
        with patch.object(self.runtime, 'read_db', side_effect=AssertionError('Repeated scan')):
            self.assertFalse(recovery.queue_task_recovery(self.runtime))
            self.clock += 30
            self.assertFalse(recovery.queue_task_recovery(self.runtime))
        function, args = self.jobs.pop(0)
        function(*args)
        self.assertFalse(self.run_check())
        with patch.object(self.runtime, 'read_db', side_effect=AssertionError('Repeated empty scan')):
            self.assertFalse(self.run_check())

    def test_running_index_batch_cursor_reaches_later_tasks_after_unconfirmed_oldest(self):
        tasks = [self.task(str(i).zfill(3)) for i in range(70)]
        for task in tasks:
            self.native(task, status='inProgress' if int(task['itemId']) < 32 else 'completed')
        with self.runtime.db() as db:
            db.executemany('INSERT INTO runtime_tasks VALUES (?,?)', (
                ('history-' + str(i), json.dumps({'id': 'history-' + str(i),
                 'status': 'completed', 'created': i, 'tail': 'untouched-history'})) for i in range(2000)))
        original = json.loads
        def decode(value, *args, **kwargs):
            if isinstance(value, str) and 'untouched-history' in value:
                raise AssertionError('Historical payload was decoded')
            return original(value, *args, **kwargs)
        with self.runtime.read_db() as db:
            plan = db.execute("EXPLAIN QUERY PLAN SELECT rowid,record FROM runtime_tasks "
                "WHERE json_extract(record,'$.status')='running' "
                "ORDER BY json_extract(record,'$.created'),rowid LIMIT 32").fetchall()
            self.assertTrue(any('runtime_task_status' in row[3] for row in plan))
            self.assertFalse(any('TEMP B-TREE' in row[3] for row in plan))
        with patch('json.loads', side_effect=decode):
            self.assertTrue(self.run_check())
            self.assertEqual(self.runtime._task_recovery.last['checked'], 32)
            self.assertEqual(self.saved('tasks', tasks[32]['id'])['status'], 'running')
            self.assertTrue(self.run_check(30))
            self.assertEqual(self.saved('tasks', tasks[32]['id'])['status'], 'completed')
            self.assertTrue(self.run_check(30))
            self.assertEqual(self.saved('tasks', tasks[-1]['id'])['status'], 'completed')
            self.assertEqual(self.saved('tasks', tasks[0]['id'])['status'], 'running')

    def test_slow_first_turn_does_not_starve_later_native_groups(self):
        slow = self.task('slow', turn='slow-turn')
        fast = self.task('fast', turn='fast-turn')
        self.native(slow)
        self.native(fast)
        def stall(params):
            if params['turnId'] == 'slow-turn':
                self.clock += recovery.READ_SECONDS
        self.read_hook = stall
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', fast['id'])['status'], 'running')
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', fast['id'])['status'], 'completed')
        self.assertEqual(self.saved('tasks', slow['id'])['status'], 'running')

    def test_native_pagination_requires_complete_unique_cursor_sequence(self):
        task = self.task()
        item = self.native(task)
        self.pages[('old-turn', None)] = {'data': [], 'nextCursor': 'second'}
        self.pages[('old-turn', 'second')] = {'data': [
            {'turnId': 'old-turn', 'item': item}], 'nextCursor': 'second'}
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'running')
        self.pages[('old-turn', 'second')]['nextCursor'] = None
        self.assertTrue(self.run_check(30))
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')

    def test_exact_task_after_more_than_one_thousand_native_items_recovers(self):
        task = self.task()
        item = self.native(task)
        self.pages[('old-turn', None)] = {'data': [
            {'turnId': 'old-turn', 'item': {'id': 'other-' + str(i), 'type': 'agentMessage'}}
            for i in range(1000)], 'nextCursor': 'last-page'}
        self.pages[('old-turn', 'last-page')] = {'data': [
            {'turnId': 'old-turn', 'item': item}], 'nextCursor': None}
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')
        self.assertEqual(len(self.native_calls), 2)

    def test_receipt_lookup_uses_alias_and_primary_keys_with_large_owner_history(self):
        task = self.task(kind='dynamicToolCall')
        receipt = self.receipt(task)
        with self.runtime.db() as db:
            db.executemany('INSERT INTO runtime_tool_requests VALUES (?,?)', (
                ('history-' + str(i), json.dumps({'id': 'history-' + str(i),
                 'agent': self.agent['id'], 'accountKey': 'default',
                 'threadId': 'fixture-thread', 'turnId': 'other-' + str(i),
                 'callId': 'history-' + str(i), 'stage': 'completed', 'updated': i,
                 'tail': 'Owner history must stay unread' * 50})) for i in range(2000)))
        with self.runtime.read_db() as db:
            # Prepare the schema before measuring receipt selection itself.
            db.execute('SELECT id FROM runtime_tool_requests WHERE id=?', (receipt['id'],)).fetchone()
            instructions = [0]
            def bounded_read():
                instructions[0] += 100
                return instructions[0] > 1000
            db.set_progress_handler(bounded_read, 100)
            try:
                observed = recovery._receipt_state(db, self.agent, task)
                self.assertEqual(len(observed.aliases), 1)
                self.assertEqual(observed.rows[0][0], receipt['id'])
                proof = recovery._terminal_receipt(observed, self.agent, task)
                self.assertEqual(proof['id'], receipt['id'])
                self.assertLessEqual(instructions[0], 1000)
                instructions[0] = 0
                with self.assertRaisesRegex(sqlite3.OperationalError, 'interrupted'):
                    db.execute("SELECT id,record FROM runtime_tool_requests "
                        "WHERE json_extract(record,'$.agent')=? "
                        "AND json_extract(record,'$.accountKey')=? "
                        "AND json_extract(record,'$.threadId')=? "
                        "AND json_extract(record,'$.turnId')=? "
                        "AND json_extract(record,'$.callId')=? LIMIT 2",
                        (self.agent['id'], 'default', 'fixture-thread', 'old-turn', task['itemId'])).fetchall()
            finally:
                db.set_progress_handler(None, 0)
        with self.runtime.db() as db:
            db.execute('DELETE FROM runtime_tool_request_aliases WHERE agent=? AND alias=?',
                       (self.agent['id'], task['itemId']))
        self.assertTrue(self.run_check())
        self.assertEqual(self.saved('tasks', task['id'])['status'], 'completed')
        self.assertEqual(self.native_calls, [])


if __name__ == '__main__':
    unittest.main()
