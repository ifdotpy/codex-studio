#!/usr/bin/env python3
"""Capacity continuation uses empty input and one durable claim per failed turn."""
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime, ResponseTimeout
from codex_native_errors import NativeRpcError
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CapacityContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, fixture.FakeServer)
        self.server = self.runtime.connect()
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Original task'})
        self.key = a['id']
        fixture.eventually(lambda: self.agent().get('turnId'))

    def tearDown(self):
        if self.server.start_gate:
            self.server.start_gate.set()
        self.runtime.close()
        self.temp.cleanup()

    def agent(self):
        return self.runtime.agent(self.key)

    def starts(self):
        return [p for method, p in self.server.calls if method == 'turn/start']

    def fail(self, info='serverOverloaded', turn_id=None):
        a = self.agent()
        turn_id = turn_id or a['turnId']
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': a['threadId'],
            'turn': {'id': turn_id, 'status': 'failed',
                     'error': {'message': 'Model is at capacity', 'codexErrorInfo': info}}}})
        return self.agent().get('capacityRetry')

    def retry(self, retry=None, action='retry'):
        return self.runtime.capacity_retry(self.key, (retry or self.agent()['capacityRetry'])['id'], action)

    def expire(self):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.agent()
            a['capacityRetry']['dueAt'] = time.time() - 1
            self.runtime.capacity_save(db, a, a['capacityRetry'])
            self.runtime.put(db, 'agents', a)
        self.runtime.capacity_tick()

    def mutate(self, **fields):
        with self.runtime.lock, self.runtime.db() as db:
            a = self.agent()
            a.update(fields)
            self.runtime.put(db, 'agents', a)

    def test_four_delays_then_manual_only_without_input_replay(self):
        for index, delay in enumerate((10, 30, 120, 300)):
            retry = self.fail()
            self.assertEqual(retry['attempt'], index + 1)
            self.assertAlmostEqual(retry['dueAt'] - time.time(), delay, delta=1)
            self.expire()
            fixture.eventually(lambda: self.agent().get('turnId'))
            self.assertEqual(self.starts()[-1]['input'], [])
            self.assertNotIn('clientUserMessageId', self.starts()[-1])
            self.assertEqual(self.agent()['capacityRetryCount'], index + 1)
        retry = self.fail()
        self.assertEqual(retry['status'], 'exhausted')
        self.assertIsNone(retry['dueAt'])
        self.runtime.capacity_tick()
        self.assertEqual(len(self.starts()), 5)
        self.retry(retry)
        fixture.eventually(lambda: self.agent().get('turnId'))
        self.assertEqual(len(self.starts()), 6)
        self.assertEqual(self.fail()['status'], 'exhausted')

    def test_cancel_and_stale_timer_do_not_claim_but_manual_retry_can(self):
        retry = self.fail()
        result = self.retry(retry, 'cancel')
        self.assertEqual(result['status'], 'cancelled')
        self.assertGreaterEqual(result['updatedAt'], retry['updatedAt'])
        self.runtime.capacity_retry(self.key, retry['id'], 'retry', _automatic=True)
        self.assertEqual(len(self.starts()), 1)
        self.assertTrue(self.agent()['autoWake'])
        self.retry(retry)
        fixture.eventually(lambda: self.agent().get('turnId'))
        self.assertEqual(self.retry(retry, 'cancel')['status'], 'starting')
        self.assertEqual(len(self.starts()), 2)

    def test_duplicate_http_actions_and_scheduler_race_claim_once(self):
        retry = self.fail()
        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(lambda _: self.retry(retry), range(16)))
        fixture.eventually(lambda: self.agent().get('turnId'))
        self.assertEqual(len(self.starts()), 2)
        self.assertTrue(all(r.get('claimedAt') for r in results))
        newer = self.fail()
        self.assertNotEqual(retry['id'], newer['id'])
        self.retry(retry)
        self.retry(retry, 'cancel')
        self.assertEqual(self.agent()['capacityRetry'], newer)
        self.assertEqual(len(self.starts()), 2)

    def test_pending_events_stay_held_until_success(self):
        self.fail()
        with self.runtime.lock, self.runtime.db() as db:
            a = self.agent()
            self.runtime.enqueue(db, a, 'followup', 'A later task', 'held-event')
        self.retry()
        fixture.eventually(lambda: self.agent().get('turnId'))
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='held-event'").fetchone()[0], 'pending')
        a = self.agent()
        self.server.complete(a['threadId'], a['turnId'])
        fixture.eventually(lambda: len(self.starts()) == 3)
        self.assertIn('A later task', self.starts()[-1]['input'][0]['text'])
        self.assertNotIn('capacityRetryCount', self.agent())

    def test_unknown_native_outcome_and_restart_never_replay(self):
        retry = self.fail()
        self.server.fail_start = True
        self.retry(retry)
        fixture.eventually(lambda: self.agent()['capacityRetry']['status'] == 'unknown')
        self.assertTrue(self.agent()['inFlight'])
        self.retry(retry)
        self.runtime.capacity_tick()
        self.assertEqual(len(self.starts()), 2)
        self.runtime.close()
        self.runtime = Runtime(self.root, fixture.FakeServer)
        self.server = self.runtime.connect()
        self.assertEqual(self.agent()['capacityRetry']['status'], 'unknown')
        self.retry(retry)
        self.runtime.capacity_tick()
        self.assertFalse(self.starts())

    def test_restart_cancels_unsent_timer_but_keeps_manual_action(self):
        retry = self.fail()
        self.runtime.close()
        self.runtime = Runtime(self.root, fixture.FakeServer)
        self.server = self.runtime.connect()
        self.assertEqual(self.agent()['capacityRetry']['status'], 'cancelled')
        self.server.seq = 100  # Native turn identities do not restart with the process.
        self.retry(retry)
        fixture.eventually(lambda: self.agent().get('turnId'))
        self.assertEqual(len(self.starts()), 1)
        self.assertEqual(self.starts()[0]['input'], [])

    def test_known_start_rejection_consumes_claim_without_increment(self):
        retry = self.fail()
        original = self.server.call
        def reject(method, params, timeout=60):
            if method == 'turn/start':
                raise NativeRpcError({'code': -32600, 'message': 'Rejected'})
            return original(method, params, timeout)
        self.server.call = reject
        self.retry(retry)
        fixture.eventually(lambda: self.agent()['capacityRetry']['status'] == 'failed')
        self.assertEqual(self.agent().get('capacityRetryCount', 0), 0)
        self.server.call = original
        self.retry(retry)
        self.assertEqual(len(self.starts()), 1)

    def test_error_notification_and_stale_terminal_do_not_schedule(self):
        a = self.agent()
        self.server.notify({'method': 'error', 'params': {'threadId': a['threadId'],
            'turnId': a['turnId'], 'error': {'message': 'capacity', 'codexErrorInfo': 'serverOverloaded'},
            'willRetry': True}})
        self.assertNotIn('capacityRetry', self.agent())
        self.fail(turn_id='stale-turn')
        self.assertNotIn('capacityRetry', self.agent())
        self.fail('unauthorized')
        self.assertNotIn('capacityRetry', self.agent())

    def test_budget_block_and_stale_state_cannot_start(self):
        retry = self.fail()
        self.mutate(tokenBudget=1, tokensUsed=1)
        with self.assertRaisesRegex(ValueError, 'budget'):
            self.retry(retry)
        self.expire()
        self.assertEqual(self.agent()['capacityRetry']['status'], 'cancelled')
        self.mutate(tokenBudget=None, epoch=self.agent()['epoch'] + 1)
        with self.assertRaisesRegex(ValueError, 'earlier'):
            self.retry(retry)
        self.assertEqual(len(self.starts()), 1)

    def test_native_block_and_pending_approval_cannot_start(self):
        retry = self.fail()
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'requests', {'id': 'approval', 'agent': self.key, 'status': 'pending'})
        with self.assertRaisesRegex(ValueError, 'pending request'):
            self.retry(retry)
        with self.runtime.db() as db:
            db.execute("DELETE FROM runtime_requests WHERE id='approval'")
        self.mutate(nativeThreadBlock={'threadId': self.agent()['threadId'], 'error': {}})
        with self.assertRaisesRegex(ValueError, 'precaution'):
            self.retry(retry)
        self.assertEqual(len(self.starts()), 1)

    def test_terminal_before_ack_without_started_notification_waits_for_exact_ack(self):
        retry = self.fail()
        original = self.server.call
        ack = threading.Event()
        def complete_before_ack(method, params, timeout=60):
            if method != 'turn/start':
                return original(method, params, timeout)
            self.server.calls.append((method, params))
            self.server.notify({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                'turn': {'id': 'retry-terminal', 'status': 'failed',
                         'error': {'message': 'capacity', 'codexErrorInfo': 'serverOverloaded'}}}})
            ack.wait(5)
            return {'turn': {'id': 'retry-terminal'}}
        self.server.call = complete_before_ack
        try:
            self.retry(retry)
            fixture.eventually(lambda: self.agent().get('lastCompletedTurn') == 'retry-terminal')
            self.assertEqual(self.agent()['capacityRetry']['id'], retry['id'])
            self.assertEqual(self.agent().get('capacityRetryCount', 0), 0)
            ack.set()
            fixture.eventually(lambda: self.agent()['capacityRetry']['id'] != retry['id'])
            self.assertEqual(self.agent()['capacityRetry']['attempt'], 2)
            self.assertEqual(self.agent()['capacityRetryCount'], 1)
            self.assertEqual(len(self.starts()), 2)
        finally:
            ack.set()

    def test_success_before_ack_without_started_notification_releases_pending_input(self):
        retry = self.fail()
        original = self.server.call
        ack = threading.Event()
        def complete_before_ack(method, params, timeout=60):
            if method != 'turn/start' or params.get('input'):
                return original(method, params, timeout)
            self.server.calls.append((method, params))
            self.server.notify({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                'turn': {'id': 'retry-success', 'status': 'completed'}}})
            ack.wait(5)
            return {'turn': {'id': 'retry-success'}}
        self.server.call = complete_before_ack
        try:
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.enqueue(db, self.agent(), 'followup', 'Later task', 'later-task')
            self.retry(retry)
            fixture.eventually(lambda: self.agent().get('lastCompletedTurn') == 'retry-success')
            self.runtime.dispatch()
            self.assertEqual(len(self.starts()), 2)
            ack.set()
            fixture.eventually(lambda: len(self.starts()) == 3)
            self.assertIn('Later task', self.starts()[-1]['input'][0]['text'])
            self.assertFalse(self.agent().get('nativeFailureHold'))
        finally:
            ack.set()

    def test_late_acknowledgement_binds_without_second_submission(self):
        retry = self.fail()
        self.server.start_gate = threading.Event()
        wait = self.server.wait
        self.server.wait = lambda future, timeout=60: (_ for _ in ()).throw(
            ResponseTimeout('Response timed out; outcome unknown'))
        self.retry(retry)
        fixture.eventually(lambda: self.agent()['capacityRetry']['status'] == 'unknown')
        self.retry(retry)
        self.server.wait = wait
        self.server.start_gate.set()
        fixture.eventually(lambda: bool(self.agent()['capacityRetry'].get('acceptedTurnId')))
        self.assertEqual(self.agent()['capacityRetryCount'], 1)
        self.assertEqual(len(self.starts()), 2)
        self.assertEqual(self.fail()['attempt'], 2)

    def test_restart_after_claim_before_submission_preserves_manual_retry(self):
        retry = self.fail()
        with self.runtime.lock, self.runtime.db() as db:
            a = self.agent()
            retry.update(status='starting', claimedAt=time.time(), dueAt=None)
            a.update(status='starting', inFlight=True, startAttempt={
                'id': 'capacity:' + retry['id'], 'capacityRetryId': retry['id'],
                'action': 'capacity', 'epoch': a['epoch'], 'events': [], 'submitted': False})
            self.runtime.capacity_save(db, a, retry)
            self.runtime.put(db, 'agents', a)
        self.runtime.close()
        self.runtime = Runtime(self.root, fixture.FakeServer)
        self.server = self.runtime.connect()
        self.server.seq = 100
        self.assertEqual(self.agent()['capacityRetry']['status'], 'cancelled')
        self.assertNotIn('claimedAt', self.agent()['capacityRetry'])
        self.retry(retry)
        fixture.eventually(lambda: self.agent().get('turnId'))
        self.assertEqual(len(self.starts()), 1)

    def test_old_updaters_reject_runtime_without_capacity_lifecycle(self):
        from codex_native_errors_update import apply as errors_update
        from codex_turn_recovery_update import apply as recovery_update
        class PreviousRuntime:
            pass
        runtime = PreviousRuntime()
        runtime.marker = 'unchanged'
        for update in (errors_update, recovery_update):
            with self.assertRaisesRegex(RuntimeError, 'requires current source'):
                update(runtime)
            self.assertEqual(vars(runtime), {'marker': 'unchanged'})

    def test_duplicate_internal_dispatch_does_not_submit_again(self):
        retry = self.fail()
        self.retry(retry)
        fixture.eventually(lambda: self.agent().get('turnId'))
        self.runtime.run_native_action(self.key, self.agent()['startAttempt'])
        self.assertEqual(len(self.starts()), 2)

    def test_missing_native_turn_identity_remains_unknown(self):
        retry = self.fail()
        original = self.server.call
        def malformed(method, params, timeout=60):
            return {} if method == 'turn/start' else original(method, params, timeout)
        self.server.call = malformed
        self.retry(retry)
        fixture.eventually(lambda: self.agent()['capacityRetry']['status'] == 'unknown')
        self.assertTrue(self.agent()['inFlight'])
        self.assertFalse(self.agent()['capacityRetry'].get('acceptedTurnId'))
        self.retry(retry)
        self.assertEqual(self.agent().get('capacityRetryCount', 0), 0)

    def test_guards_are_checked_again_before_native_submit(self):
        retry = self.fail()
        original = self.runtime.prepare
        def change_budget(a):
            self.mutate(tokenBudget=1, tokensUsed=1)
            return original(a)
        self.runtime.prepare = change_budget
        self.retry(retry)
        fixture.eventually(lambda: self.agent()['capacityRetry']['status'] == 'failed')
        self.assertIn('budget', self.agent()['capacityRetry']['reason'])
        self.assertEqual(len(self.starts()), 1)

    def test_account_thread_settings_workspace_and_concurrency_guards(self):
        retry = self.fail()
        original = self.agent()
        for field, value in [('accountKey', 'other'), ('threadId', 'other-thread'),
                             ('effort', 'ultra'), ('cwd', '/tmp'), ('deletedAt', time.time())]:
            with self.subTest(field=field):
                self.mutate(**{field: value})
                with self.assertRaisesRegex(ValueError, 'earlier'):
                    self.retry(retry)
                self.mutate(**{field: original.get(field)})
        self.mutate(workspaceOperation='checkpoint')
        with self.assertRaisesRegex(ValueError, 'workspace'):
            self.retry(retry)
        self.mutate(workspaceOperation=None, concurrency=1)
        child = self.runtime.create({'name': 'Child', 'prompt': 'Work'}, self.key, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            child.update(status='running', inFlight=True)
            self.runtime.put(db, 'agents', child)
        with self.assertRaisesRegex(ValueError, 'slot'):
            self.retry(retry)
        self.assertEqual(len(self.starts()), 1)

    def test_new_message_releases_unsent_retry_claim_before_enqueue(self):
        retry = self.fail()
        entered, release = threading.Event(), threading.Event()
        original = self.runtime.prepare
        def gated(a):
            if (a.get('startAttempt') or {}).get('action') == 'capacity':
                entered.set()
                release.wait(5)
            return original(a)
        self.runtime.prepare = gated
        try:
            self.retry(retry)
            self.assertTrue(entered.wait(2))
            self.runtime.send(self.key, 'Replacement task')
            fixture.eventually(lambda: bool(self.agent().get('turnId')))
            release.set()
            self.assertEqual(len(self.starts()), 2)
            self.assertIn('Replacement task', self.starts()[-1]['input'][0]['text'])
            self.assertNotIn('capacityRetry', self.agent())
            self.assertTrue(self.agent()['inFlight'])
        finally:
            release.set()

    def test_new_message_keeps_submitted_unknown_retry_reserved(self):
        retry = self.fail()
        self.server.fail_start = True
        self.retry(retry)
        fixture.eventually(lambda: self.agent()['capacityRetry']['status'] == 'unknown')
        self.runtime.send(self.key, 'Follow-up task')
        self.runtime.dispatch()
        self.assertEqual(len(self.starts()), 2)
        self.assertTrue(self.agent()['inFlight'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND status='pending'",
                                        (self.key,)).fetchone()[0], 1)

    def test_stop_and_new_user_instruction_replace_timer(self):
        retry = self.fail()
        self.runtime.stop(self.key)
        self.retry(retry)
        self.assertEqual(len(self.starts()), 1)
        self.assertNotIn('capacityRetry', self.agent())
        self.runtime.send(self.key, 'New task')
        fixture.eventually(lambda: self.agent().get('turnId'))
        self.assertEqual(self.fail()['attempt'], 1)


if __name__ == '__main__':
    unittest.main()
