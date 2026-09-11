#!/usr/bin/env python3
"""Native discovery recovery, transaction boundaries, and mutation non-replay."""
import concurrent.futures
import importlib.util
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_browser_recovery as recovery


class Server(f.FakeServer):
    failure = None
    hold = None

    def call(self, method, params, timeout=60):
        if method == 'thread/unsubscribe':
            self.calls.append((method, params))
            return {'status': 'unsubscribed'}
        if method == 'thread/resume' and self.failure:
            self.calls.append((method, params))
            raise RuntimeError(self.failure)
        return super().call(method, params, timeout)

    def submit(self, method, params):
        future = concurrent.futures.Future()
        try:
            future.set_result(self.call(method, params))
        except Exception as error:
            future.set_exception(error)
        return len(self.calls), method, future

    def wait(self, submitted, timeout=60):
        if submitted[1] == 'thread/unsubscribe' and self.hold:
            self.hold.wait(5)
        return submitted[2].result(timeout)

    def after_events(self, callback):
        callback()


class Runtime(f.Runtime):
    def schedule(self):
        pass


class BrowserRecovery(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.runtime = Runtime(Path(self.tmp.name), Server)
        self.addCleanup(self.runtime.close)
        self.server = self.runtime.connect()
        self.agent = self.runtime.create({'name': 'Browser test', 'cwd': self.tmp.name, 'prompt': ''}, draft=True, defer=True)
        self.set_agent(threadId='native-thread', turnId='turn-one', inFlight=True, status='running', autoWake=True)
        # Browser availability is covered by browser-native-contract and the live test.
        self.available = patch.object(recovery, 'busy', wraps=recovery.busy)
        self.runtime.new_thread_params = lambda a: {'config': {'features.multi_agent': False}, 'developerInstructions': 'Original instructions', 'dynamicTools': [], 'model': a['model']}
        self.config = patch('codex_browser.browser_config', return_value={'mcp_servers.node_repl': {}})
        self.config.start()
        self.addCleanup(self.config.stop)

    def set_agent(self, **values):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(self.agent['id'], db)
            a.update(values)
            self.runtime.put(db, 'agents', a)
            self.agent = a
        return a

    def event(self, text='Browser is not available: chrome', status='failed', item_id='failed-call', **values):
        item = {'type': 'mcpToolCall', 'server': 'node_repl', 'tool': 'js', 'status': status,
                'id': item_id, 'result': {'content': [{'type': 'text', 'text': text}]}, **values}
        self.runtime.notification({'method': 'item/completed', 'params': {'threadId': 'native-thread',
            'turnId': 'turn-one', 'item': item}}, 'default', self.runtime.connection_ids['default'])
        return self.runtime.agent(self.agent['id'])

    def tick(self):
        with self.runtime.lock, self.runtime.db() as db:
            actors = self.runtime.records(db, 'agents')
            recovery.tick(self.runtime, db, actors)

    def wait_stage(self, stage):
        f.eventually(lambda: self.runtime.agent(self.agent['id']).get('browserRecovery', {}).get('stage') == stage)
        return self.runtime.agent(self.agent['id'])

    def idle(self):
        return self.set_agent(inFlight=False, turnId=None, status='completed', activeTools=[])

    def test_detects_real_failure_but_defers_until_idle(self):
        a = self.event()
        self.assertEqual(a['browserRecovery']['stage'], 'pending')
        self.tick()
        self.assertFalse(any(m == 'thread/unsubscribe' for m, _ in self.server.calls))
        self.idle()
        self.tick()
        a = self.wait_stage('verify')
        self.assertEqual(a['threadId'], 'native-thread')
        native = [(m, p) for m, p in self.server.calls if m.startswith('thread/')]
        self.assertEqual([m for m, _ in native], ['thread/unsubscribe', 'thread/resume'])
        self.assertEqual(native[1][1]['threadId'], 'native-thread')
        self.assertNotIn('history', native[1][1])
        self.assertNotIn('dynamicTools', native[1][1])
        self.assertIn('Browser connection generation:', native[1][1]['developerInstructions'])
        with self.runtime.db() as db:
            events = db.execute("SELECT kind,text FROM runtime_events WHERE agent=?", (a['id'],)).fetchall()
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]['kind'], 'browser_recovery')
        self.assertIn('Do not replay prior clicks', events[0]['text'])
        self.assertFalse(any(m in {'turn/start', 'command/exec'} for m, _ in self.server.calls))

    def test_duplicate_failure_does_not_create_multiple_recoveries(self):
        first = self.event()['browserRecovery']['id']
        self.assertEqual(self.event()['browserRecovery']['id'], first)
        self.idle()
        for _ in range(4): self.tick()
        self.wait_stage('verify')
        self.tick()
        self.assertEqual(sum(m == 'thread/resume' for m, _ in self.server.calls), 1)

    def test_timeout_permission_and_page_text_do_not_trigger(self):
        for n, text in enumerate(['Timed out', 'access not granted', 'admin-enforced policy could not be verified',
                                  'Website says Browser is not available: chrome']):
            self.assertNotIn('browserRecovery', self.event(text, item_id=str(n)))
        self.assertNotIn('browserRecovery', self.event(status='completed'))
        self.assertNotIn('browserRecovery', self.event(server='other'))

    def test_stale_turn_does_not_trigger(self):
        self.set_agent(turnId='new-turn')
        self.assertNotIn('browserRecovery', self.event())

    def test_live_monitor_and_command_defer_recovery(self):
        for table in ['monitors', 'tasks']:
            self.event();self.idle()
            with self.runtime.db() as db:
                self.runtime.put(db, table, {'id': 'live', 'agent': self.agent['id'], 'status': 'running'})
            self.tick()
            self.assertEqual(self.runtime.agent(self.agent['id'])['browserRecovery']['stage'], 'pending')
            with self.runtime.db() as db:
                db.execute('DELETE FROM runtime_' + table + ' WHERE id=?', ('live',))
        self.assertFalse(any(m == 'thread/unsubscribe' for m, _ in self.server.calls))

    def test_unknown_resume_is_held_not_replayed(self):
        self.event();self.idle()
        self.server.failure = 'response timed out; outcome unknown'
        self.tick();a = self.wait_stage('failed')
        self.assertTrue(a['nativeFailureHold'])
        for _ in range(3): self.tick()
        self.assertEqual(sum(m == 'thread/resume' for m, _ in self.server.calls), 1)
        self.assertIn('outcome unknown', a['error'])
        self.assertEqual(a['browserRecovery']['nativeRequest']['method'], 'thread/resume')
        self.assertIsInstance(a['browserRecovery']['nativeRequest']['id'], int)

    def test_disabled_integration_does_not_unsubscribe(self):
        self.event();self.idle()
        with patch('codex_browser.browser_config', return_value={}):
            self.tick();self.wait_stage('failed')
        self.assertFalse(any(m == 'thread/unsubscribe' for m, _ in self.server.calls))

    def test_stop_during_reconnect_cannot_resume_work(self):
        self.event();self.idle()
        gate = self.server.hold = threading.Event()
        self.tick()
        f.eventually(lambda: any(m == 'thread/unsubscribe' for m, _ in self.server.calls))
        self.set_agent(epoch=2, status='paused', autoWake=False)
        gate.set()
        f.eventually(lambda: not getattr(self.runtime, '_browser_recovery_jobs', set()))
        a = self.runtime.agent(self.agent['id'])
        self.assertFalse(a['autoWake'])
        self.assertEqual(a['status'], 'paused')
        self.assertFalse(any(m == 'thread/resume' for m, _ in self.server.calls))
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE kind='browser_recovery'").fetchone()[0], 0)

    def test_failed_probe_stops_the_automatic_loop(self):
        self.event();self.idle();self.tick();self.wait_stage('verify')
        self.set_agent(turnId='turn-one', status='running', inFlight=True)
        self.event(item_id='probe-failed')
        self.idle();self.tick()
        self.assertEqual(self.runtime.agent(self.agent['id'])['browserRecovery']['stage'], 'failed')
        self.assertEqual(sum(m == 'thread/resume' for m, _ in self.server.calls), 1)

    def test_native_discovery_success_verifies_recovery(self):
        self.event();self.idle();self.tick();self.wait_stage('verify')
        self.set_agent(turnId='turn-one', status='running', inFlight=True)
        a = self.event('# Selected Browser\n- Name: Chrome\n- Type: extension\n- ID: fixture', status='completed', item_id='probe-ok')
        self.assertEqual(a['browserRecovery']['stage'], 'verified')

    def test_persisted_incomplete_recovery_is_not_replayed(self):
        self.event();self.idle()
        a = self.runtime.agent(self.agent['id'])
        a['browserRecovery']['stage'] = 'reconnecting'
        self.set_agent(browserRecovery=a['browserRecovery'])
        self.tick()
        a = self.wait_stage('failed')
        self.assertTrue(a['nativeFailureHold'])
        self.assertFalse(any(m == 'thread/resume' for m, _ in self.server.calls))

    def test_changed_epoch_does_not_block_a_new_failure_episode(self):
        a = self.event()
        previous = a['browserRecovery'].copy()
        previous['stage'] = 'failed'
        self.set_agent(epoch=2, browserRecovery=previous)
        a = self.event(item_id='new-epoch-call')
        self.assertEqual(a['browserRecovery']['stage'], 'pending')
        self.assertEqual(a['browserRecovery']['epoch'], 2)
        self.assertNotEqual(a['browserRecovery']['id'], previous['id'])

    def test_delayed_close_callbacks_drain_before_marking_loaded(self):
        self.event();self.idle()
        def after_events(callback):
            self.runtime.loaded.discard(self.agent['id'])
            callback()
        self.server.after_events = after_events
        self.tick();self.wait_stage('verify')
        self.assertIn(self.agent['id'], self.runtime.loaded)


    def test_recovery_does_not_use_coordination_executor(self):
        self.event();self.idle()
        with patch.object(self.runtime.recovery_pool, 'submit', side_effect=AssertionError('must stay available')):
            self.tick();self.wait_stage('verify')

    def test_concurrent_recovery_limit_defers_without_losing_request(self):
        self.event();self.idle()
        self.runtime._browser_recovery_jobs = {'other-one', 'other-two'}
        self.tick()
        self.assertEqual(self.runtime.agent(self.agent['id'])['browserRecovery']['stage'], 'pending')
        self.assertFalse(any(m == 'thread/unsubscribe' for m, _ in self.server.calls))
        self.runtime._browser_recovery_jobs.clear()



if __name__ == '__main__':
    unittest.main()
