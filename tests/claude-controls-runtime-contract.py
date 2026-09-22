#!/usr/bin/env python3
"""Claude session controls against isolated SQLite and a fake native provider."""
import copy
import importlib.util
import json
from pathlib import Path
import queue
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_claude_controls import action, retire_idle_bridge


class Runtime(f.Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.05)
            self.changed.clear()


class Server(f.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.state = {'settings': {}, 'turns': [{'id': 't1', 'status': 'completed'}, {'id': 't2', 'status': 'completed'}], 'tasks': []}
        self.fail = False
        self.queue = []
        self.receipts = {}
        self.forks = 0
        self.before_reply = None

    def call(self, method, params, timeout=60):
        if method == 'claude/state':
            return copy.deepcopy(self.state)
        if method == 'thread/queue/list':
            return {'items': self.queue}
        if method == 'claude/settings':
            self.state['settings'] = params['settings']
            return {'settings': params['settings']}
        if method == 'thread/rollback':
            if params['requestId'] not in self.receipts:
                self.forks += 1
                self.receipts[params['requestId']] = {'threadId': params['threadId'], 'nativeId': 'fork', 'removedTurns': 1}
                self.state['turns'] = self.state['turns'][:1]
            if self.before_reply:
                self.before_reply()
            if self.fail:
                raise RuntimeError('lost response')
            return self.receipts[params['requestId']]
        return super().call(method, params, timeout)


class Controls(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.rt = Runtime(Path(self.tmp.name), Server)
        agent = self.rt.new_lead({})
        self.key = agent['id']
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db)
            agent.update(cwd=self.tmp.name, provider='claude', threadId='logical', status='completed', autoWake=False, inFlight=False)
            self.rt.put(db, 'agents', agent)
            self.rt.item(db, self.key, 'old', 'assistant', 'keep', turnId='t1')
            self.rt.item(db, self.key, 'new', 'assistant', 'remove', turnId='t2')
        get = self.rt.accounts.get
        self.account_patch = patch.object(self.rt.accounts, 'get', side_effect=lambda key: {**get(key), 'provider': 'claude', 'status': 'ready'})
        self.account_patch.start()
        self.server = self.rt.connect()

    def tearDown(self):
        self.account_patch.stop()
        self.rt.close()
        self.tmp.cleanup()

    def call(self, kind, **body):
        return action(self.rt, {'id': self.key, 'action': kind, **body})

    def records(self):
        with self.rt.db() as db:
            return [json.loads(r[0]) for r in db.execute('SELECT record FROM runtime_items WHERE agent=? ORDER BY created', (self.key,))]

    def test_rollback_receipt_and_hidden_history(self):
        result = self.call('rollback', turn_id='t2', request_id='r')
        self.assertEqual(result['threadId'], 'logical')
        self.assertNotIn('afterRestore', self.records()[0])
        self.assertEqual(self.records()[1]['afterRestore'], 'claude-control:r')
        self.assertEqual(self.records()[1]['text'], 'remove')
        self.assertEqual(self.rt.agent(self.key)['status'], 'paused')
        self.assertEqual(self.call('rollback', turn_id='t2', request_id='r'), result)
        self.assertEqual(self.server.forks, 1)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.call('rollback', turn_id='t1', request_id='r')

    def test_lost_response_preserves_rows_and_recovers_same_request(self):
        self.server.fail = True
        with self.assertRaisesRegex(RuntimeError, 'lost response'):
            self.call('rollback', turn_id='t2', request_id='r')
        self.assertTrue(all('afterRestore' not in row for row in self.records()))
        self.assertEqual(self.call('state')['controlOperation']['requestId'], 'r')
        self.server.fail = False
        self.call('rollback', turn_id='t2', request_id='r')
        self.assertEqual(self.server.forks, 1)
        self.assertIn('afterRestore', self.records()[1])

    def test_queued_local_and_native_messages_block_rollback(self):
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db); agent['autoWake'] = True; self.rt.put(db, 'agents', agent)
            self.rt.enqueue(db, agent, 'user', 'queued', 'event')
        with self.assertRaisesRegex(ValueError, 'queued messages'):
            self.call('rollback', turn_id='t2', request_id='r')
        with self.rt.db() as db:
            db.execute("UPDATE runtime_events SET status='cancelled' WHERE id='event'")
        self.server.queue = [{'id': 'steer'}]
        with self.assertRaisesRegex(ValueError, 'queued Claude input'):
            self.call('rollback', turn_id='t2', request_id='r2')
        self.assertEqual(self.server.forks, 0)

    def test_new_message_during_native_call_stays_queued(self):
        def enqueue():
            with self.rt.lock, self.rt.db() as db:
                agent = self.rt.agent(self.key, db); agent['autoWake'] = True; self.rt.put(db, 'agents', agent)
                self.rt.enqueue(db, agent, 'user', 'new message', 'later')
        self.server.before_reply = enqueue
        self.call('rollback', turn_id='t2', request_id='r')
        with self.rt.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='later'").fetchone()[0], 'pending')

    def test_settings_validation_and_no_thread(self):
        with self.assertRaisesRegex(ValueError, 'integer'):
            self.call('settings', settings={'autoCompactWindow': True})
        self.call('settings', settings={'permissionMode': 'plan', 'thinking': False})
        self.assertEqual(self.rt.agent(self.key)['claudeOptions']['permissionMode'], 'plan')
        with self.rt.lock, self.rt.db() as db:
            agent = self.rt.agent(self.key, db); agent['threadId'] = None; self.rt.put(db, 'agents', agent)
        self.assertEqual(self.call('state')['turns'], [])
        self.call('settings', settings={'permissionMode': 'default'})

    def test_idle_bridge_upgrade_preserves_tasks_and_invalidates_before_close(self):
        self.server.initialize_result = {'capabilities': {'claudeVersion': 2}}
        self.server.provider_options = {}
        self.server.pending = {}
        self.server.callbacks = queue.Queue()
        self.server.state['tasks'] = [{'task_id': 'background'}]
        self.assertFalse(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {'customModels': []}}, self.server))
        self.server.state['tasks'] = []
        previous = self.rt.connection_ids['default']
        def close():
            self.assertNotEqual(self.rt.connection_ids['default'], previous)
            self.server.closed = True
        self.server.close = close
        self.assertTrue(retire_idle_bridge(self.rt, 'default', {'claudeOptions': {'customModels': []}}, self.server))
        self.assertNotIn('default', self.rt.servers)


if __name__ == '__main__':
    unittest.main()
