#!/usr/bin/env python3
"""Task decisions bypass slow tools and expose committed evidence before reply."""
import importlib.util
import json
from pathlib import Path
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('task_recovery_fixture', Path(__file__).with_name('spawn-request-recovery-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class TaskCompletionRecovery(unittest.TestCase):
    setUp = f.SpawnRequestRecovery.setUp
    tearDown = f.SpawnRequestRecovery.tearDown
    lead = f.SpawnRequestRecovery.lead
    agent_update = f.SpawnRequestRecovery.agent_update
    message = f.SpawnRequestRecovery.message
    response = f.SpawnRequestRecovery.response
    value = f.SpawnRequestRecovery.value

    def review_task(self):
        task = self.runtime.work_action(self.actor['id'], {'action': 'create', 'title': 'Acceptance fixture'})
        return self.runtime.work_action(self.actor['id'], {'action': 'submit', 'task_id': task['id'],
            'result': 'Exact source inspected', 'checks': 'Fixture checks passed', 'revision': 'fixture-revision', 'files': []})

    def accept(self, task):
        return {'action': 'accept', 'task_id': task['id'], 'result': 'Lead exact-source audit passed'}

    def test_direct_and_old_thread_accept_complete_while_all_slow_tools_are_blocked(self):
        release = threading.Event()
        entered = threading.Barrier(self.runtime.tool_pool._max_workers + 1)
        def hold():
            entered.wait(4)
            release.wait(10)
        jobs = [self.runtime.tool_pool.submit(hold) for _ in range(self.runtime.tool_pool._max_workers)]
        try:
            entered.wait(4)
            for wrapped in (False, True):
                task = self.review_task()
                name, args = 'orchestration_task', self.accept(task)
                if wrapped:
                    name, args = 'orchestration_send', {'agent_id': 'workspace', 'text': json.dumps({'tool': name, 'arguments': args})}
                call = 'accept-' + str(wrapped)
                self.runtime.request(self.message(call, name, args))
                result = self.response(call)
                self.assertTrue(result['success'], result)
                self.assertEqual(self.value(result)['status'], 'accepted')
                self.assertEqual(len(self.value(result)['decisions']), 1)
                self.assertTrue(all(not job.done() for job in jobs))
        finally:
            release.set()
            for job in jobs:
                job.result(3)

    def test_committed_accept_is_recoverable_while_its_final_tool_result_is_pending(self):
        task = self.review_task()
        committed, release = threading.Event(), threading.Event()
        original = self.runtime.work_action
        def delayed(*args, **kwargs):
            result = original(*args, **kwargs)
            committed.set()
            if not release.wait(8):
                raise RuntimeError('Fixture result gate timeout')
            return result
        with patch.object(self.runtime, 'work_action', side_effect=delayed):
            try:
                self.runtime.request(self.message('accept-delayed', 'orchestration_task', self.accept(task)))
                self.assertTrue(committed.wait(3))
                for _ in range(2):
                    receipt = self.runtime.request_action(self.actor['id'], {'action': 'get', 'request_id': 'accept-delayed'})
                    self.assertEqual((receipt['stage'], receipt['outcome']), ('running', 'pending'))
                    self.assertTrue(receipt['operationApplied'])
                    self.assertEqual(receipt['operationResult']['status'], 'accepted')
                    self.assertEqual(len(receipt['operationResult']['decisions']), 1)
                    self.assertNotIn('result', receipt)
                self.assertFalse(any(r['id'] == 'accept-delayed' for r in self.runtime.server.responses))
            finally:
                release.set()
            final = self.response('accept-delayed')
        self.assertTrue(final['success'], final)
        receipt = self.runtime.request_action(self.actor['id'], {'action': 'get', 'request_id': 'accept-delayed'})
        self.assertEqual(receipt['outcome'], 'applied')
        self.assertEqual(receipt['result'], final)
        tasks = original(self.actor['id'], {'action': 'list'})['items']
        self.assertEqual(len(next(t for t in tasks if t['id'] == task['id'])['decisions']), 1)


if __name__ == '__main__':
    unittest.main()
