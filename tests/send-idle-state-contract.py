#!/usr/bin/env python3
"""Idle sends do not wait for the periodic scheduler to repair a stale label."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'send_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class IdleSendContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='studio-idle-send-')
        self.runtime = f.Runtime(Path(self.directory.name), f.FakeServer)
        self.runtime.closed = True
        self.runtime.changed.set()
        self.runtime.scheduler.join(timeout=5)
        self.assertFalse(self.runtime.scheduler.is_alive())
        self.runtime.closed = False
        self.runtime._fast_delivery_enabled = False

    def tearDown(self):
        self.runtime.close()
        self.directory.cleanup()

    def lead(self, **state):
        agent = self.runtime.create({'name': 'Lead', 'cwd': self.directory.name, 'prompt': ''},
                                    draft=True, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            agent.update(**state)
            self.runtime.put(db, 'agents', agent)
        return agent

    def test_idle_busy_label_dispatches_without_global_repair(self):
        for label in ('running', 'starting'):
            with self.subTest(label=label):
                agent = self.lead(status=label, inFlight=False, turnId=None)
                self.runtime.send(agent['id'], 'New task', 'send-' + label, delivery='after_tool')
                self.assertEqual(self.runtime.agent(agent['id'])['status'], 'queued')
                with patch.object(self.runtime.delivery_executor(), 'submit') as start:
                    self.assertEqual(self.runtime.dispatch(agent['id']), 1)
                    self.assertEqual(start.call_count, 1)
                self.assertEqual(self.runtime.delivery_receipt('send-' + label)['status'], 'reserved')
                self.runtime.send(agent['id'], 'New task', 'send-' + label, delivery='after_tool')
                with patch.object(self.runtime.delivery_executor(), 'submit') as start:
                    self.assertEqual(self.runtime.dispatch(agent['id']), 0)
                    start.assert_not_called()

    def test_fast_send_does_not_wait_for_global_maintenance(self):
        agent = self.lead(status='running', inFlight=False, turnId=None)
        self.runtime._fast_delivery_enabled = True
        self.runtime.send(agent['id'], 'Immediate task', 'fast-idle', delivery='after_tool')
        f.eventually(lambda: self.runtime.delivery_receipt('fast-idle')['status'] == 'delivered',
                     timeout=3)
        calls = [params for method, params in self.runtime.server.calls if method == 'turn/start']
        self.assertEqual(len(calls), 1)
        self.runtime.send(agent['id'], 'Immediate task', 'fast-idle', delivery='after_tool')
        self.assertEqual(len([method for method, _ in self.runtime.server.calls if method == 'turn/start']), 1)

    def test_native_identity_and_pending_preparation_keep_their_guard(self):
        for state in (
            {'status': 'running', 'inFlight': False, 'turnId': 'unknown-turn'},
            {'status': 'starting', 'inFlight': False, 'turnId': None,
             'startAttempt': {'id': 'unknown-attempt', 'submitted': True}},
            {'status': 'approval', 'inFlight': False, 'turnId': None},
        ):
            with self.subTest(state=state):
                agent = self.lead(**state)
                receipt = self.runtime.send(agent['id'], 'New task', delivery='after_tool')
                self.assertEqual(self.runtime.agent(agent['id'])['status'], state['status'])
                with patch.object(self.runtime.delivery_executor(), 'submit') as start:
                    self.assertEqual(self.runtime.dispatch(agent['id']), 0)
                    start.assert_not_called()
                self.assertEqual(self.runtime.delivery_receipt(receipt['id'])['status'], 'pending')

    def test_historical_unknown_input_does_not_block_a_finished_attempt(self):
        agent = self.lead(status='queued', inFlight=False, turnId=None,
                          startAttempt={'id': 'finished', 'epoch': 0, 'submitted': True,
                                        'events': ['finished-input'], 'turnId': 'finished-turn'})
        with self.runtime.lock, self.runtime.db() as db:
            for event_id, status in [('historical-input', 'uncertain'), ('finished-input', 'delivered')]:
                db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                           (event_id, agent['id'], 'user', event_id, status, 1, 0, None, None))
        self.runtime._fast_delivery_enabled = True
        self.runtime.send(agent['id'], 'New task', 'fresh-input', delivery='after_tool')
        f.eventually(lambda: self.runtime.delivery_receipt('fresh-input')['status'] == 'delivered',
                     timeout=3)
        self.assertEqual(len([method for method, _ in self.runtime.server.calls
                              if method == 'turn/start']), 1)
        self.assertEqual(self.runtime.delivery_receipt('historical-input')['status'], 'uncertain')
        self.assertEqual(self.runtime.delivery_receipt('finished-input')['status'], 'delivered')
        self.assertEqual(self.runtime.delivery_receipt('fresh-input')['status'], 'delivered')

    def test_unresolved_current_attempt_still_blocks_a_new_dispatch(self):
        for status in ('reserved', 'dispatching', 'uncertain'):
            with self.subTest(status=status):
                event_id = 'current-' + status
                agent = self.lead(status='queued', inFlight=False, turnId=None,
                                  startAttempt={'id': 'unknown', 'epoch': 0, 'submitted': True,
                                                'events': [event_id]})
                with self.runtime.lock, self.runtime.db() as db:
                    db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                               (event_id, agent['id'], 'user', event_id, status, 1, 0, None, None))
                receipt = self.runtime.send(agent['id'], 'New task', delivery='after_tool')
                with patch.object(self.runtime.delivery_executor(), 'submit') as start:
                    self.assertEqual(self.runtime.dispatch(agent['id']), 0)
                    start.assert_not_called()
                self.assertEqual(self.runtime.delivery_receipt(event_id)['status'], status)
                self.assertEqual(self.runtime.delivery_receipt(receipt['id'])['status'], 'pending')


if __name__ == '__main__':
    unittest.main()
