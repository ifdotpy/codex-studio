#!/usr/bin/env python3
"""Pending browser repair must not stop saved input or command results."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'browser_recovery_fixture', Path(__file__).with_name('browser-recovery-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
recovery = fixture.recovery


class BrowserRecoveryPendingCommand(unittest.TestCase):
    set_agent = fixture.BrowserRecovery.set_agent
    event = fixture.BrowserRecovery.event
    tick = fixture.BrowserRecovery.tick
    wait_stage = fixture.BrowserRecovery.wait_stage
    idle = fixture.BrowserRecovery.idle

    def setUp(self):
        profile = tempfile.TemporaryDirectory(prefix='studio-browser-profile-')
        self.addCleanup(profile.cleanup)
        home = patch('codex_accounts.codex_home', return_value=Path(profile.name))
        home.start()
        self.addCleanup(home.stop)
        auth = patch('codex_accounts.auth_metadata', return_value={
            'accountId': 'fixture-account', '_credentialIdentity': 'chatgpt:fixture-account',
            'status': 'ready',
        })
        auth.start()
        self.addCleanup(auth.stop)
        fixture.BrowserRecovery.setUp(self)
        self.runtime.loaded.add(self.agent['id'])
        # The saved native thread already has the current tool catalog.
        from codex_native_tools import mark_current
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.agent['id'], db)
            mark_current(actor, self.runtime.tool_definitions(actor))
            self.runtime.put(db, 'agents', actor)

    def pending(self):
        self.event(text='No browser is available')
        return self.idle()

    def row(self, key):
        with self.runtime.db() as db:
            return dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (key,)).fetchone())

    def native_methods(self):
        return [method for method, _params in self.server.calls
                if method in {'thread/unsubscribe', 'thread/resume', 'turn/start'}]

    def queue_reasons(self):
        result = self.runtime.model_directory(self.agent['id'], 'orchestration_status', {})
        return next(row['reasons'] for row in result['capacity']['queued'] if row['id'] == self.agent['id'])

    def test_context_preflight_and_queue_reasons_allow_pending_without_native_receipt(self):
        from codex_context_repair import _local_idle
        self.pending()
        user = self.runtime.send(self.agent['id'], 'Original context input', 'context-user')
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.agent['id'], db)
            self.runtime.put(db, 'monitors', {
                'id': 'persistent-context-server', 'agent': actor['id'], 'status': 'running',
                'stallTimeoutSeconds': 0,
            })
            monitor = self.runtime.enqueue(db, actor, 'monitor_exit', 'Original context result', 'context-result')
            self.assertEqual(_local_idle(self.runtime, db, actor, None, allow_background_work=True), [])
        self.assertEqual(self.queue_reasons(), ['awaiting_dispatch'])
        self.assertEqual((self.row(user['id'])['status'], self.row(monitor)['status']), ('pending', 'pending'))
        self.assertEqual(self.native_methods(), [])

    def test_persistent_monitor_allows_exact_user_and_monitor_delivery_once(self):
        before = self.pending()
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {
                'id': 'persistent-server', 'agent': self.agent['id'], 'status': 'running',
                'stallTimeoutSeconds': 0,
            })
        user = self.runtime.send(self.agent['id'], 'Fixture user input', 'saved-user-input')
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.agent['id'], db)
            monitor = self.runtime.enqueue(db, actor, 'monitor_exit', 'Fixture command result', 'saved-monitor-result')
            self.assertTrue(recovery.busy(self.runtime, db, actor))
        self.tick()
        self.assertEqual(self.runtime.agent(self.agent['id'])['browserRecovery']['stage'], 'pending')
        self.assertEqual(self.native_methods(), [])
        self.assertEqual(self.runtime.dispatch_candidates(self.agent['id']), 2,
                         'Pending browser recovery blocks saved input behind a persistent monitor')
        fixture.f.eventually(lambda: all(self.row(key)['status'] == 'delivered'
                                        for key in (user['id'], monitor)))
        actor = self.runtime.agent(self.agent['id'])
        self.assertEqual((actor['epoch'], actor['threadId']), (before['epoch'], before['threadId']))
        self.assertEqual(actor['browserRecovery']['id'], before['browserRecovery']['id'])
        self.assertNotIn('nativeRequest', actor['browserRecovery'])
        self.assertTrue(actor['autoWake'])
        with self.runtime.db() as db:
            saved_monitor = json.loads(db.execute('SELECT record FROM runtime_monitors WHERE id=?',
                                                  ('persistent-server',)).fetchone()[0])
            self.assertEqual(saved_monitor['status'], 'running')
            self.assertEqual(saved_monitor['stallTimeoutSeconds'], 0)
        calls = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]['clientUserMessageId'], user['id'])
        text = '\n'.join(value.get('text', '') for value in calls[0]['input'])
        self.assertEqual(text.count('Fixture user input'), 1)
        self.assertEqual(text.count('Fixture command result'), 1)
        self.runtime.send(self.agent['id'], 'Fixture user input', user['id'])
        self.runtime.dispatch_candidates(self.agent['id'])
        self.assertEqual(self.native_methods(), ['turn/start'])

    def test_saved_input_takes_priority_when_native_repair_is_otherwise_idle(self):
        self.pending()
        user = self.runtime.send(self.agent['id'], 'Input before optional repair', 'idle-pending-user')
        self.tick()
        self.assertEqual(self.runtime.agent(self.agent['id'])['browserRecovery']['stage'], 'pending',
                         'Optional recovery takes the native thread before saved input')
        self.assertEqual(self.native_methods(), [])
        self.assertEqual(self.runtime.dispatch_candidates(self.agent['id']), 1)
        fixture.f.eventually(lambda: self.row(user['id'])['status'] == 'delivered')
        self.assertEqual(self.native_methods(), ['turn/start'])

    def test_reconnecting_stage_remains_fenced(self):
        actor = self.pending()
        user = self.runtime.send(self.agent['id'], 'Wait for exact recovery', 'reconnecting-user')
        operation = copy.deepcopy(actor['browserRecovery'])
        operation['stage'] = 'reconnecting'
        self.set_agent(browserRecovery=operation)
        self.runtime._browser_recovery_jobs = {operation['id']}
        self.addCleanup(self.runtime._browser_recovery_jobs.clear)
        self.tick()
        from codex_context_repair import _local_idle
        with self.runtime.lock, self.runtime.db() as db:
            with self.assertRaisesRegex(ValueError, 'existing native recovery receipt'):
                _local_idle(self.runtime, db, self.runtime.agent(self.agent['id'], db), None,
                            allow_background_work=True)
        self.assertIn('browser_recovery', self.queue_reasons())
        self.assertEqual(self.runtime.dispatch_candidates(self.agent['id']), 0)
        self.assertEqual(self.row(user['id'])['status'], 'pending')
        self.assertEqual(self.native_methods(), [])

    def test_pending_native_request_requires_receipt_review_and_never_replays(self):
        actor = self.pending()
        user = self.runtime.send(self.agent['id'], 'Saved input after unknown request', 'unknown-native-user')
        operation = copy.deepcopy(actor['browserRecovery'])
        operation['nativeRequest'] = {'method': 'thread/unsubscribe', 'id': 913, 'submittedAt': 1.0}
        self.set_agent(browserRecovery=operation)
        from codex_context_repair import _local_idle
        with self.runtime.lock, self.runtime.db() as db:
            with self.assertRaisesRegex(ValueError, 'existing native recovery receipt'):
                _local_idle(self.runtime, db, self.runtime.agent(self.agent['id'], db), None,
                            allow_background_work=True)
        self.assertIn('browser_recovery', self.queue_reasons())
        self.assertEqual(self.runtime.dispatch_candidates(self.agent['id']), 0,
                         'A submitted browser request must fence native input')
        self.tick()
        actor = self.runtime.agent(self.agent['id'])
        self.assertTrue(actor.get('nativeFailureHold'), 'An unknown native receipt requires a hold')
        self.assertEqual(actor['browserRecovery']['stage'], 'failed')
        self.assertEqual(actor['browserRecovery']['nativeRequest'], operation['nativeRequest'])
        self.assertEqual(self.row(user['id'])['status'], 'pending')
        self.assertEqual(self.native_methods(), [])

    def test_missing_config_before_native_call_preserves_saved_input(self):
        actor = self.pending()
        operation = copy.deepcopy(actor['browserRecovery'])
        operation['stage'] = 'reconnecting'
        self.set_agent(browserRecovery=operation)
        user = {}

        def unavailable(_home, _base_home):
            # A real user send can arrive while the external config check runs.
            user.update(self.runtime.send(self.agent['id'], 'Input during config check', 'config-check-user'))
            return {}, 'Fixture integration unavailable'

        with patch('codex_browser.browser_status', side_effect=unavailable):
            recovery.reconnect(self.runtime, self.agent['id'], operation)
        actor = self.runtime.agent(self.agent['id'])
        self.assertEqual(actor['browserRecovery']['stage'], 'failed')
        self.assertIn('Fixture integration unavailable', actor['browserRecovery']['error'])
        self.assertNotIn('nativeRequest', actor['browserRecovery'])
        self.assertEqual(actor['status'], 'queued', 'Preflight failure marks runnable input as failed')
        self.assertFalse(actor.get('nativeFailureHold'), 'No native request was submitted')
        self.assertEqual(self.native_methods(), [])
        self.assertEqual(self.row(user['id'])['status'], 'pending')
        self.assertEqual(self.runtime.dispatch_candidates(self.agent['id']), 1)
        fixture.f.eventually(lambda: self.row(user['id'])['status'] == 'delivered')
        self.assertEqual(self.native_methods(), ['turn/start'])

    def test_actual_unknown_native_response_keeps_the_existing_hold(self):
        self.pending()
        self.server.failure = 'response timed out; outcome unknown'
        self.tick()
        actor = self.wait_stage('failed')
        self.assertTrue(actor['nativeFailureHold'])
        self.assertEqual(actor['browserRecovery']['nativeRequest']['method'], 'thread/resume')
        receipt = copy.deepcopy(actor['browserRecovery']['nativeRequest'])
        for _ in range(3):
            self.tick()
        self.assertEqual(self.runtime.agent(self.agent['id'])['browserRecovery']['nativeRequest'], receipt)
        self.assertEqual(self.native_methods(), ['thread/unsubscribe', 'thread/resume'])

    def test_exact_successful_discovery_verifies_pending_repair_without_reconnect(self):
        before = self.event(text='No browser is available')
        actor = self.event('# Selected Browser\n- Name: Chrome\n- Type: extension\n- ID: fixture',
                           status='completed', item_id='read-only-browser-proof')
        self.assertEqual(actor['browserRecovery']['stage'], 'verified')
        self.assertEqual(actor['browserRecovery']['id'], before['browserRecovery']['id'])
        self.assertEqual(actor['browserRecovery']['verificationItem'], 'read-only-browser-proof')
        self.assertEqual(self.native_methods(), [])

    def test_successful_discovery_cannot_clear_a_submitted_pending_request(self):
        before = self.event(text='No browser is available')
        operation = copy.deepcopy(before['browserRecovery'])
        receipt = {'method': 'thread/unsubscribe', 'id': 417, 'submittedAt': 1.0}
        operation['nativeRequest'] = receipt
        self.set_agent(browserRecovery=operation)
        actor = self.event('# Selected Browser\n- Name: Chrome\n- Type: extension\n- ID: fixture',
                           status='completed', item_id='discovery-during-unknown-request')
        self.assertEqual(actor['browserRecovery']['stage'], 'pending')
        self.assertEqual(actor['browserRecovery']['nativeRequest'], receipt)
        self.assertEqual(self.native_methods(), [])

    def test_wrong_connection_or_quoted_discovery_cannot_verify_pending_repair(self):
        self.event(text='No browser is available')
        actor = self.event('Quoted page: # Selected Browser\n- Name: Chrome\n- Type: extension\n',
                           status='completed', item_id='quoted-browser')
        self.assertEqual(actor['browserRecovery']['stage'], 'pending')
        actor = self.runtime.agent(self.agent['id'])
        item = {'type': 'mcpToolCall', 'server': 'node_repl', 'tool': 'js', 'status': 'completed',
                'id': 'foreign-connection', 'result': {'content': [{'type': 'text',
                'text': '# Selected Browser\n- Name: Chrome\n- Type: extension\n- ID: fixture'}]}}
        self.runtime.notification({'method': 'item/completed', 'params': {
            'threadId': 'native-thread', 'turnId': 'turn-one', 'item': item,
        }}, 'default', 'foreign-connection')
        actor = self.runtime.agent(self.agent['id'])
        self.assertEqual(actor['browserRecovery']['stage'], 'pending')
        self.assertEqual(self.native_methods(), [])


if __name__ == '__main__':
    unittest.main()
