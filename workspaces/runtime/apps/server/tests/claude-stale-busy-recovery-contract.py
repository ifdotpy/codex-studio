#!/usr/bin/env python3
"""Retry a fresh Claude turn despite an earlier busy Studio reservation."""
from codex_layout import REPOSITORY_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import unittest


ROOT = REPOSITORY_ROOT


def fixture(name, filename):
    spec = importlib.util.spec_from_file_location(name, SERVER_TESTS_ROOT / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


runtime_fixture = fixture('claude_busy_runtime_fixture', 'claude-pre-input-recovery-contract.py')
bridge_fixture = fixture('claude_busy_bridge_fixture', 'claude-pre-admission-receipt-contract.py')


class StaleBusyRecovery(runtime_fixture.ClaudePreInputRecovery):
    def stale_busy_start(self):
        self.agent = self.agent_update(self.agent, inFlight=True, status='running',
                                      turnId='previous-native-turn', turnEpoch=self.agent['epoch'],
                                      autoWake=True)
        self.assertFalse(self.server.active_turns)
        self.start()
        self.assertTrue(self.attempt['activeAtReservation'])
        self.assertTrue(self.attempt['submitted'])
        self.assertNotEqual(self.turn_id, 'previous-native-turn')
        self.assertIn('claudeInputRequest', self.attempt)
        self.assertEqual(self.attempt['claudeInputRequest']['clientUserMessageId'], self.ids[0])

    def test_new_native_turn_retries_exact_active_reservation_once(self):
        self.stale_busy_start()
        original_attempt = copy.deepcopy(self.attempt)
        self.complete(self.terminal(clientUserMessageId=self.ids[0]))
        self.assert_pending_batch()
        actor = self.runtime.agent(self.agent['id'])
        self.assertEqual(actor['status'], 'queued')
        self.assertFalse(actor.get('nativeFailureHold'))
        self.assertTrue(self.receipt()['retry'])
        self.runtime.dispatch()
        runtime_fixture.f.eventually(lambda: all(
            self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        starts = [params for method, params in self.server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 2)
        self.assertEqual(starts[0]['input'], starts[1]['input'])
        self.assertEqual(starts[0]['clientUserMessageId'], starts[1]['clientUserMessageId'])
        actor = self.runtime.agent(self.agent['id'])
        second_attempt = copy.deepcopy(actor['startAttempt'])
        self.complete(self.terminal(clientUserMessageId=self.ids[0]))
        self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt'], second_attempt)
        self.attempt, self.turn_id = second_attempt, actor['turnId']
        self.complete(self.terminal(clientUserMessageId=self.ids[0]))
        self.assert_pending_batch()
        self.assertTrue(self.runtime.agent(self.agent['id'])['nativeFailureHold'])
        self.assertFalse(self.receipt()['retry'])
        self.runtime.dispatch()
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.server.calls), 2)
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_execution_attempts WHERE id=?',
                                         (original_attempt['id'],)).fetchone()[0])
        self.assertEqual(saved['claudeInputRequest'], original_attempt['claudeInputRequest'])

    def test_active_reservation_requires_explicit_original_identity(self):
        self.stale_busy_start()
        self.complete()
        actor = self.runtime.agent(self.agent['id'])
        self.assertTrue(actor['nativeFailureHold'])
        self.assertNotIn('claudePreInputRetry', actor)
        self.assertEqual(self.server.calls, self.calls)
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')

    def test_historical_active_reservation_without_snapshot_is_held(self):
        self.stale_busy_start()
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.agent['id'], db)
            actor['startAttempt'].pop('claudeInputRequest')
            self.runtime.put(db, 'agents', actor)
            saved = json.loads(db.execute('SELECT record FROM runtime_execution_attempts WHERE id=?',
                                         (self.attempt['id'],)).fetchone()[0])
            saved.pop('claudeInputRequest', None)
            db.execute('UPDATE runtime_execution_attempts SET record=? WHERE id=?',
                       (json.dumps(saved), self.attempt['id']))
        self.complete(self.terminal(clientUserMessageId=self.ids[0]))
        actor = self.runtime.agent(self.agent['id'])
        self.assertTrue(actor['nativeFailureHold'])
        self.assertNotIn('claudePreInputRetry', actor)
        self.runtime.dispatch()
        self.assertEqual(self.server.calls, self.calls)

    def test_reservation_mode_must_match_the_durable_attempt(self):
        self.stale_busy_start()
        from codex_claude_input_recovery import recover_rejected_start
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(self.agent['id'], db)
            actor['startAttempt']['activeAtReservation'] = False
            self.assertIsNone(recover_rejected_start(self.runtime, db, actor, actor['startAttempt'],
                turn=self.terminal(clientUserMessageId=self.ids[0]),
                account_key='default', connection_id=self.connection))
        self.assertEqual(self.runtime.agent(self.agent['id'])['startAttempt'], self.attempt)
        self.assertEqual(self.server.calls, self.calls)

    def test_actual_steer_cannot_use_original_turn_rejection(self):
        self.start()
        original_id, original_turn = self.ids[0], self.turn_id
        self.ids = ['late-a', 'late-b']
        self.queue()
        self.runtime.dispatch()
        runtime_fixture.f.eventually(lambda: all(
            self.runtime.delivery_receipt(key)['status'] == 'delivered' for key in self.ids))
        actor = self.runtime.agent(self.agent['id'])
        self.attempt = copy.deepcopy(actor['startAttempt'])
        self.assertTrue(self.attempt['activeAtReservation'])
        self.assertEqual(actor['turnId'], original_turn)
        self.assertEqual(self.attempt['claudeInputRequest']['clientUserMessageId'], self.ids[0])
        calls = list(self.server.calls)
        self.complete(self.terminal(clientUserMessageId=original_id))
        actor = self.runtime.agent(self.agent['id'])
        self.assertTrue(actor['nativeFailureHold'])
        self.assertNotIn('claudePreInputRetry', actor)
        self.runtime.dispatch()
        self.assertEqual(self.server.calls, calls)
        for key in self.ids:
            self.assertEqual(self.runtime.delivery_receipt(key)['status'], 'delivered')


class NativeCompletionIdentity(bridge_fixture.PreAdmission):
    def test_preparation_rejection_names_original_input_not_queued_steer(self):
        (self.root / '.hang-account').touch()
        original = self.turn('Private first input', 'first-original')
        steer = self.turn('Private queued input', 'later-steer')
        self.assertTrue(steer['steered'])
        self.assertEqual(steer['turn']['id'], original['turn']['id'])
        completed = self.completed()
        self.assert_rejected(completed)
        self.assertIn('clientUserMessageId', completed)
        self.assertEqual(completed['clientUserMessageId'], 'first-original')
        saved = self.history()[0]
        self.assertEqual(saved['clientUserMessageId'], completed['clientUserMessageId'])
        self.assertIn('later-steer', [item['id'] for item in saved['items']])
        self.assertEqual(self.admissions(), [])

    def test_preparation_post_admission_error_has_no_rejection_identity(self):
        self.turn('post-admission-timeout', 'admitted-original')
        completed = self.completed()
        self.assertEqual(completed['status'], 'failed')
        self.assertNotIn('startOutcome', completed)
        self.assertNotIn('clientUserMessageId', completed)
        self.assertNotIn('data', completed['error'])
        self.assertEqual(len(self.admissions()), 1)


def load_tests(loader, tests, pattern):
    return unittest.TestSuite(cls(name) for cls in (StaleBusyRecovery, NativeCompletionIdentity)
                              for name in cls.__dict__ if name.startswith('test_'))


if __name__ == '__main__':
    unittest.main()
