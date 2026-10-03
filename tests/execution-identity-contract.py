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

    def test_child_and_background_completion_cannot_end_root(self):
        a = self.lead()
        run_id = self.records(a)[0]['id']
        for turn in ('child-turn', a['turnId']):
            self.event(a, 'turn/completed', {'id': turn, 'status': 'completed'}, parentTurnId=a['turnId'])
            self.assertEqual(self.runtime.agent(a['id'])['turnId'], a['turnId'])
            self.assertEqual(self.records(a)[0]['status'], 'running')
        self.event(a, 'turn/started', {'id': 'background-turn', 'status': 'inProgress'}, background=True)
        self.runtime.server.complete(a['threadId'], a['turnId'], 'Root result')
        before = self.runtime.agent(a['id'])
        self.event(a, 'turn/completed', {'id': 'background-turn', 'status': 'failed'}, background=True)
        self.event(a, 'turn/completed', {'id': 'unowned-late-turn', 'status': 'failed'})
        after = self.runtime.agent(a['id'])
        self.assertEqual((after['status'], after['lastCompletedTurn']), (before['status'], a['turnId']))
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
