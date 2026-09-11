#!/usr/bin/env python3
"""Exercise native safety state and exact-once retry without model inference."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from concurrent.futures import Future
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime
from codex_native_errors import NativeRpcError
from codex_safety_buffering import fail
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

class Server(fixture.FakeServer):
    def __init__(self, *args):
        super().__init__(*args)
        self.turn_status = 'inProgress'
        self.items = [{'type': 'userMessage', 'content': [{'type': 'text', 'text': 'Original input'}]}]
        self.gated = None
        self.gate_future = None
        self.extra_cursor = False
        self.newer_turn = False
        self.failed_method = None

    def call(self, method, params, timeout=60):
        if method in {'thread/turns/list', 'thread/items/list', 'thread/fork'}:
            self.calls.append((method, params))
            if method == self.failed_method:
                raise NativeRpcError({'code': -32600, 'message': 'Rejected'})
        if method == 'thread/turns/list':
            return {'data': [{'id': 'newer' if self.newer_turn else self.source_turn,
                              'status': self.turn_status}]}
        if method == 'thread/items/list':
            return {'data': [{'turnId': self.source_turn, 'item': i} for i in self.items],
                    'nextCursor': 'more' if self.extra_cursor else None}
        if method == 'thread/fork':
            return {'thread': {'id': 'forked-thread'}}
        if method == 'turn/interrupt':
            self.turn_status = 'interrupted'
        return super().call(method, params, timeout)

    def submit(self, method, params):
        if method == self.gated:
            self.gate_future = Future()
            self.gate_params = (method, params)
            return self.gate_future
        return super().submit(method, params)

    def release(self):
        self.gate_future.set_result(self.call(*self.gate_params))

class Safety(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = Runtime(Path(self.temp.name), Server)
        self.server = self.runtime.connect()
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Work'})
        self.key = a['id']
        fixture.eventually(lambda: bool(self.runtime.agent(self.key).get('turnId')))
        self.a = self.runtime.agent(self.key)
        self.turn = self.a['turnId']
        self.server.source_turn = self.turn
        self.connection = self.runtime.connection_ids['default']

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def event(self, method, **params):
        self.runtime.notification({'method': method, 'params': {'threadId': self.a['threadId'],
            'turnId': self.turn, **params}}, 'default', self.connection)

    def buffering(self, **extra):
        self.event('model/safetyBuffering/updated', model='gpt-6-astra', showBufferingUi=True,
                   fasterModel='gpt-5.6-sol', useCases=['cyber'], reasons=['review'], **extra)

    def action(self, choice='retry'):
        return self.runtime.native_action(self.key, {'safety': choice, 'turnId': self.turn})

    def receipt(self):
        return self.runtime.agent(self.key).get('nativeSafetyRetry', {})

    def settled(self):
        fixture.eventually(lambda: self.receipt().get('stage') in {'running', 'failed', 'unknown'})
        return self.receipt()

    def test_buffering_wait_is_persistent_and_not_a_model_call(self):
        calls = list(self.server.calls)
        self.buffering()
        self.assertEqual(self.runtime.agent(self.key)['activity']['phase'], 'safety')
        self.action('wait')
        self.buffering()
        self.assertTrue(self.runtime.agent(self.key)['nativeSafetyBuffering']['dismissed'])
        self.assertEqual(self.server.calls, calls)

    def test_response_or_tool_progress_disables_retry_including_reordered_offer(self):
        for method, params in [('item/agentMessage/delta', {'itemId': 'text', 'delta': 'Answer'}),
                ('item/started', {'item': {'id': 'cmd', 'type': 'commandExecution'}})]:
            with self.subTest(method=method):
                self.event(method, **params)
                self.buffering()
                self.assertTrue(self.runtime.agent(self.key)['nativeSafetyBuffering']['responseStarted'])
                with self.assertRaisesRegex(ValueError, 'no longer active'): self.action()

    def test_wrong_turn_and_terminal_refusal_do_not_offer_retry(self):
        self.buffering(turnId='stale')
        self.assertNotIn('nativeSafetyBuffering', self.runtime.agent(self.key))
        self.buffering()
        self.event('error', error={'message': 'Refused', 'codexErrorInfo': 'cyberPolicy'}, willRetry=False)
        self.assertNotIn('nativeSafetyBuffering', self.runtime.agent(self.key))
        with self.assertRaises(ValueError): self.action()

    def test_retry_exact_native_fork_and_input_preserves_team_and_permissions(self):
        child = self.runtime.create({'name': 'Worker', 'prompt': 'Work'}, self.key, defer=True)
        self.buffering()
        self.action()
        self.assertEqual(self.settled()['stage'], 'running')
        a = self.runtime.agent(self.key)
        self.assertEqual(a['threadId'], 'forked-thread')
        self.assertEqual(a['effort'], 'low')
        self.assertEqual(a['model'], 'gpt-5.6-sol')
        self.assertEqual(self.runtime.agent(child['id'])['parentId'], self.key)
        forks = [p for m,p in self.server.calls if m == 'thread/fork']
        self.assertEqual(len(forks), 1)
        self.assertEqual(forks[0]['beforeTurnId'], self.turn)
        self.assertTrue(forks[0]['deferGoalContinuation'])
        starts = [p for m,p in self.server.calls if m == 'turn/start' and p['threadId'] == 'forked-thread']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['input'], self.server.items[0]['content'])
        self.assertEqual(starts[0]['approvalPolicy'], self.runtime.turn_permissions(self.a)['approvalPolicy'])
        self.action()
        self.assertEqual(len([p for m,p in self.server.calls if m == 'thread/fork']), 1)

    def test_tool_history_cannot_be_replayed(self):
        self.server.items.append({'type': 'commandExecution', 'id': 'done'})
        self.buffering(); self.action()
        self.assertEqual(self.settled()['stage'], 'failed')
        self.assertFalse(any(m in {'thread/fork', 'turn/interrupt'} for m,p in self.server.calls))

    def test_partial_history_cannot_be_replayed(self):
        self.server.extra_cursor = True
        self.buffering(); self.action()
        self.assertEqual(self.settled()['stage'], 'failed')
        self.assertFalse(any(m == 'thread/fork' for m,p in self.server.calls))

    def test_late_fork_response_is_reconciled_without_duplicate_mutation(self):
        self.server.gated = 'thread/fork'
        self.buffering(); self.action()
        fixture.eventually(lambda: self.server.gate_future is not None)
        with self.runtime.db() as db:
            op = json.loads(db.execute('SELECT record FROM runtime_safety_retries').fetchone()[0])
        fail(self.runtime, op, TimeoutError('Delayed receipt'), unknown=True)
        self.assertEqual(self.action()['stage'], 'unknown')
        self.server.release()
        fixture.eventually(lambda: self.receipt().get('stage') == 'running')
        self.assertEqual(len([p for m,p in self.server.calls if m == 'thread/fork']), 1)
        self.assertEqual(len([p for m,p in self.server.calls if m == 'turn/start' and p['threadId']=='forked-thread']), 1)

    def test_stop_during_pending_fork_cannot_submit_a_turn(self):
        self.server.gated = 'thread/fork'
        self.buffering(); self.action()
        fixture.eventually(lambda: self.server.gate_future is not None)
        self.runtime.stop(self.key, False)
        self.server.release()
        fixture.eventually(lambda: self.receipt().get('stage') == 'failed')
        self.assertFalse(any(m == 'turn/start' and p['threadId']=='forked-thread' for m,p in self.server.calls))

    def test_fast_completion_is_delivered_to_parent_even_before_start_receipt(self):
        from unittest.mock import patch
        self.server.finish_before_reply = True
        self.buffering()
        with patch.object(self.runtime, 'parent_event', wraps=self.runtime.parent_event) as delivered:
            self.action()
            fixture.eventually(lambda: self.receipt().get('stage') == 'running')
            self.assertFalse(self.runtime.agent(self.key)['inFlight'])
            turns = [call.args[2] for call in delivered.call_args_list]
            self.assertNotIn(self.turn, turns)
            self.assertIn(self.receipt()['acceptedTurnId'], turns)

    def test_wrong_account_connection_and_missing_model_cannot_retry(self):
        msg = {'method': 'model/safetyBuffering/updated', 'params': {'threadId': self.a['threadId'],
               'turnId': self.turn, 'showBufferingUi': True, 'fasterModel': 'gpt-5.6-sol'}}
        self.runtime.notification(msg, 'wrong-account', self.connection)
        self.runtime.notification(msg, 'default', 'old-connection')
        self.assertNotIn('nativeSafetyBuffering', self.runtime.agent(self.key))
        self.event('model/safetyBuffering/updated', showBufferingUi=True)
        with self.assertRaisesRegex(ValueError, 'did not offer'): self.action()

    def test_cancel_preserves_late_fork_receipt_but_never_starts_it(self):
        self.server.gated = 'thread/fork'
        self.buffering(); self.action()
        fixture.eventually(lambda: self.server.gate_future is not None)
        self.assertEqual(self.action('cancel')['stage'], 'cancelled')
        self.server.release()
        def retained():
            with self.runtime.db() as db:
                op = json.loads(db.execute('SELECT record FROM runtime_safety_retries').fetchone()[0])
            return op.get('responses', {}).get('fork')
        fixture.eventually(retained)
        self.assertEqual(self.receipt()['stage'], 'cancelled')
        self.assertFalse(any(m == 'turn/start' and p['threadId']=='forked-thread' for m,p in self.server.calls))

    def test_restart_retains_receipt_and_does_not_replay_fork(self):
        self.server.gated = 'thread/fork'
        self.buffering(); self.action()
        fixture.eventually(lambda: self.server.gate_future is not None)
        self.runtime.close()
        self.runtime = Runtime(Path(self.temp.name), Server)
        self.assertEqual(self.action()['stage'], 'fork')
        self.assertFalse(any(m == 'thread/fork' for m,p in self.runtime.connect().calls))
        self.assertEqual(self.action('cancel')['stage'], 'cancelled')
        from codex_safety_buffering import active
        self.assertFalse(active(self.runtime.agent(self.key)))

    def test_simultaneous_clicks_submit_one_fork(self):
        from concurrent.futures import ThreadPoolExecutor
        self.server.gated = 'thread/fork'
        self.buffering()
        with ThreadPoolExecutor(max_workers=4) as pool:
            results = list(pool.map(lambda _: self.action(), range(4)))
        self.assertEqual(len({r['id'] for r in results}), 1)
        fixture.eventually(lambda: self.server.gate_future is not None)
        self.server.release()
        fixture.eventually(lambda: self.receipt().get('stage') == 'running')
        self.assertEqual(len([m for m,p in self.server.calls if m == 'thread/fork']), 1)

    def test_model_verification_and_guardian_review_preserve_reasons(self):
        self.event('model/verification', verifications=['trustedAccessForCyber'])
        self.event('item/autoApprovalReview/started', reviewId='r', review={'status': 'inProgress'})
        self.event('item/autoApprovalReview/completed', reviewId='r', review={'status': 'denied', 'rationale': 'Outside scope'})
        with self.runtime.db() as db:
            items = [json.loads(r[0]) for r in db.execute('SELECT record FROM runtime_items WHERE agent=?', (self.key,))]
        self.assertTrue(any('additional cybersecurity checks' in i.get('text', '') for i in items))
        self.assertEqual(len([i for i in items if i.get('nativeReview')]), 1)
        self.assertTrue(any(i.get('details') == 'Outside scope' for i in items))
        self.assertTrue(self.runtime.agent(self.key)['inFlight'])

if __name__ == '__main__': unittest.main()
