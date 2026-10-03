#!/usr/bin/env python3
"""Execution records use real Runtime callers and temporary state only."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('execution_fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_execution import chain, ensure_tables, message_identity


class ExecutionIdentityContract(unittest.TestCase):
    setUp = fixture.RuntimeContract.setUp
    tearDown = fixture.RuntimeContract.tearDown
    lead = fixture.RuntimeContract.lead

    def records(self, agent):
        with self.runtime.db() as db:
            before = db.total_changes
            result = chain(db, agent=agent['id'])['runs']
            self.assertEqual(db.total_changes, before)
            return result

    def event(self, actor, method, turn, **fields):
        self.runtime.notification({'method': method, 'params': {
            'threadId': actor['threadId'], 'turn': turn, **fields}})

    def test_normal_turn_and_exact_message_identity(self):
        a = self.lead()
        fixture.eventually(lambda: self.records(a)[0]['attempts'][0]['submission'] == 'accepted')
        run = self.records(a)[0]
        self.assertEqual(run['turnId'], a['turnId'])
        self.assertEqual(run['accountKey'], a['accountKey'])
        self.assertEqual(run['epoch'], a['epoch'])
        self.assertEqual(len(run['inputEventIds']), 1)
        operation = 'turn:' + a['id'] + ':' + run['inputEventIds'][0]
        self.assertEqual(run['attempts'][0]['nativeOperationId'], operation)
        self.assertIn(operation, run['requestIds'])
        self.runtime.server.complete(a['threadId'], a['turnId'], 'Exact result')
        finished = self.records(a)[0]
        self.assertEqual((finished['status'], finished['result']), ('completed', 'Exact result'))
        self.assertEqual(finished['attempts'][0]['turnStatus'], 'completed')
        self.assertEqual(finished['attempts'][0]['resultRunId'], finished['id'])
        identity = message_identity(self.runtime, a['id'], a['threadId'], a['turnId'])
        from codex_message_info import message_info
        metadata = message_info(self.runtime, a['id'], a['turnId'] + '-answer', a['turnId'], a['threadId'])
        self.assertEqual(metadata['runId'], run['id'])
        self.assertEqual(metadata['attemptId'], run['attempts'][0]['id'])
        self.assertEqual(identity['runId'], run['id'])
        self.assertEqual(identity['attemptId'], run['attempts'][0]['id'])
        self.assertEqual(message_identity(self.runtime, a['id'], 'other-thread', a['turnId']), {})

    def test_completion_before_start_response_keeps_terminal_run(self):
        self.runtime.connect().finish_before_reply = True
        a = self.runtime.create({'name': 'Early completion', 'cwd': str(self.root), 'prompt': 'Finish'})
        fixture.eventually(lambda: self.runtime.agent(a['id'])['status'] == 'completed')
        fixture.eventually(lambda: self.records(a)[0]['attempts'][0]['submission'] == 'accepted')
        run = self.records(a)[0]
        self.assertEqual(run['status'], 'completed')
        self.assertEqual(run['result'], 'Result with evidence')

    def test_child_and_background_completion_cannot_end_root(self):
        a = self.lead()
        run_id = self.records(a)[0]['id']
        self.runtime.notification({'method': 'item/completed', 'params': {
            'threadId': a['threadId'], 'turnId': a['turnId'],
            'item': {'id': a['turnId'] + '-answer', 'type': 'agentMessage', 'text': 'Root result'}}})
        for turn in ('child-turn', a['turnId']):
            self.event(a, 'turn/completed', {'id': turn, 'status': 'completed'}, parentTurnId=a['turnId'])
            self.assertEqual(self.records(a)[0]['status'], 'running')
        self.event(a, 'turn/started', {'id': 'background-turn', 'status': 'inProgress'}, background=True)
        self.event(a, 'turn/completed', {'id': a['turnId'], 'status': 'completed'})
        self.event(a, 'turn/completed', {'id': 'background-turn', 'status': 'failed'}, background=True)
        self.event(a, 'turn/completed', {'id': 'unowned-late-turn', 'status': 'failed'})
        # The observer does not change the pre-existing agent state machine.
        # Only the execution record requires exact non-child root evidence.
        run = self.records(a)[0]
        self.assertEqual((run['id'], run['status'], run['result']), (run_id, 'completed', 'Root result'))
        self.assertTrue(any(node['kind'] == 'background' and node['status'] == 'failed' for node in run['nodes']))

    def test_late_background_after_next_turn_keeps_original_run(self):
        a = self.lead()
        first = self.records(a)[0]['id']
        self.event(a, 'turn/started', {'id': 'bg', 'status': 'inProgress'}, background=True)
        self.runtime.server.complete(a['threadId'], a['turnId'])
        self.runtime.send(a['id'], 'Next input', 'next-input')
        fixture.eventually(lambda: self.runtime.agent(a['id']).get('turnId') not in (None, a['turnId']))
        current = self.runtime.agent(a['id'])
        self.event(current, 'turn/completed', {'id': 'bg', 'status': 'completed'}, background=True)
        self.assertEqual(self.runtime.agent(a['id'])['turnId'], current['turnId'])
        runs = self.records(a)
        old = next(run for run in runs if run['id'] == first)
        self.assertEqual(old['nodes'][0]['status'], 'completed')
        self.assertFalse(runs[0]['nodes'])
        self.assertEqual(runs[0]['status'], 'running')
        self.event(current, 'turn/completed', {'id': 'late-child', 'status': 'completed'}, parentTurnId=a['turnId'])
        old = next(run for run in self.records(a) if run['id'] == first)
        self.assertTrue(any(node['turnId'] == 'late-child' for node in old['nodes']))
        self.assertFalse(self.records(a)[0]['nodes'])

    def test_child_thread_uses_parent_link_and_never_changes_root(self):
        a = self.lead()
        self.runtime.notification({'method': 'turn/completed', 'params': {
            'threadId': 'provider-child-thread', 'parentThreadId': a['threadId'],
            'turn': {'id': 'provider-child-turn', 'status': 'completed'}}})
        run = self.records(a)[0]
        self.assertEqual(run['nodes'][0]['threadId'], 'provider-child-thread')
        self.assertEqual(self.runtime.agent(a['id'])['turnId'], a['turnId'])
        self.assertEqual(run['status'], 'running')

    def test_busy_input_adds_attempt_to_containing_run(self):
        a = self.lead()
        run_id = self.records(a)[0]['id']
        self.runtime.send(a['id'], 'Steer', 'busy-input', delivery='steer')
        fixture.eventually(lambda: self.runtime.delivery_receipt('busy-input')['status'] == 'delivered')
        runs = self.records(a)
        self.assertEqual(len(runs), 1)
        self.assertEqual(runs[0]['id'], run_id)
        self.assertEqual(len(runs[0]['attempts']), 2)
        self.assertIn('busy-input', runs[0]['inputEventIds'])

    def test_unknown_then_rejected_retry_preserves_input_and_attempt_ids(self):
        self.runtime.connect().fail_start = True
        a = self.runtime.create({'name': 'Retry', 'cwd': str(self.root), 'prompt': 'Input'})
        self.runtime.dispatch()
        fixture.eventually(lambda: bool(self.runtime.agent(a['id']).get('error')))
        failed = self.runtime.agent(a['id'])
        attempt = failed['startAttempt']
        first = self.records(a)[0]
        self.assertEqual(first['attempts'][0]['submission'], 'unknown')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', (attempt['events'][0],)).fetchone()[0], 'uncertain')
        self.runtime.start_error(a['id'], attempt['id'], RuntimeError('Definitive rejection'))
        self.runtime.server.fail_start = False
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            current.update(status='queued', autoWake=True, error=None)
            for event in attempt['events']:
                db.execute("UPDATE runtime_events SET status='pending',error=NULL WHERE id=?", (event,))
            self.runtime.put(db, 'agents', current)
        self.runtime.dispatch()
        fixture.eventually(lambda: any(attempt['submission'] == 'accepted' for attempt in self.records(a)[0]['attempts']))
        run = self.records(a)[0]
        self.assertEqual(run['id'], first['id'])
        self.assertEqual({v['submission'] for v in run['attempts']}, {'accepted', 'rejected'})
        self.assertEqual(len({v['id'] for v in run['attempts']}), 2)
        self.assertEqual(len({v['nativeOperationId'] for v in run['attempts']}), 1)
        self.assertEqual(run['inputEventIds'], first['inputEventIds'])
        accepted = next(attempt for attempt in run['attempts'] if attempt['submission'] == 'accepted')
        self.assertEqual(message_identity(self.runtime, a['id'], run['threadId'], run['turnId'])['attemptId'], accepted['id'])
        self.assertNotEqual(accepted['id'], first['attempts'][0]['id'])

    def test_unknown_input_receipt_becomes_accepted_without_new_identity(self):
        self.runtime.connect().fail_start = True
        a = self.runtime.create({'name': 'Unknown', 'cwd': str(self.root), 'prompt': 'Input'})
        fixture.eventually(lambda: bool(self.runtime.agent(a['id']).get('error')))
        current = self.runtime.agent(a['id'])
        first = self.records(a)[0]
        self.event(current, 'turn/started', {'id': 'confirmed-turn', 'status': 'inProgress'})
        self.assertEqual(self.records(a)[0]['attempts'][0]['submission'], 'observed')
        self.runtime.notification({'method': 'item/completed', 'params': {
            'threadId': current['threadId'], 'turnId': 'confirmed-turn',
            'item': {'id': 'native-input', 'type': 'userMessage', 'clientId': current['startAttempt']['events'][0]}}})
        run = self.records(a)[0]
        self.assertEqual((run['id'], run['attempts'][0]['id']), (first['id'], first['attempts'][0]['id']))
        self.assertEqual(run['attempts'][0]['submission'], 'accepted')
        self.assertEqual(self.runtime.delivery_receipt(current['startAttempt']['events'][0])['status'], 'delivered')

    def test_busy_start_that_returns_new_turn_preserves_both_runs(self):
        import time
        a = self.lead()
        first = self.records(a)[0]
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            attempt = {'id': 'busy-rollover-attempt', 'accountKey': a['accountKey'], 'connectionId': self.runtime.connection_ids[a['accountKey']],
                       'epoch': a['epoch'], 'threadId': a['threadId'], 'submitted': True,
                       'events': [], 'activeAtReservation': True, 'created': time.time()}
            current['startAttempt'] = attempt
            self.runtime.put(db, 'agents', current)
        self.runtime.start_accepted(a['id'], attempt, {'turn': {'id': 'new-native-turn'}})
        runs = self.records(a)
        self.assertEqual(len(runs), 2)
        old = next(run for run in runs if run['id'] == first['id'])
        new = next(run for run in runs if run['turnId'] == 'new-native-turn')
        self.assertEqual(old['turnId'], a['turnId'])
        self.assertNotIn(attempt['id'], old['attemptIds'])
        self.assertEqual(new['attemptIds'], [attempt['id']])
        self.assertEqual(message_identity(self.runtime, a['id'], a['threadId'], new['turnId'])['attemptId'], attempt['id'])
        self.event(a, 'turn/completed', {'id': a['turnId'], 'status': 'completed'})
        self.assertEqual(self.runtime.agent(a['id'])['turnId'], 'new-native-turn')
        self.assertEqual(next(run for run in self.records(a) if run['id'] == first['id'])['status'], 'completed')

    def test_restart_retains_unknown_attempt_without_attaching_it_to_previous_turn(self):
        a = self.lead()
        self.runtime.server.complete(a['threadId'], a['turnId'])
        old = self.records(a)[0]
        self.runtime.server.fail_start = True
        self.runtime.send(a['id'], 'Next input', 'unknown-next')
        fixture.eventually(lambda: 'outcome unknown' in str(self.runtime.agent(a['id']).get('error')))
        unknown = self.records(a)[0]
        self.assertNotEqual(unknown['id'], old['id'])
        self.runtime.close()
        self.runtime = fixture.Runtime(self.root, fixture.FakeServer)
        runs = self.records(a)
        pending = next(run for run in runs if run['id'] == unknown['id'])
        previous = next(run for run in runs if run['id'] == old['id'])
        self.assertEqual(pending['status'], 'unknown')
        self.assertEqual(pending['attempts'][0]['submission'], 'unknown')
        self.assertEqual(pending['restartRecovery']['stage'], 'held')
        self.assertNotEqual(previous.get('restartRecovery', {}).get('stage'), 'held')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', ('unknown-next',)).fetchone()[0], 'uncertain')

    def test_restart_restores_unsent_input_with_new_attempt_and_same_run(self):
        from types import SimpleNamespace
        with patch.object(self.runtime, 'delivery_executor', return_value=SimpleNamespace(submit=lambda *_: None)):
            a = self.runtime.create({'name': 'Unsent', 'cwd': str(self.root), 'prompt': 'Input'})
            self.runtime.dispatch()
            fixture.eventually(lambda: bool(self.runtime.agent(a['id']).get('startAttempt')))
            first = self.records(a)[0]
            self.assertEqual(first['attempts'][0]['submission'], 'unsent')
            self.runtime.close()
        self.runtime = fixture.Runtime(self.root, fixture.FakeServer)
        fixture.eventually(lambda: any(attempt['submission'] == 'accepted' for run in self.records(a) for attempt in run['attempts']))
        run = self.records(a)[0]
        self.assertEqual(run['id'], first['id'])
        self.assertEqual({attempt['submission'] for attempt in run['attempts']}, {'unsent', 'accepted'})
        self.assertEqual(run['inputEventIds'], first['inputEventIds'])

    def test_restart_keeps_run_without_claiming_native_interruption(self):
        a = self.lead()
        first = self.records(a)[0]
        self.runtime.close()
        self.runtime = fixture.Runtime(self.root, fixture.FakeServer)
        restored = self.runtime.agent(a['id'])
        run = self.records(a)[0]
        self.assertEqual((run['id'], run['turnId'], run['epoch']), (first['id'], first['turnId'], first['epoch']))
        self.assertEqual(run['status'], 'running')
        self.assertEqual(restored['restartRecovery']['stage'], 'pending')
        self.event(restored, 'turn/completed', {'id': a['turnId'], 'status': 'completed'})
        self.assertEqual(self.records(a)[0]['status'], 'completed')

    def test_spawn_monitor_tool_receipt_and_task_submit_chain(self):
        a = self.lead(maxAgents=4)
        request = self.runtime.reserve_tool_request({'id': 81, 'params': {
            'threadId': a['threadId'], 'turnId': a['turnId'], 'callId': 'spawn-call',
            'tool': 'orchestration_spawn', 'arguments': {'request_id': 'spawn-request', 'agents': [{'name': 'Worker', 'prompt': 'Check', 'role': 'reviewer'}]}}})
        children = self.runtime.spawn_agents(a, {'agents': [{'name': 'Worker', 'prompt': 'Check', 'role': 'reviewer'}]}, request['id'])
        child = self.runtime.agent(children['agents'][0]['id'])
        fixture.eventually(lambda: self.runtime.agent(child['id']).get('turnId'))
        task = self.runtime.work_action(a['id'], {'action': 'create', 'title': 'Check', 'owner': child['id']})
        self.runtime.work_action(child['id'], {'action': 'submit', 'task_id': task['id'], 'result': 'Evidence', 'checks': 'Contract', 'revision': 'fixture'})
        monitor = self.runtime.monitor(a['id'], {'command': 'echo fixture'}, key=request['id'])
        run = self.records(a)[0]
        self.assertEqual({effect['kind'] for effect in run['effects']}, {'tool_requests', 'spawn', 'monitors', 'requests'})
        receipt = next(effect for effect in run['effects'] if effect['kind'] == 'tool_requests')
        self.assertEqual(receipt['requestId'], 'spawn-request')
        self.assertEqual(next(effect for effect in run['effects'] if effect['kind'] == 'monitors')['referenceId'], monitor['id'])
        child_run = self.records(child)[0]
        self.assertTrue(any(effect['kind'] == 'task_submit' and effect['taskId'] == task['id'] for effect in child_run['effects']))
        self.assertEqual(next(node for node in run['nodes'] if node['kind'] == 'managed_worker')['agentId'], child['id'])

    def test_record_failure_does_not_discard_native_completion_or_items_and_tasks(self):
        a = self.lead()
        with self.runtime.db() as db:
            self.runtime.item(db, a['id'], 'fault-item', 'assistant', 'Result', turnId=a['turnId'])
            self.runtime.put(db, 'tasks', {'id': 'fault-task', 'agent': a['id'], 'turnId': a['turnId'],
                                          'kind': 'tool', 'status': 'running', 'created': 1})
        with patch('codex_execution.observe_native', side_effect=RuntimeError('observer fault')), \
                patch('codex_execution.reconcile_effect', side_effect=RuntimeError('record fault')), \
                self.assertLogs('codex_execution', level='ERROR') as logs:
            self.event(a, 'turn/completed', {'id': a['turnId'], 'status': 'completed'})
        self.assertGreaterEqual(len(logs.output), 2)
        self.assertEqual(self.runtime.agent(a['id'])['lastCompletedTurn'], a['turnId'])
        with self.runtime.db() as db:
            item = json.loads(db.execute('SELECT record FROM runtime_items WHERE id=?', (a['id'] + ':fault-item',)).fetchone()[0])
            task = json.loads(db.execute("SELECT record FROM runtime_tasks WHERE id='fault-task'").fetchone()[0])
        self.assertEqual(item['turnStatus'], 'completed')
        self.assertEqual(task['status'], 'interrupted')

    def test_completion_after_cleared_turn_finishes_items_and_failure_notice(self):
        a = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            current.update(turnId=None, inFlight=False)
            self.runtime.put(db, 'agents', current)
            self.runtime.item(db, a['id'], 'cleared-item', 'assistant', 'Partial', turnId=a['turnId'])
        self.event(a, 'turn/completed', {'id': a['turnId'], 'status': 'failed', 'error': {'message': 'Cleared turn failed'}})
        with self.runtime.db() as db:
            rows = [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_items WHERE agent=?', (a['id'],))]
        self.assertEqual(next(item for item in rows if item['id'].endswith(':cleared-item'))['turnStatus'], 'failed')
        self.assertTrue(any('Cleared turn failed' in item.get('text', '') for item in rows))
        self.assertEqual(self.records(a)[0]['status'], 'failed')

    def test_native_self_started_turn_keeps_existing_agent_transition(self):
        a = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            current.pop('startAttempt', None)
            self.runtime.put(db, 'agents', current)
        self.event(a, 'turn/started', {'id': 'self-started-turn', 'status': 'inProgress'})
        self.assertEqual(self.runtime.agent(a['id'])['turnId'], 'self-started-turn')
        self.assertEqual(self.runtime.agent(a['id'])['status'], 'running')
        self.assertEqual({run['turnId'] for run in self.records(a)}, {a['turnId'], 'self-started-turn'})

    def test_old_completion_still_finishes_its_tasks_items_and_notice(self):
        a = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            current.pop('startAttempt', None)
            current.update(turnId='new-root', inFlight=True)
            self.runtime.put(db, 'agents', current)
            self.runtime.item(db, a['id'], 'old-item', 'assistant', 'Partial', turnId=a['turnId'])
            self.runtime.put(db, 'tasks', {'id': 'old-task', 'agent': a['id'], 'turnId': a['turnId'],
                                          'kind': 'tool', 'status': 'running', 'created': 1})
        self.event(a, 'turn/completed', {'id': a['turnId'], 'status': 'failed', 'error': {'message': 'Old turn failed'}})
        with self.runtime.db() as db:
            rows = [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_items WHERE agent=?', (a['id'],))]
            task = json.loads(db.execute("SELECT record FROM runtime_tasks WHERE id='old-task'").fetchone()[0])
        self.assertEqual(task['status'], 'interrupted')
        self.assertEqual(next(item for item in rows if item['id'].endswith(':old-item'))['turnStatus'], 'failed')
        self.assertTrue(any('Old turn failed' in item.get('text', '') for item in rows))
        self.assertEqual(self.runtime.agent(a['id'])['turnId'], 'new-root')
        self.assertEqual(next(run for run in self.records(a) if run['turnId'] == 'new-root')['status'], 'running')

    def test_hourly_server_maintenance_prunes_finished_runs_in_bounded_batches(self):
        import time
        from codex_canvas import Canvas, make_server
        from codex_execution import _save_run, prune, RETENTION_SECONDS, PRUNE_LIMIT
        a = self.lead()
        active_run = self.records(a)[0]
        active = active_run['id']
        input_id = active_run['inputEventIds'][0]
        now = time.time()
        with self.runtime.db() as db:
            for index in range(PRUNE_LIMIT + 1):
                run = {'id': 'expired-' + str(index), 'agent': a['id'], 'accountKey': a['accountKey'], 'epoch': a['epoch'],
                       'created': now - RETENTION_SECONDS - 1, 'status': 'completed'}
                _save_run(db, run)
                db.execute('INSERT INTO runtime_execution_inputs VALUES (?,?)', (run['id'], 'retained-event'))
                for table in ('attempts', 'effects', 'nodes'):
                    if table == 'effects':
                        db.execute('INSERT INTO runtime_execution_effects VALUES (?,?,?,?,?)', (run['id'], run['id'], 'tool_requests', 'retained-request', '{}'))
                    else:
                        db.execute('INSERT INTO runtime_execution_' + table + ' VALUES (?,?,?)', (run['id'], run['id'], '{}'))
            for status in ('running', 'unknown'):
                _save_run(db, {'id': 'old-' + status, 'agent': a['id'], 'accountKey': a['accountKey'],
                               'epoch': a['epoch'], 'created': now - RETENTION_SECONDS - 1, 'status': status})
            self.assertEqual(prune(db, now=now - 2), 0)
        canvas = Canvas(self.root)
        canvas.runtime = self.runtime
        server = make_server(canvas)
        try:
            server.service_actions()
            with self.runtime.db() as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_execution_runs WHERE id LIKE 'expired-%'").fetchone()[0], 1)
            server.service_actions()  # The same hourly window does not prune again.
            with self.runtime.db() as db:
                self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_execution_runs WHERE id LIKE 'expired-%'").fetchone()[0], 1)
                self.assertEqual(prune(db, now=now), 1)
                for table in ('attempts', 'inputs', 'effects', 'nodes'):
                    self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_execution_' + table + " WHERE run LIKE 'expired-%'").fetchone()[0], 0)
                self.assertIsNotNone(db.execute('SELECT 1 FROM runtime_execution_runs WHERE id=?', (active,)).fetchone())
                self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_execution_runs WHERE id IN ('old-running','old-unknown')").fetchone()[0], 2)
                self.assertIsNotNone(db.execute('SELECT 1 FROM runtime_events WHERE id=?', (input_id,)).fetchone())
        finally:
            server.server_close()

    def test_agent_and_execution_writes_rollback_together_and_deltas_do_not_write(self):
        a = self.lead()
        with self.runtime.lock, self.assertRaisesRegex(RuntimeError, 'abort'), self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            current['startAttempt']['id'] = 'rolled-back-attempt'
            self.runtime.put(db, 'agents', current)
            raise RuntimeError('abort')
        self.assertNotIn('rolled-back-attempt', [v['id'] for v in self.records(a)[0]['attempts']])
        statements = []
        with self.runtime.lock, self.runtime.db() as db:
            db.set_trace_callback(statements.append)
            current = self.runtime.agent(a['id'], db)
            current['tail'] = 'Text delta'
            self.runtime.put(db, 'agents', current)
        self.assertFalse(any('runtime_execution_' in sql for sql in statements))


if __name__ == '__main__':
    unittest.main(verbosity=2)
