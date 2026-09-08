#!/usr/bin/env python3
"""Native unload and rejection recovery without replaying user input."""
import importlib.util
from concurrent.futures import Future
from unittest.mock import patch
from pathlib import Path
import sys
import tempfile
import time
import unittest
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime, NativeRpcError, ResponseTimeout
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

class MissingThreadContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        self.server = self.runtime.connect()
        self.agent = self.runtime.create({'name': 'Worker', 'cwd': self.temp.name, 'prompt': 'Task'}, draft=True)
        self.agent = self.runtime.prepare(self.agent)
        self.key = self.agent['id']
        self.thread = self.agent['threadId']
        self.connection = self.runtime.connection_ids['default']

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def unload(self, method, account='default', connection=None, thread=None):
        self.runtime.notification({'method': method, 'params': {'threadId': thread or self.thread,
            'status': {'type': 'notLoaded'}}}, account, connection or self.connection)

    def test_unload_resumes_exact_thread_without_starting_turn(self):
        for method in ('thread/closed', 'thread/status/changed'):
            with self.subTest(method=method):
                before = self.runtime.agent(self.key)
                self.unload(method)
                self.assertNotIn(self.key, self.runtime.loaded)
                self.assertEqual(self.runtime.agent(self.key), before)
                result = self.runtime.prepare(before)
                self.assertEqual(result['threadId'], self.thread)
                self.assertIn(self.key, self.runtime.loaded)
        resumes = [p for m, p in self.server.calls if m == 'thread/resume']
        self.assertEqual(len(resumes), 2)
        self.assertTrue(all(p['threadId'] == self.thread for p in resumes))
        self.assertFalse(any(m == 'turn/start' for m, _ in self.server.calls))

    def test_unload_during_resume_rejects_late_preparation(self):
        self.runtime.loaded.discard(self.key)
        native = Future()
        with patch.object(self.server, 'submit', return_value=native):
            completion = self.runtime.prepare_locked(self.runtime.agent(self.key))
            self.unload('thread/closed')
            native.set_result({'thread': {'id': self.thread}})
            with self.assertRaisesRegex(ValueError, 'earlier agent state'):
                completion.result(timeout=3)
        self.assertNotIn(self.key, self.runtime.loaded)
        self.runtime.prepare(self.runtime.agent(self.key))
        self.assertIn(self.key, self.runtime.loaded)
        self.assertFalse(any(m == 'turn/start' for m, _ in self.server.calls))

    def test_unload_preserves_active_turn_and_unknown_delivery(self):
        self.rejection(ResponseTimeout('outcome unknown'), unknown=True)
        before = self.runtime.agent(self.key)
        self.unload('thread/closed')
        self.assertEqual(self.runtime.agent(self.key), before)
        self.assertEqual(self.runtime.delivery_receipt('input')['status'], 'uncertain')

    def test_wrong_account_connection_and_thread_cannot_invalidate(self):
        self.unload('thread/closed', account='other')
        self.unload('thread/closed', connection='old')
        self.unload('thread/closed', thread='other')
        self.assertIn(self.key, self.runtime.loaded)

    def rejection(self, error, unknown=False):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.key, db)
            a.update(inFlight=True, status='starting', autoWake=False, startAttempt={
                'id': 'attempt', 'epoch': a['epoch'], 'accountKey': 'default',
                'connectionId': self.connection, 'threadId': self.thread,
                'submitted': True, 'events': ['input']})
            self.runtime.put(db, 'agents', a)
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                ('input', self.key, 'user', 'Do not replay', 'uncertain', time.time(), a['epoch'], None, None))
        self.runtime.start_error(self.key, 'attempt', error, unknown=unknown)

    def test_exact_native_rejection_clears_cache_and_resolves_receipt(self):
        self.rejection(NativeRpcError({'code': -32600, 'message': 'thread not found: ' + self.thread}))
        self.assertNotIn(self.key, self.runtime.loaded)
        self.assertEqual(self.runtime.delivery_receipt('input')['status'], 'failed')
        self.runtime.prepare(self.runtime.agent(self.key))
        self.assertEqual(self.runtime.agent(self.key)['threadId'], self.thread)
        self.assertFalse(any(m == 'turn/start' for m, _ in self.server.calls))

    def test_timeout_does_not_reload_or_resolve_delivery(self):
        self.rejection(ResponseTimeout('turn/start response timed out; outcome unknown'), unknown=True)
        self.assertIn(self.key, self.runtime.loaded)
        self.assertEqual(self.runtime.delivery_receipt('input')['status'], 'uncertain')
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])

    def test_other_thread_rejection_does_not_invalidate(self):
        self.rejection(NativeRpcError({'code': -32600, 'message': 'thread not found: other'}))
        self.assertIn(self.key, self.runtime.loaded)

    def test_untyped_error_does_not_invalidate(self):
        self.rejection(RuntimeError('thread not found: ' + self.thread))
        self.assertIn(self.key, self.runtime.loaded)

if __name__ == '__main__':
    unittest.main()
