#!/usr/bin/env python3
"""Run the real dynamic dispatch against isolated databases and fake app-server IO."""

import importlib.util
import json
from pathlib import Path
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('spawn_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class SpawnRequestRecovery(unittest.TestCase):
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    agent_update = f.WorkspaceContract.agent_update

    def setUp(self):
        f.WorkspaceContract.setUp(self)
        self.actor = self.runtime.prepare(self.lead())
        self.actor = self.agent_update(self.actor, turnId='lead-turn-1', status='running', autoWake=True)

    def message(self, call='spawn-1', tool='orchestration_spawn', args=None):
        return {'id': call, 'method': 'item/tool/call', '_studioReceivedAt': time.time() - 0.25,
                'params': {'threadId': self.actor['threadId'], 'turnId': self.actor['turnId'],
                           'callId': call, 'tool': tool,
                           'arguments': args if args is not None else {
                               'request_id': 'batch-1', 'agents': [
                                   {'name': 'first', 'prompt': 'Inspect first', 'role': 'reviewer'},
                                   {'name': 'second', 'prompt': 'Inspect second', 'role': 'reviewer'}]}}}

    def response(self, call):
        f.eventually(lambda: any(r['id'] == call for r in self.runtime.server.responses), timeout=3)
        return next(r['result'] for r in self.runtime.server.responses if r['id'] == call)

    def value(self, result):
        return json.loads(result['contentItems'][0]['text'])

    def children(self):
        return [a for a in self.runtime.snapshot()['agents'] if a.get('parentId') == self.actor['id']]

    def request(self, action='get', request_id='batch-1'):
        return self.runtime.request_action(self.actor['id'], {'action': action, 'request_id': request_id})

    def test_stable_id_replay_across_new_call_and_turn_preserves_exact_batch(self):
        self.runtime.dynamic(self.message())
        original = self.response('spawn-1')
        self.assertTrue(original['success'], original)
        ids = [a['id'] for a in self.value(original)['agents']]
        self.actor = self.agent_update(self.actor, turnId='lead-turn-2')
        self.runtime.dynamic(self.message('spawn-2'))
        self.assertEqual(self.response('spawn-2'), original)
        self.assertEqual({a['id'] for a in self.children()}, set(ids))
        receipt = self.request()
        self.assertEqual(receipt['agentIds'], ids)
        self.assertEqual(receipt['outcome'], 'applied')
        self.assertGreaterEqual(receipt['queueDelayMs'], 200)
        changed = self.message('changed')
        changed['params']['arguments']['agents'][1]['prompt'] = 'Different work'
        self.runtime.dynamic(changed)
        self.assertFalse(self.response('changed')['success'])
        self.assertIn('different content', self.response('changed')['contentItems'][0]['text'])
        self.assertEqual(len(self.children()), 2)
        self.assertEqual(self.request()['result'], original)

    def test_batch_write_failure_rolls_back_children_initial_events_and_receipt(self):
        original_put = self.runtime.put
        inserted = set()
        def fail_second(db, table, record):
            if table == 'agents' and record.get('parentId') == self.actor['id']:
                inserted.add(record['id'])
                if len(inserted) == 2:
                    raise RuntimeError('Injected second child write failure')
            return original_put(db, table, record)
        with patch.object(self.runtime, 'put', side_effect=fail_second):
            self.runtime.dynamic(self.message())
        self.assertFalse(self.response('spawn-1')['success'])
        self.assertEqual(self.children(), [])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE id LIKE '%:initial'").fetchone()[0], 0)
        receipt = self.request()
        self.assertEqual(receipt['outcome'], 'not_applied')
        self.assertNotIn('agentIds', receipt)
        self.assertIn('Injected second child', receipt['result']['contentItems'][0]['text'])

    def test_cancel_during_catalog_wait_prevents_entire_batch(self):
        entered, release = threading.Event(), threading.Event()
        original_catalog = self.runtime.catalog
        def delayed_catalog(*args, **kwargs):
            entered.set()
            if not release.wait(5):
                raise RuntimeError('Fixture gate timeout')
            return original_catalog(*args, **kwargs)
        with patch.object(self.runtime, 'catalog', side_effect=delayed_catalog):
            worker = threading.Thread(target=self.runtime.dynamic, args=(self.message(),))
            worker.start()
            try:
                self.assertTrue(entered.wait(3))
                cancelled = self.request('cancel')
                self.assertEqual(cancelled['outcome'], 'pending')
                self.assertTrue(cancelled['cancelRequested'])
            finally:
                release.set()
                worker.join(5)
            self.assertFalse(worker.is_alive())
        self.assertEqual(self.children(), [])
        self.assertFalse(self.response('spawn-1')['success'])
        self.assertEqual(self.request()['outcome'], 'not_applied')

    def test_reply_write_failure_keeps_applied_receipt_and_recoverable_ids(self):
        with patch.object(self.runtime.server, 'write', side_effect=BrokenPipeError('Fixture response loss')):
            self.runtime.dynamic(self.message())
        self.assertFalse(self.runtime.server.responses)
        receipt = self.request()
        self.assertEqual(receipt['outcome'], 'applied')
        self.assertEqual(set(receipt['agentIds']), {a['id'] for a in self.children()})
        self.assertEqual(len(receipt['agentIds']), 2)
        self.assertIn('tool_response_delivery_failed', (self.state / 'runtime-errors.log').read_text())
        self.runtime.dynamic(self.message('recovery', 'orchestration_request', {'action': 'get', 'request_id': 'batch-1'}))
        recovered = self.value(self.response('recovery'))
        self.assertEqual(recovered['agentIds'], receipt['agentIds'])
        self.assertEqual(recovered['result'], receipt['result'])
        self.assertEqual(len(self.children()), 2)

    def test_status_and_spawn_progress_when_general_and_tool_pools_are_full(self):
        release = threading.Event()
        count_lock = threading.Lock()
        entered = 0
        expected = self.runtime.pool._max_workers + self.runtime.tool_pool._max_workers
        all_entered = threading.Event()
        def hold():
            nonlocal entered
            with count_lock:
                entered += 1
                if entered == expected:
                    all_entered.set()
            release.wait(8)
        futures = []
        try:
            for executor in [self.runtime.pool, self.runtime.tool_pool]:
                futures.extend(executor.submit(hold) for _ in range(executor._max_workers))
            self.assertTrue(all_entered.wait(4))
            self.runtime.request(self.message('spawn-1'))
            self.assertTrue(self.response('spawn-1')['success'])
            self.runtime.request(self.message('status-1', 'orchestration_request', {'action': 'get', 'request_id': 'batch-1'}))
            self.assertEqual(self.value(self.response('status-1'))['outcome'], 'applied')
            self.runtime.request(self.message('peers-1', 'orchestration_peers', {}))
            peers = self.value(self.response('peers-1'))
            self.assertTrue(self.response('peers-1')['success'])
            self.assertTrue({a['id'] for a in self.children()}.issubset({a['id'] for a in peers['peers']}))
            self.assertTrue(all(not future.done() for future in futures), 'Both saturated pools must still be blocked')
        finally:
            release.set()
            for future in futures:
                future.result(3)

    def test_spawned_children_run_and_completion_resumes_finished_parent(self):
        self.runtime.dynamic(self.message())
        ids = self.request()['agentIds']
        self.runtime.dispatch()
        f.eventually(lambda: all(self.runtime.agent(agent_id)['status'] == 'running' for agent_id in ids))
        parent = self.agent_update(self.actor, status='completed', turnId=None, inFlight=False, autoWake=True)
        child = self.runtime.agent(ids[0])
        self.runtime.server.complete(child['threadId'], child['turnId'], 'Child verified output')
        f.eventually(lambda: self.runtime.agent(child['id'])['status'] == 'completed')
        with self.runtime.db() as db:
            results = [dict(row) for row in db.execute("SELECT * FROM runtime_events WHERE agent=? AND kind='child_result'", (parent['id'],))]
        self.assertEqual(len(results), 1)
        self.assertIn('Child verified output', results[0]['text'])
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(parent['id'])['status'] == 'running')
        starts = [params for method, params in self.runtime.server.calls if method == 'turn/start' and params['threadId'] == parent['threadId']]
        self.assertTrue(any('Child verified output' in json.dumps(params) for params in starts))

    def test_old_thread_recovery_bypasses_both_pools_and_send_bypasses_slow_tools(self):
        self.runtime.dynamic(self.message())
        child_id = self.request()['agentIds'][0]
        release_tools, release_coordination = threading.Event(), threading.Event()
        tools_entered = threading.Barrier(self.runtime.tool_pool._max_workers + 1)
        coordination_entered = threading.Barrier(self.runtime.coordination_pool._max_workers + 1)
        def hold(entered, release):
            entered.wait(4)
            release.wait(10)
        tool_futures = [self.runtime.tool_pool.submit(hold, tools_entered, release_tools)
                        for _ in range(self.runtime.tool_pool._max_workers)]
        coordination_futures = [self.runtime.coordination_pool.submit(hold, coordination_entered, release_coordination)
                                for _ in range(self.runtime.coordination_pool._max_workers)]
        try:
            tools_entered.wait(4)
            coordination_entered.wait(4)
            envelope = json.dumps({'agent_id': 'workspace', 'text': json.dumps({
                'tool': 'orchestration_request', 'arguments': {'action': 'get', 'request_id': 'batch-1'}})})
            self.runtime.request(self.message('legacy-recovery', 'orchestration_send', envelope))
            self.assertEqual(self.value(self.response('legacy-recovery'))['outcome'], 'applied')
            self.assertTrue(all(not future.done() for future in tool_futures + coordination_futures))
            release_coordination.set()
            for future in coordination_futures:
                future.result(3)
            self.runtime.request(self.message('ordinary-send', 'orchestration_send', {
                'agent_id': child_id, 'text': 'Continue the bounded task'}))
            receipt = self.value(self.response('ordinary-send'))
            self.assertEqual(receipt['status'], 'queued')
            with self.runtime.db() as db:
                event = db.execute('SELECT agent,kind,text FROM runtime_events WHERE id=?', (receipt['id'],)).fetchone()
                self.assertEqual(tuple(event), (child_id, 'followup', 'Continue the bounded task'))
            self.runtime.request(self.message('complaint-shortcut', 'orchestration_send', {
                'agent_id': 'complaint', 'text': json.dumps({'action': 'read'})}))
            self.assertTrue(self.response('complaint-shortcut')['success'])
            self.assertTrue(all(not future.done() for future in tool_futures))
        finally:
            release_tools.set()
            release_coordination.set()
            for future in tool_futures + coordination_futures:
                future.result(3)

    def test_malformed_workspace_envelopes_fail_in_dynamic_without_intake_crash(self):
        for n, text in enumerate(['not-json', '[]', json.dumps({'tool': []}),
                                  json.dumps({'tool': 'orchestration_request', 'arguments': []})]):
            call_id = 'malformed-' + str(n)
            self.runtime.request(self.message(call_id, 'orchestration_send', {'agent_id': 'workspace', 'text': text}))
            self.assertFalse(self.response(call_id)['success'])
        self.assertEqual(self.children(), [])

    def test_connection_replacement_while_waiting_for_lock_cannot_reserve_or_spawn(self):
        original_generation = self.runtime.connection_ids['default']
        original_check = self.runtime.connection_current
        checked = threading.Event()
        failures = []
        def observed_check(account, generation):
            valid = original_check(account, generation)
            if generation == original_generation and valid:
                checked.set()
            return valid
        def delayed_request():
            try:
                self.runtime.request(self.message(), connection_id=original_generation)
            except RuntimeError as error:
                # A stale connection cannot receive even the rejected request's reply.
                failures.append(str(error))
        with patch.object(self.runtime, 'connection_current', side_effect=observed_check):
            with self.runtime.lock:
                worker = threading.Thread(target=delayed_request)
                worker.start()
                self.assertTrue(checked.wait(3), 'The request must pass its outside-lock check first')
                self.runtime.connection_ids['default'] = 'replacement-generation'
            worker.join(3)
            self.assertFalse(worker.is_alive())
        self.assertEqual(self.children(), [])
        self.assertFalse(self.runtime.server.responses)
        self.assertEqual(failures, [], 'Stale intake is discarded without a stale reply or handler error')
        with self.runtime.db() as db:
            for table in ['runtime_tool_requests', 'runtime_tool_request_aliases', 'runtime_tool_results']:
                self.assertEqual(db.execute('SELECT COUNT(*) FROM ' + table).fetchone()[0], 0, table)

    def test_connection_end_settles_a_previously_queued_call_without_execution(self):
        generation = self.runtime.connection_ids['default']
        message = self.message()
        self.runtime.reserve_tool_request(message, connection_id=generation)
        self.runtime.connection_ids['default'] = 'replacement-generation'
        self.runtime.dynamic(message, connection_id=generation)
        self.assertEqual(self.request()['outcome'], 'not_applied')
        self.assertEqual(self.children(), [])
        self.assertFalse(self.runtime.server.responses)

    def test_connection_change_after_catalog_cannot_commit_workers(self):
        generation = self.runtime.connection_ids['default']
        catalog = self.runtime.catalog()
        def replaced_catalog(*args):
            with self.runtime.lock:
                self.runtime.connection_ids['default'] = 'replacement-generation'
            return catalog
        with patch.object(self.runtime, 'catalog', side_effect=replaced_catalog):
            self.runtime.dynamic(self.message(), connection_id=generation)
        self.assertEqual(self.children(), [])
        self.assertEqual(self.request()['outcome'], 'not_applied')
        self.assertFalse(self.runtime.server.responses)


if __name__ == '__main__':
    unittest.main()
