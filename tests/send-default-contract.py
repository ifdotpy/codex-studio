#!/usr/bin/env python3
"""Assignment delivery ignores legacy modes and preserves exact receipts."""
import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class SendDefault(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = f.Runtime(Path(self.temp.name), f.FakeServer)
        self.lead = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Fixture'})
        f.eventually(lambda: self.runtime.agent(self.lead['id'])['status'] == 'running')
        self.lead = self.runtime.agent(self.lead['id'])
        self.worker = self.runtime.create({'name': 'Worker', 'prompt': 'Fixture', 'role': 'reviewer'},
                                          self.lead['id'])
        f.eventually(lambda: self.runtime.agent(self.worker['id'])['status'] == 'running')
        self.worker = self.runtime.agent(self.worker['id'])

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def message(self, call='first', **args):
        return {'id': call, 'params': {'threadId': self.lead['threadId'],
            'turnId': self.lead['turnId'], 'callId': call, 'tool': 'orchestration_send',
            'arguments': {'agent_id': self.worker['id'], 'text': 'Fix the implementation',
                          'request_id': 'assignment', **args}}}

    def invoke(self, message):
        self.runtime.dynamic(message)
        return next(row['result'] for row in self.runtime.server.responses if row['id'] == message['id'])

    def test_default_reaches_active_worker_once(self):
        result = self.invoke(self.message())
        self.assertTrue(result['success'])
        key = self.runtime.tool_request_key(self.message())
        f.eventually(lambda: self.runtime.delivery_receipt(key)['status'] == 'delivered')
        self.assertEqual(self.invoke(self.message('retry')), result)
        starts = [p for method, p in self.runtime.server.calls if method == 'turn/start'
                  and p['clientUserMessageId'] == key]
        self.assertEqual(len(starts), 1)
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.runtime.server.calls))

    def test_all_legacy_modes_have_one_delivery_rule(self):
        for index, mode in enumerate(('queue', 'steer', 'after_tool')):
            self.runtime.send(self.worker['id'], mode, 'legacy-' + str(index), delivery=mode)
            f.eventually(lambda i=index: self.runtime.delivery_receipt('legacy-' + str(i))['status'] == 'delivered')
        starts = [p for method, p in self.runtime.server.calls if method == 'turn/start'
                  and p['clientUserMessageId'].startswith('legacy-')]
        self.assertEqual(len(starts), 3)

    def test_idle_default_resumes_paused_worker(self):
        self.runtime.stop(self.worker['id'])
        result = self.invoke(self.message())
        self.assertTrue(result['success'])
        f.eventually(lambda: self.runtime.agent(self.worker['id'])['autoWake'])
        f.eventually(lambda: self.runtime.agent(self.worker['id'])['status'] == 'running')


if __name__ == '__main__':
    unittest.main(verbosity=2)
