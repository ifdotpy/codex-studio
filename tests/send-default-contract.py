#!/usr/bin/env python3
"""Assignment defaults steer active workers and preserve explicit queues and receipts."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('steer_fixture', Path(__file__).with_name('critical-steer-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class SendDefault(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        with patch.object(f.Runtime, 'schedule', lambda self: None):
            self.runtime = f.Runtime(Path(self.temp.name), f.SteerServer)
        self.server = self.runtime.connect()
        self.lead = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Fixture'}, defer=True)
        self.change(self.lead, threadId='lead-thread', turnId='lead-turn', autoWake=True, status='running', inFlight=True)
        self.worker = self.runtime.create({'name': 'Worker', 'prompt': 'Fixture', 'role': 'reviewer'},
                                          self.lead['id'], defer=True)
        self.change(self.worker, threadId='worker-thread', turnId='worker-turn', autoWake=True, status='running', inFlight=True)

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def change(self, agent, **fields):
        with self.runtime.lock, self.runtime.db() as db:
            row = self.runtime.agent(agent['id'], db)
            row.update(fields)
            self.runtime.put(db, 'agents', row)

    def message(self, call='first', **args):
        return {'id': call, 'params': {'threadId': 'lead-thread', 'turnId': 'lead-turn',
            'callId': call, 'tool': 'orchestration_send', 'arguments': {
                'agent_id': self.worker['id'], 'text': 'Fix the active implementation',
                'request_id': 'assignment', **args}}}

    def invoke(self, message):
        self.runtime.dynamic(message)
        return next(row['result'] for row in self.server.responses if row['id'] == message['id'])

    def metadata(self, message):
        with self.runtime.db() as db:
            return json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?',
                (self.runtime.tool_request_key(message),)).fetchone()[0])

    def test_default_steers_active_worker_once_even_after_turn_finishes(self):
        message = self.message()
        result = self.invoke(message)
        self.assertTrue(result['success'], result)
        self.assertEqual(self.metadata(message)['delivery'], 'steer')
        self.assertEqual(self.metadata(message)['requestedDelivery'], 'after_tool')
        self.change(self.worker, turnId=None, inFlight=False, status='completed')
        self.assertEqual(self.invoke(self.message('retry')), result)
        steers = [params for method, params in self.server.calls if method == 'turn/steer']
        self.assertEqual(len(steers), 1)
        self.assertEqual(steers[0]['expectedTurnId'], 'worker-turn')

    def test_explicit_queue_waits_and_changed_delivery_cannot_reuse_identity(self):
        message = self.message(delivery='queue')
        result = self.invoke(message)
        self.assertTrue(result['success'], result)
        self.assertEqual(self.metadata(message)['delivery'], 'queue')
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.server.calls))
        changed = self.invoke(self.message('changed', delivery='steer'))
        self.assertFalse(changed['success'])

    def test_idle_default_starts_a_new_turn_and_resumes_a_paused_worker(self):
        self.change(self.worker, turnId=None, inFlight=False, status='paused', autoWake=False)
        message = self.message()
        result = self.invoke(message)
        self.assertTrue(result['success'], result)
        self.assertEqual(self.metadata(message)['delivery'], 'queue')
        self.assertTrue(self.runtime.agent(self.worker['id'])['autoWake'])
        self.runtime.dispatch()
        f.fixture.eventually(lambda: any(method == 'turn/start' for method, _ in self.server.calls))
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.server.calls))

    def test_pre_update_queue_receipt_is_not_changed_or_sent_again(self):
        message = self.message()
        key = self.runtime.tool_request_key(message)
        self.runtime.send(self.worker['id'], message['params']['arguments']['text'], key,
            manual=False, resume=True, delivery='queue', sender=self.lead['id'], sender_epoch=0)
        before = self.metadata(message)
        result = self.invoke(message)
        self.assertTrue(result['success'], result)
        self.assertEqual(self.metadata(message), before)
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.server.calls))

    def test_checkpoint_guard_still_defers_the_default(self):
        message = self.message()
        with patch.object(self.runtime, 'workspace_blockers', return_value=[{'operation': 'checkpoint'}]):
            result = self.invoke(message)
        self.assertTrue(result['success'], result)
        self.assertEqual(self.metadata(message)['delivery'], 'queue')
        self.assertEqual(self.metadata(message)['requestedDelivery'], 'after_tool')
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.server.calls))

    def test_old_event_without_metadata_keeps_its_queue_receipt(self):
        message = self.message()
        key = self.runtime.tool_request_key(message)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enqueue(db, self.runtime.agent(self.worker['id'], db), 'followup',
                                 message['params']['arguments']['text'], key)
        result = self.invoke(message)
        self.assertTrue(result['success'], result)
        self.assertFalse(any(method == 'turn/steer' for method, _ in self.server.calls))


if __name__ == '__main__':
    unittest.main()
