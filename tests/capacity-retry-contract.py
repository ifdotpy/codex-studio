#!/usr/bin/env python3
"""Capacity continuation uses empty input and one durable claim per failed turn."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime, ResponseTimeout
from codex_native_errors import NativeRpcError
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CapacityContract(unittest.TestCase):
    def test_temporary_slot_wait_reschedules_retry(self):
        retry = self.fail()
        slot_ceiling = patch.dict('os.environ', {'CODEX_CANVAS_CONCURRENCY': '1'})
        slot_ceiling.start()
        self.addCleanup(slot_ceiling.stop)
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['concurrency'] = 1
            agent['capacityRetry']['dueAt'] = time.time()-1
            self.runtime.capacity_save(db, agent, agent['capacityRetry'])
            self.runtime.put(db, 'agents', agent)
            sibling = dict(agent, id='busy-sibling', parentId=agent['id'],
                inFlight=True, status='running', turnId='busy-turn')
            sibling.pop('capacityRetry', None)
            self.runtime.put(db, 'agents', sibling)
        self.runtime.capacity_tick()
        current = self.runtime.agent(self.key)['capacityRetry']
        self.assertEqual(current['status'], 'scheduled')
        self.assertGreater(current['dueAt'], time.time())
        self.assertFalse(current.get('claimedAt'))
        original_submit = self.runtime.pool.submit
        retry_futures = []

        def track_retry(function, *args, **kwargs):
            future = original_submit(function, *args, **kwargs)
            if (getattr(function, '__name__', None) == 'capacity_run'
                    and args and args[0] == self.key):
                retry_futures.append(future)
            return future

        with patch.object(self.runtime.pool, 'submit', side_effect=track_retry):
            with self.runtime.lock, self.runtime.db() as db:
                sibling = self.runtime.agent('busy-sibling', db)
                sibling.update(inFlight=False, status='completed', turnId=None)
                self.runtime.put(db, 'agents', sibling)
                agent = self.runtime.agent(self.key, db)
                agent['capacityRetry']['dueAt'] = time.time()-1
                self.runtime.capacity_save(db, agent, agent['capacityRetry'])
                self.runtime.put(db, 'agents', agent)
            self.runtime.capacity_tick()
        self.assertEqual(len(retry_futures), 1)
        retry_futures[0].result()
        self.assertTrue(self.runtime.agent(self.key)['capacityRetry'].get('acceptedTurnId'))

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.runtime = Runtime(self.root, fixture.FakeServer)
        self.server = self.runtime.connect()
        self._start_futures = {}
        self._start_condition = threading.Condition()
        executor = self.runtime.delivery_executor()
        original_submit = executor.submit

        def track_start(function, *args, **kwargs):
            future = original_submit(function, *args, **kwargs)
            if getattr(function, '__name__', None) == 'start' and args:
                agent_id = args[0]['id']
                with self._start_condition:
                    self._start_futures.setdefault(agent_id, []).append(future)
                    self._start_condition.notify_all()
            return future

        self._submit_patch = patch.object(executor, 'submit', side_effect=track_start)
        self._submit_patch.start()
        self.addCleanup(self._submit_patch.stop)
        a = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Original task'})
        self.key = a['id']
        self.wait_start(self.key)

    def tearDown(self):
        if self.server.start_gate:
            self.server.start_gate.set()
        self.runtime.close()
        self.temp.cleanup()

    def agent(self):
        return self.runtime.agent(self.key)

    def test_capacity_poll_uses_the_status_due_index(self):
        with self.runtime.db() as db:
            plan = [row[3] for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT record FROM runtime_agents WHERE "
                "json_extract(record,'$.capacityRetry.status')='scheduled' "
                "AND json_extract(record,'$.capacityRetry.dueAt')<=? UNION ALL "
                "SELECT record FROM runtime_agents WHERE "
                "json_extract(record,'$.capacityRetry.status')='failed' "
                "AND json_extract(record,'$.capacityRetry.acceptedTurnId') IS NULL "
                "AND json_extract(record,'$.capacityRetry.reason') LIKE 'Context repair waits for %'",
                (time.time(),))]
        self.assertTrue(any("runtime_agent_capacity_retry_state_due_v2" in step for step in plan), plan)

    def starts(self):
        return [p for method, p in self.server.calls if method == 'turn/start']

    def wait_start(self, agent_id):
        with self._start_condition:
            while not self._start_futures.get(agent_id):
                self._start_condition.wait()
            future = self._start_futures[agent_id].pop(0)
        future.result()

    def fail(self, info='serverOverloaded', turn_id=None):
        a = self.agent()
        turn_id = turn_id or a['turnId']
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': a['threadId'],
            'turn': {'id': turn_id, 'status': 'failed',
                     'error': {'message': 'Model is at capacity', 'codexErrorInfo': info}}}})
        return self.agent().get('capacityRetry')

    def retry(self, retry=None, action='retry'):
        return self.runtime.capacity_retry(self.key, (retry or self.agent()['capacityRetry'])['id'], action)

    def retry_and_wait(self, retry=None, action='retry'):
        result, futures = self.retry_captured(retry, action)
        self.assertEqual(len(futures), 1)
        futures[0].result()
        return result

    def retry_captured(self, retry=None, action='retry'):
        original_submit = self.runtime.pool.submit
        futures = []

        def track_retry(function, *args, **kwargs):
            future = original_submit(function, *args, **kwargs)
            if (getattr(function, '__name__', None) == 'capacity_run'
                    and args and args[0] == self.key):
                futures.append(future)
            return future

        with patch.object(self.runtime.pool, 'submit', side_effect=track_retry):
            result = self.retry(retry, action)
        return result, futures

    def expire(self):
        original_submit = self.runtime.pool.submit
        futures = []

        def track_retry(function, *args, **kwargs):
            future = original_submit(function, *args, **kwargs)
            if (getattr(function, '__name__', None) == 'capacity_run'
                    and args and args[0] == self.key):
                futures.append(future)
            return future

        with patch.object(self.runtime.pool, 'submit', side_effect=track_retry):
            self._expire()
        for future in futures:
            future.result()

    def _expire(self):
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
            self.assertEqual(self.starts()[-1]['input'], [])
            self.assertNotIn('clientUserMessageId', self.starts()[-1])
            self.assertEqual(self.agent()['capacityRetryCount'], index + 1)
        retry = self.fail()
        self.assertEqual(retry['status'], 'exhausted')
        self.assertIsNone(retry['dueAt'])
        self.runtime.capacity_tick()
        self.assertEqual(len(self.starts()), 5)
        self.retry_and_wait(retry)
        self.assertEqual(len(self.starts()), 6)
        self.assertEqual(self.fail()['status'], 'exhausted')

    def test_connection_failures_retry_longer_with_the_same_continuation(self):
        retry = self.fail({'httpConnectionFailed': {'httpStatusCode': None}})
        self.assertEqual((retry['cause'], retry['maxAttempts'], retry['status']), ('httpConnectionFailed', 7, 'scheduled'))
        self.assertAlmostEqual(retry['dueAt'] - time.time(), 10, delta=1)
        self.expire()
        self.assertEqual(self.starts()[-1]['input'], [])
        retry = self.fail('responseStreamDisconnected')
        self.assertEqual((retry['cause'], retry['attempt']), ('responseStreamDisconnected', 2))
        self.assertAlmostEqual(retry['dueAt'] - time.time(), 30, delta=1)

    def test_request_and_context_errors_are_not_retried(self):
        for info in ('badRequest', 'contextWindowExceeded', {'other': {}}):
            self.assertIsNone(self.fail(info))
            self.assertEqual(len(self.starts()), 1)

    def test_failed_turn_projects_capacity_retry_renderer_fields(self):
        retry = self.fail()
        with self.runtime.db() as db:
            value = json.loads(db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.key,)).fetchone()[0])["value"]["capacityRetry"]
        self.assertEqual(value["id"], retry["id"])
        self.assertEqual(value["status"], retry["status"])
        self.assertEqual(value["threadId"], retry["threadId"])
        self.assertNotIn("cwd", value)
        self.assertNotIn("settings", value)

    def test_cancel_and_stale_timer_do_not_claim_but_manual_retry_can(self):
        retry = self.fail()
        result = self.retry(retry, 'cancel')
        self.assertEqual(result['status'], 'cancelled')
        self.assertGreaterEqual(result['updatedAt'], retry['updatedAt'])
        self.runtime.capacity_retry(self.key, retry['id'], 'retry', _automatic=True)
        self.assertEqual(len(self.starts()), 1)
        self.assertTrue(self.agent()['autoWake'])
        self.retry_and_wait(retry)
        self.assertEqual(self.retry(retry, 'cancel')['status'], 'starting')
        self.assertEqual(len(self.starts()), 2)

    def test_duplicate_http_actions_and_scheduler_race_claim_once(self):
        retry = self.fail()
        original_submit = self.runtime.pool.submit
        futures = []

        def track_retry(function, *args, **kwargs):
            future = original_submit(function, *args, **kwargs)
            if getattr(function, '__name__', None) == 'capacity_run' and args and args[0] == self.key:
                futures.append(future)
            return future

        with patch.object(self.runtime.pool, 'submit', side_effect=track_retry):
            with ThreadPoolExecutor(max_workers=8) as pool:
                results = list(pool.map(lambda _: self.retry(retry), range(16)))
        self.assertEqual(len(futures), 1)
        futures[0].result()
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
        self.retry_and_wait()
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='held-event'").fetchone()[0], 'pending')
        a = self.agent()
        started = threading.Event()
        original = self.server.call

        def track_followup(method, params, timeout=60):
            try:
                return original(method, params, timeout)
            finally:
                if method == 'turn/start' and 'A later task' in str(params.get('input')):
                    started.set()

        with patch.object(self.server, 'call', side_effect=track_followup):
            self.server.complete(a['threadId'], a['turnId'])
            self.runtime.dispatch()
            started.wait()
        self.assertEqual(len(self.starts()), 3)
        self.assertIn('A later task', self.starts()[-1]['input'][0]['text'])
        self.assertNotIn('capacityRetryCount', self.agent())

    def test_unknown_native_outcome_and_restart_never_replay(self):
        retry = self.fail()
        self.server.fail_start = True
        self.retry_and_wait(retry)
        self.assertEqual(self.agent()['capacityRetry']['status'], 'unknown')
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
        with self.runtime.db() as db:
            receipt = db.execute("SELECT id,turn_id FROM runtime_events WHERE agent=? "
                                 "AND status='delivered' ORDER BY created LIMIT 1", (self.key,)).fetchone()
        self.assertIsNotNone(receipt)
        self.runtime.close()
        self.runtime = Runtime(self.root, fixture.FakeServer)
        self.server = self.runtime.connect()
        original_call = self.server.call
        def exact_native_history(method, params, timeout=60):
            if method == 'thread/read':
                return {'thread': {'id': params['threadId'], 'status': {'type': 'idle'}}}
            if method == 'thread/turns/list':
                return {'data': [{'id': receipt['turn_id'],
                                  'clientUserMessageId': receipt['id']}], 'nextCursor': None}
            return original_call(method, params, timeout)
        self.server.call = exact_native_history
        self.assertEqual(self.agent()['capacityRetry']['status'], 'cancelled')
        self.server.seq = 100  # Native turn identities do not restart with the process.
        self.retry_and_wait(retry)
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
        self.retry_and_wait(retry)
        self.assertEqual(self.agent()['capacityRetry']['status'], 'failed')
        self.assertEqual(self.agent().get('capacityRetryCount', 0), 0)
        self.server.call = original
        self.retry(retry)
        self.assertEqual(len(self.starts()), 1)

    def test_context_repair_wait_reschedules_without_using_an_attempt(self):
        import codex_context_repair
        retry = self.fail()
        original = codex_context_repair.repair_before_start
        def busy(rt, a):
            raise codex_context_repair._waiting('Context repair waits for tasks: busy-command')
        codex_context_repair.repair_before_start = busy
        try:
            self.retry_and_wait(retry)
        finally:
            codex_context_repair.repair_before_start = original
        waiting = self.agent()['capacityRetry']
        self.assertEqual(waiting['status'], 'scheduled')
        self.assertNotIn('claimedAt', waiting)
        self.assertAlmostEqual(waiting['dueAt'] - time.time(), 15, delta=2)
        self.assertEqual(self.agent().get('capacityRetryCount', 0), 0)
        self.assertEqual(len(self.starts()), 1)
        self.expire()
        self.assertEqual(self.starts()[-1]['input'], [])

    def test_old_failed_context_wait_is_scheduled_again(self):
        retry = self.fail()
        with self.runtime.lock, self.runtime.db() as db:
            a = self.agent()
            a['capacityRetry'].update(status='failed', dueAt=None, claimedAt=time.time(),
                                      reason='Context repair waits for tasks: old-command')
            a['startAttempt'] = {'id': 'capacity:' + retry['id'], 'epoch': a['epoch'], 'events': [],
                                 'action': 'capacity', 'submitted': False, 'capacityRetryId': retry['id']}
            self.runtime.capacity_save(db, a, a['capacityRetry'])
            self.runtime.put(db, 'agents', a)
        self.runtime.capacity_tick()
        again = self.agent()['capacityRetry']
        self.assertEqual((again['id'], again['status'], again['waits']), (retry['id'], 'scheduled', 1))

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
        self.assertEqual(self.agent()['capacityRetry']['status'], 'scheduled')
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
        terminal = threading.Event()
        def complete_before_ack(method, params, timeout=60):
            if method != 'turn/start':
                return original(method, params, timeout)
            self.server.calls.append((method, params))
            self.server.notify({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                'turn': {'id': 'retry-terminal', 'status': 'failed',
                         'error': {'message': 'capacity', 'codexErrorInfo': 'serverOverloaded'}}}})
            terminal.set()
            ack.wait()
            return {'turn': {'id': 'retry-terminal'}}
        self.server.call = complete_before_ack
        try:
            _, futures = self.retry_captured(retry)
            terminal.wait()
            self.assertEqual(self.agent().get('lastCompletedTurn'), 'retry-terminal')
            self.assertEqual(self.agent()['capacityRetry']['id'], retry['id'])
            self.assertEqual(self.agent().get('capacityRetryCount', 0), 0)
            ack.set()
            self.assertEqual(len(futures), 1)
            futures[0].result()
            self.assertNotEqual(self.agent()['capacityRetry']['id'], retry['id'])
            self.assertEqual(self.agent()['capacityRetry']['attempt'], 2)
            self.assertEqual(self.agent()['capacityRetryCount'], 1)
            self.assertEqual(len(self.starts()), 2)
        finally:
            ack.set()

    def test_success_before_ack_without_started_notification_releases_pending_input(self):
        retry = self.fail()
        original = self.server.call
        ack = threading.Event()
        terminal = threading.Event()
        def complete_before_ack(method, params, timeout=60):
            if method != 'turn/start' or params.get('input'):
                return original(method, params, timeout)
            self.server.calls.append((method, params))
            self.server.notify({'method': 'turn/completed', 'params': {'threadId': params['threadId'],
                'turn': {'id': 'retry-success', 'status': 'completed'}}})
            terminal.set()
            ack.wait()
            return {'turn': {'id': 'retry-success'}}
        self.server.call = complete_before_ack
        try:
            with self.runtime.lock, self.runtime.db() as db:
                self.runtime.enqueue(db, self.agent(), 'followup', 'Later task', 'later-task')
            _, futures = self.retry_captured(retry)
            terminal.wait()
            self.assertEqual(self.agent().get('lastCompletedTurn'), 'retry-success')
            self.runtime.dispatch()
            self.assertEqual(len(self.starts()), 2)
            ack.set()
            self.assertEqual(len(futures), 1)
            futures[0].result()
            self.runtime.dispatch()
            self.wait_start(self.key)
            self.assertEqual(len(self.starts()), 3)
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
        self.retry_and_wait(retry)
        self.assertEqual(self.agent()['capacityRetry']['status'], 'unknown')
        self.retry(retry)
        self.server.wait = wait
        original_submit = self.runtime.pool.submit
        futures = []
        submitted = threading.Event()

        def track_result(function, *args, **kwargs):
            future = original_submit(function, *args, **kwargs)
            if getattr(function, '__name__', None) == 'late_result':
                futures.append(future)
                submitted.set()
            return future

        with patch.object(self.runtime.pool, 'submit', side_effect=track_result):
            self.server.start_gate.set()
            submitted.wait()
        self.assertEqual(len(futures), 1)
        futures[0].result()
        self.assertTrue(self.agent()['capacityRetry'].get('acceptedTurnId'))
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
        self.retry_and_wait(retry)
        self.assertEqual(len(self.starts()), 1)


    def test_duplicate_internal_dispatch_does_not_submit_again(self):
        retry = self.fail()
        self.retry_and_wait(retry)
        self.runtime.run_native_action(self.key, self.agent()['startAttempt'])
        self.assertEqual(len(self.starts()), 2)

    def test_missing_native_turn_identity_remains_unknown(self):
        retry = self.fail()
        original = self.server.call
        def malformed(method, params, timeout=60):
            return {} if method == 'turn/start' else original(method, params, timeout)
        self.server.call = malformed
        self.retry_and_wait(retry)
        self.assertEqual(self.agent()['capacityRetry']['status'], 'unknown')
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
        self.retry_and_wait(retry)
        self.assertEqual(self.agent()['capacityRetry']['status'], 'failed')
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
        with patch.dict('os.environ', {'CODEX_CANVAS_CONCURRENCY': '1'}):
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
                release.wait()
            return original(a)
        self.runtime.prepare = gated
        try:
            self.retry(retry)
            entered.wait()
            self.runtime.send(self.key, 'Replacement task')
            self.wait_start(self.key)
            self.assertTrue(self.agent().get('turnId'))
            release.set()
            self.assertEqual(len(self.starts()), 2)
            self.assertIn('Replacement task', self.starts()[-1]['input'][0]['text'])
            self.assertNotIn('capacityRetry', self.agent())
            self.assertTrue(self.agent()['inFlight'])
        finally:
            release.set()

    def test_new_message_uses_its_own_reservation_after_unknown_retry(self):
        retry = self.fail()
        self.server.fail_start = True
        self.retry_and_wait(retry)
        self.assertEqual(self.agent()['capacityRetry']['status'], 'unknown')
        self.runtime.send(self.key, 'Follow-up task')
        self.runtime.dispatch()
        self.wait_start(self.key)
        self.assertEqual(len(self.starts()), 3)
        self.assertTrue(self.agent()['inFlight'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE agent=? AND status='uncertain'",
                                        (self.key,)).fetchone()[0], 1)
        self.assertEqual(sum('Follow-up task' in str(p['input']) for p in self.starts()), 1)

    def test_stop_and_new_user_instruction_replace_timer(self):
        retry = self.fail()
        self.runtime.stop(self.key)
        self.retry(retry)
        self.assertEqual(len(self.starts()), 1)
        self.assertNotIn('capacityRetry', self.agent())
        self.runtime.send(self.key, 'New task')
        self.runtime.dispatch()
        self.wait_start(self.key)
        self.assertEqual(self.fail()['attempt'], 1)


if __name__ == '__main__':
    unittest.main()
