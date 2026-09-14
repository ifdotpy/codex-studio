#!/usr/bin/env python3
"""Native actions use the exact repaired thread through the real repair helper."""
import concurrent.futures
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('context_fixture', Path(__file__).with_name('context-repair-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class NativeActionRepair(f.ContextRepair):
    def setUp(self):
        super().setUp()
        self.a = self.agent_update(self.a, autoWake=True)
        self.original_servers = dict(self.runtime.servers)
        self.runtime.servers['default'] = self.server
        self.runtime.connection_ids.setdefault('default', 'fixture-connection')
        original_call, original_submit = self.server.call, self.server.submit

        def call(method, params, timeout=60):
            if method == 'model/list':
                return f.f.fixture.FakeServer.call(self.server, method, params, timeout)
            if method == 'thread/resume':
                self.server.calls.append((method, copy.deepcopy(params)))
                return {'thread': {'id': params['threadId']}, 'model': params.get('model'),
                        'sandbox': {'type': 'readOnly'}, 'approvalPolicy': 'on-request'}
            if method in {'thread/settings/update', 'thread/compact/start', 'review/start'}:
                self.server.calls.append((method, copy.deepcopy(params)))
                return {'turn': {'id': 'review-turn', 'status': 'inProgress'}} if method == 'review/start' else {}
            return original_call(method, params, timeout)

        def submit(method, params):
            if method == 'thread/fork':
                return original_submit(method, params)
            future = concurrent.futures.Future()
            try:
                future.set_result(call(method, params))
            except Exception as error:
                future.set_exception(error)
            self.server.futures.append(future)
            return len(self.server.futures), method, future

        self.server.call, self.server.submit = call, submit
        self.server.wait = lambda ticket, timeout=60: ticket[2].result(timeout)

    def tearDown(self):
        self.runtime.servers.clear()
        self.runtime.servers.update(self.original_servers)
        super().tearDown()

    def action_flow(self, action):
        source = self.path.read_bytes()
        method = 'review/start' if action == 'review' else 'thread/compact/start'
        result = self.runtime.native_action(self.a['id'], action, 'repaired-action')
        self.assertEqual(result['outcome']['status'], 'acknowledged', result)
        current = self.runtime.agent(self.a['id'])
        repair = current['contextRepair']
        self.assertEqual(repair['phase'], 'completed')
        self.assertNotEqual(current['threadId'], self.tid)
        self.assertEqual(current['threadId'], repair['newThreadId'])
        self.assertEqual(result['receipt']['threadId'], self.tid)
        self.assertEqual(current['startAttempt']['actionIdentity']['threadId'], self.tid)
        self.assertEqual(current['startAttempt']['threadId'], current['threadId'])
        self.assertEqual(current['startAttempt']['id'], result['receipt']['attemptId'])
        self.assertEqual(repair['source']['attemptId'], result['receipt']['attemptId'])
        for expected in ('thread/resume', 'thread/settings/update', method):
            calls = [params for name, params in self.server.calls if name == expected]
            self.assertEqual(len(calls), 1, (expected, self.server.calls))
            self.assertEqual(calls[0]['threadId'], current['threadId'])
        self.assertEqual(len(self.forks()), 1)
        self.assertEqual(source, self.path.read_bytes())
        replay = self.runtime.native_action(self.a['id'], action, 'repaired-action')
        self.assertEqual(replay['receipt'], result['receipt'])
        self.assertEqual(replay['outcome'], result['outcome'])
        self.assertTrue(replay['replayed'])
        self.assertEqual(len([name for name, _ in self.server.calls if name == method]), 1)
        self.assertEqual(len(self.forks()), 1)

    def test_review_uses_repaired_thread_once(self):
        self.action_flow('review')

    def test_compact_uses_repaired_thread_once(self):
        self.action_flow('compact')


if __name__ == '__main__':
    suite = unittest.TestSuite(NativeActionRepair(name) for name in NativeActionRepair.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
