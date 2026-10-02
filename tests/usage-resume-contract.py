#!/usr/bin/env python3
"""Automatic usage-limit continuation keeps the exact thread and queued input."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_runtime import Runtime
spec = importlib.util.spec_from_file_location('runtime_fixture', ROOT / 'tests/runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ManualRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.1)
            self.changed.clear()


class UsageResumeContract(unittest.TestCase):
    def test_scoped_claude_limit_notification_merges_valid_buckets(self):
        rt = self.runtime
        rt.set_rate_limits('default', {'data': {'rateLimits': {'limitId': 'claude', 'primary': {'usedPercent': 10}},
            'rateLimitsByLimitId': {'claude-sonnet': {'limitId': 'claude-sonnet', 'secondary': {'usedPercent': 1}}}},
            'at': time.time() - 5})
        self.server.notify({'method': 'account/rateLimits/updated', 'params': {
            'rateLimits': {'limitId': 'claude', 'primary': {'usedPercent': 10}},
            'rateLimitsByLimitId': {'claude-sonnet': {'limitId': 'claude-sonnet', 'secondary': {'usedPercent': 100}}}}})
        data = rt.rate_limits_for('default')['data']
        self.assertEqual(data['rateLimitsByLimitId']['claude-sonnet']['secondary']['usedPercent'], 100)
        self.assertEqual(data['rateLimitsByLimitId']['claude']['primary']['usedPercent'], 10)
        self.server.notify({'method': 'account/rateLimits/updated', 'params': {
            'rateLimits': {'limitId': 'claude', 'primary': {'usedPercent': 11}},
            'rateLimitsByLimitId': {'claude-sonnet': {'limitId': 'other', 'secondary': {'usedPercent': 0}},
                'claude-opus': 'invalid',
                'claude-other': {'limitId': 'claude-other', 'secondary': {'usedPercent': 'full'}}}}})
        data = rt.rate_limits_for('default')['data']
        self.assertEqual(data['rateLimitsByLimitId']['claude-sonnet']['secondary']['usedPercent'], 100)
        self.assertNotIn('claude-opus', data['rateLimitsByLimitId'])
        self.assertNotIn('claude-other', data['rateLimitsByLimitId'])

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.runtime = ManualRuntime(Path(self.temp.name), fixture.FakeServer)
        self.server = self.runtime.connect()
        original_call = self.server.call
        self.server.limit_result = {'rateLimits': {'primary': {'usedPercent': 100,
            'resetsAt': time.time() + 10000}}}
        def limited_call(method, params, timeout=60):
            if method == 'account/rateLimits/read':
                self.server.calls.append((method, params))
                return self.server.limit_result
            return original_call(method, params, timeout)
        self.server.call = limited_call
        self.agent = self.runtime.create({'name': 'Lead', 'cwd': self.temp.name, 'prompt': 'Original task'})
        self.key = self.agent['id']
        self.runtime.dispatch()
        fixture.eventually(lambda: self.runtime.agent(self.key).get('turnId'))
        self.fail_turn()
        # The failure starts one background limits read; let it land before tests change limits.
        fixture.eventually(lambda: self.runtime.rate_limits_for('default').get('at'))

    def tearDown(self):
        self.runtime.close()
        self.temp.cleanup()

    def fail_turn(self):
        agent = self.runtime.agent(self.key)
        self.failed_turn_id = agent['turnId']
        self.server.notify({'method': 'turn/completed', 'params': {'threadId': agent['threadId'], 'turn': {
            'id': agent['turnId'], 'status': 'failed',
            'error': {'message': 'Usage limit reached', 'codexErrorInfo': 'usageLimitExceeded'},
        }}})
        return self.runtime.agent(self.key)

    def make_due(self, *, blocked=False):
        future = time.time() + 10000
        self.server.limit_result = {'rateLimits': {'primary': {'usedPercent': 100 if blocked else 15,
            'resetsAt': future}}}
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            resume = agent['usageResume']
            resume['dueAt'] = time.time() - 1
            resume['failedAt'] = time.time() - 120
            self.runtime.usage_resume_save(db, agent, resume)
            self.runtime.put(db, 'agents', agent)

    def schedule_auth_failure(self, provider='codex', message='unexpected status 401 Unauthorized: sign-in expired'):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent.update(provider=provider, lastCompletedTurn='auth-failed-turn',
                         lastCompletedTurnStatus='failed', nativeFailureHold=True,
                         error={'message': message, 'codexErrorInfo': 'other'})
            turn = {'id': 'auth-failed-turn', 'status': 'failed', 'error': agent['error']}
            self.runtime.usage_resume_completed(db, agent, turn, True)
            self.runtime.put(db, 'agents', agent)
            self.failed_turn_id = turn['id']
        return self.runtime.agent(self.key)['usageResume']

    def test_limits_update_preserves_auth_backoff(self):
        self.schedule_auth_failure()
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            resume = agent['usageResume']
            resume.update(failedAt=time.time()-120, authAttempt=3, dueAt=time.time()+1800)
            due = resume['dueAt']
            self.runtime.usage_resume_save(db, agent, resume)
            self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_limits_changed('default', {
            'data': {'rateLimits': {'primary': {'usedPercent': 10}}}, 'at': time.time()})
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['dueAt'], due)

    def test_budget_deferred_capacity_retry_resumes_exact_action(self):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent.update(lastCompletedTurn='capacity-budget',
                error={'codexErrorInfo':'serverOverloaded'}, tokenBudget=1000000)
            self.runtime.capacity_completed(db, agent,
                {'id':'capacity-budget','status':'failed','error':agent['error']}, True)
            self.runtime.put(db, 'agents', agent)
        retry_id = self.runtime.agent(self.key)['capacityRetry']['id']
        self.runtime.capacity_retry(self.key, retry_id, 'retry')
        fixture.eventually(lambda: bool(self.runtime.agent(self.key).get('budgetActionWait')))
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['tokenBudget'] = None
            self.runtime.put(db, 'agents', agent)
        self.runtime.dispatch()
        fixture.eventually(lambda: not self.runtime.agent(self.key).get('budgetActionWait'))
        fixture.eventually(lambda: bool(self.runtime.agent(self.key)['capacityRetry'].get('acceptedTurnId')))
        self.assertEqual(self.runtime.agent(self.key)['capacityRetry']['acceptedTurnId'],
                         self.runtime.agent(self.key)['turnId'])

    def test_auth_failure_schedules_typed_resume_once_and_waits_for_account_proof(self):
        resume = self.schedule_auth_failure()
        self.assertEqual(resume['cause'], 'auth')
        self.assertEqual(resume['status'], 'scheduled')
        with self.runtime.db() as db:
            count = db.execute('SELECT COUNT(*) FROM runtime_usage_resumes').fetchone()[0]
            agent = self.runtime.agent(self.key, db)
            self.runtime.usage_resume_completed(db, agent, {'id': self.failed_turn_id,
                'status': 'failed', 'error': {'message': '401 Unauthorized'}}, True)
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_usage_resumes').fetchone()[0], count)
        original_call = self.server.call
        def unavailable(method, params, timeout=60):
            if method == 'account/rateLimits/read':
                raise RuntimeError('401 Unauthorized: token marker-not-a-secret')
            if method == 'account/read':
                return {'account': {'id': 'cached-local-account'}}
            return original_call(method, params, timeout)
        self.server.call = unavailable
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['usageResume']['dueAt'] = time.time() - 1
            agent['usageResume']['failedAt'] = time.time() - 120
            self.runtime.usage_resume_save(db, agent, agent['usageResume'])
            self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_tick()
        waiting = self.runtime.agent(self.key)['usageResume']
        self.assertEqual(waiting['status'], 'scheduled')
        self.assertTrue(waiting['waitingForAuth'])
        self.assertTrue(waiting['dueAt'] > time.time())
        self.assertNotIn('marker-not-a-secret', self.runtime.rate_limits_for('default')['error'])
        def recovered(method, params, timeout=60):
            if method == 'account/rateLimits/read':
                return {'rateLimits': {'primary': {'usedPercent': 22}}}
            if method == 'account/read':
                raise RuntimeError('The local account read is not sufficient proof')
            return original_call(method, params, timeout)
        self.server.call = recovered
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['usageResume']['dueAt'] = time.time() - 1
            agent['usageResume']['failedAt'] = time.time() - 120
            self.runtime.usage_resume_save(db, agent, agent['usageResume'])
            self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'started')

    def test_claude_expired_oauth_error_uses_auth_resume(self):
        resume = self.schedule_auth_failure('claude', 'Failed to authenticate: OAuth session expired')
        self.assertEqual(resume['cause'], 'auth')
        self.assertEqual(resume['status'], 'scheduled')

    def test_auth_resume_can_be_turned_off_and_stop_cancels_it(self):
        resume = self.schedule_auth_failure()
        cancelled = self.runtime.usage_resume_action(self.key, resume['id'], False)
        self.assertEqual(cancelled['status'], 'cancelled')
        self.runtime.stop(self.key, False, 'Stopped by user')
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'cancelled')

    def test_repeated_auth_failures_back_off_and_keep_one_scheduled_notice(self):
        from codex_usage_resume import _auth_backoff
        resume = self.schedule_auth_failure()
        self.assertEqual(resume['authAttempt'], 0)
        self.assertEqual(resume['dueAt'] - resume['failedAt'], 180)
        for number, delay in enumerate((600, 1800, 1800, 1800), start=1):
            with self.runtime.lock, self.runtime.db() as db:
                agent = self.runtime.agent(self.key, db)
                started = agent['usageResume']
                started.update(status='started', dueAt=None, startedAt=time.time())
                self.runtime.usage_resume_save(db, agent, started)
                turn_id = f'auth-retry-{number}'
                error = {'message': '401 Unauthorized: token rejected', 'codexErrorInfo': 'other'}
                agent.update(lastCompletedTurn=turn_id, lastCompletedTurnStatus='failed',
                             nativeFailureHold=True, error=error)
                self.runtime.usage_resume_completed(db, agent,
                    {'id': turn_id, 'status': 'failed', 'error': error}, True)
                self.runtime.put(db, 'agents', agent)
                resume = agent['usageResume']
                self.assertEqual(resume['authAttempt'], number)
                self.assertAlmostEqual(resume['dueAt'] - resume['failedAt'], delay, delta=.1)
                self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_usage_resumes "
                    "WHERE json_extract(record,'$.status')='scheduled' "
                    "AND json_extract(record,'$.cause')='auth'").fetchone()[0], 1)
        self.assertEqual([_auth_backoff(i) for i in (0, 1, 2, 3, 8)], [180, 600, 1800, 1800, 1800])

    def test_auth_wait_over_bound_appears_once_at_account_level(self):
        resume = self.schedule_auth_failure()
        resume['failedAt'] = time.time() - 1801
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            self.runtime.usage_resume_save(db, agent, resume)
            self.runtime.put(db, 'agents', agent)
        snapshot = self.runtime.accounts_snapshot()
        account = next(item for item in snapshot['accounts'] if item['id'] == 'default')
        self.assertIn('Sign in again', account['authenticationRecovery'])
        self.assertNotIn('authenticationRecovery', next(
            item for item in snapshot['accounts'] if item['id'] != 'default') if len(snapshot['accounts']) > 1 else {})

    def test_auth_classifier_uses_only_explicit_codes_and_verified_claude_text(self):
        from codex_usage_resume import _auth_error
        self.assertTrue(_auth_error({'message': 'unexpected status 401 Unauthorized'}))
        self.assertTrue(_auth_error({'message': 'HTTP 401'}))
        self.assertFalse(_auth_error({'message': '403 Forbidden'}))
        self.assertTrue(_auth_error({'message': '403 Forbidden: invalid API key'}))
        self.assertTrue(_auth_error({'message': '403: the token has expired'}))
        self.assertFalse(_auth_error({'message': '403 permission denied by policy'}))
        self.assertTrue(_auth_error({'message': '403 Unauthorized'}))
        self.assertTrue(_auth_error({'codexErrorInfo': 'unauthorized'}))
        self.assertTrue(_auth_error({'message': 'Failed to authenticate: OAuth session expired'}, 'claude'))
        self.assertFalse(_auth_error({'message': 'HTTP 500 internal error'}))
        self.assertFalse(_auth_error({'message': 'OAuth session expired'}, 'codex'))

    def test_changed_auth_refresh_timestamp_is_account_recovery_proof(self):
        path = Path(self.temp.name) / 'auth.json'
        path.write_text(json.dumps({'last_refresh': 'first-refresh', 'tokens': {}}))
        with self.runtime.accounts.lock:
            self.runtime.accounts._row('default')['home'] = self.temp.name
        before = self.runtime.usage_resume_auth_marker('default')
        metadata = path.stat()
        path.write_text(json.dumps({'last_refresh': 'second-refresh', 'tokens': {}}))
        os.utime(path, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
        self.assertNotEqual(self.runtime.usage_resume_auth_marker('default'), before)

    def test_refresh_timestamp_change_starts_auth_resume_without_rpc_success(self):
        path = Path(self.temp.name) / 'auth.json'
        path.write_text(json.dumps({'last_refresh': 'first-refresh', 'tokens': {}}))
        with self.runtime.accounts.lock:
            self.runtime.accounts._row('default')['home'] = self.temp.name
        resume = self.schedule_auth_failure()
        metadata = path.stat()
        path.write_text(json.dumps({'last_refresh': 'second-refresh', 'tokens': {}}))
        os.utime(path, ns=(metadata.st_atime_ns, metadata.st_mtime_ns))
        original_call = self.server.call
        def unavailable(method, params, timeout=60):
            if method == 'account/read':
                raise RuntimeError('Account sign-in is not available')
            return original_call(method, params, timeout)
        self.server.call = unavailable
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['usageResume']['dueAt'] = time.time() - 1
            agent['usageResume']['failedAt'] = time.time() - 120
            self.runtime.usage_resume_save(db, agent, agent['usageResume'])
            self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'started')

    def test_planned_time_uses_only_exhausted_windows(self):
        from codex_usage_resume import _reset_at
        now = 1_000_000
        five_hour = {'usedPercent': 100, 'resetsAt': now + 3600}
        weekly = {'usedPercent': 40, 'resetsAt': now + 5 * 86400}
        data = {'rateLimits': {'primary': five_hour, 'secondary': weekly},
                'rateLimitsByLimitId': {'codex': {'primary': five_hour, 'secondary': weekly},
                                        'codex-other': {'secondary': {'usedPercent': 10, 'resetsAt': now + 9 * 86400}}}}
        # A weekly window with room left must not push the planned resume days away.
        self.assertEqual(_reset_at(data), now + 3600)
        data['rateLimits']['secondary'] = {'usedPercent': 100, 'resetsAt': now + 5 * 86400}
        self.assertEqual(_reset_at(data), now + 5 * 86400)
        self.assertIsNone(_reset_at({'rateLimits': {'primary': {'usedPercent': 20, 'resetsAt': now + 60}}}))
        reached = {'rateLimits': {'rateLimitReachedType': 'primary', 'primary': {'usedPercent': 99, 'resetsAt': now + 120}}}
        self.assertEqual(_reset_at(reached), now + 120)

    def test_known_reset_is_checked_once_at_that_time(self):
        from codex_usage_resume import POLL_SECONDS, RESET_GRACE_SECONDS, _next_check
        now = 1_000_000
        # A reset hours away needs no polling before it.
        self.assertEqual(_next_check(now + 4 * 3600, now), now + 4 * 3600 + RESET_GRACE_SECONDS)
        self.assertEqual(_next_check(None, now), now + POLL_SECONDS)
        self.assertEqual(_next_check(now - 10, now), now + POLL_SECONDS)

    def test_relief_right_after_failure_waits_the_minimum_pause(self):
        from codex_usage_resume import RESUME_MIN_SECONDS
        resume = self.runtime.agent(self.key)['usageResume']
        self.runtime.usage_resume_limits_changed('default', {'data': {'rateLimits': {'primary': {
            'usedPercent': 15, 'resetsAt': time.time() + 10000}}}})
        due = self.runtime.agent(self.key)['usageResume']['dueAt']
        self.assertAlmostEqual(due, resume['failedAt'] + RESUME_MIN_SECONDS, delta=1)
        calls = list(self.server.calls)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.server.calls, calls)
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'scheduled')

    def test_relief_still_applies_to_resume_saved_before_typed_causes(self):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            resume = agent['usageResume']
            resume.pop('cause', None)
            resume['failedAt'] = time.time() - 120
            resume['dueAt'] = time.time() + 10000
            self.runtime.usage_resume_save(db, agent, resume)
            self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_limits_changed('default', {'data': {'rateLimits': {'primary': {
            'usedPercent': 15, 'resetsAt': time.time() + 10000}}}})
        self.assertLessEqual(self.runtime.agent(self.key)['usageResume']['dueAt'], time.time() + 1)

    def test_failed_turn_schedules_once_and_restart_keeps_identity(self):
        fixture.eventually(lambda: self.runtime.rate_limits_for('default').get('at'))
        reset = self.runtime.rate_limits_for('default')['data']['rateLimits']['primary']['resetsAt']
        before = self.runtime.agent(self.key)['usageResume']
        self.assertEqual(before['status'], 'scheduled')
        self.runtime.notification({'method': 'turn/completed', 'params': {
            'threadId': before['threadId'], 'turn': {'id': self.failed_turn_id, 'status': 'failed',
                'error': {'message': 'Usage limit reached', 'codexErrorInfo': 'usageLimitExceeded'}}}},
            'default', self.runtime.connection_ids['default'])
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['id'], before['id'])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_usage_resumes').fetchone()[0], 1)
        self.assertEqual(before['turnId'], self.runtime.agent(self.key)['lastCompletedTurn'])
        self.assertEqual(before['threadId'], self.runtime.agent(self.key)['threadId'])
        self.assertGreater(before['dueAt'], before['updatedAt'])
        self.assertAlmostEqual(before['plannedAt'], reset, delta=1)
        self.runtime.close()
        self.runtime = Runtime(Path(self.temp.name), fixture.FakeServer)
        after = self.runtime.agent(self.key)['usageResume']
        self.assertEqual(after['id'], before['id'])
        self.assertEqual(after['status'], 'scheduled')

    def test_resume_delivers_waiting_input_before_one_continuation(self):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            for event_id, text in [
                ('queued-message', 'Queued user message'),
                ('queued-monitor', 'Queued monitor result'),
                ('queued-child', 'Queued child result'),
            ]:
                self.runtime.enqueue(db, agent, 'followup', text, event_id)
        self.make_due()
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'started')
        self.assertFalse(self.runtime.agent(self.key).get('nativeFailureHold'))
        self.runtime.dispatch()
        fixture.eventually(lambda: len([call for call in self.server.calls if call[0] == 'turn/start']) == 2)
        started = [params for method, params in self.server.calls if method == 'turn/start'][-1]
        text = json.dumps(started.get('input', []))
        positions = [text.index(value) for value in (
            'Queued user message', 'Queued monitor result', 'Queued child result',
            'previous turn stopped because this account reached a usage or rate limit',
        )]
        self.assertEqual(positions, sorted(positions))
        self.assertIn('Check the current task and conversation state', text)
        self.runtime.usage_resume_tick()
        self.assertEqual(len([1 for method, _ in self.server.calls if method == 'turn/start']), 2)

    def test_limits_still_block_and_opt_out_stop_and_account_move_cancel(self):
        self.make_due(blocked=True)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'scheduled')
        resume = self.runtime.agent(self.key)['usageResume']
        self.runtime.usage_resume_action(self.key, resume['id'], False)
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'cancelled')
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent.update(lastCompletedTurn='later-failed-turn', lastCompletedTurnStatus='failed',
                         nativeFailureHold=True, error={'message': 'Rate limit reached',
                         'codexErrorInfo': 'rateLimitExceeded'})
            self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_action(self.key, resume['id'], True)
        reenabled = self.runtime.agent(self.key)['usageResume']
        self.assertEqual(reenabled['status'], 'scheduled')
        self.assertEqual(reenabled['turnId'], 'later-failed-turn')
        self.runtime.stop(self.key, False, 'Stopped by user')
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'cancelled')

    def test_account_change_cancels_old_account_resume(self):
        resume = self.runtime.agent(self.key)['usageResume']
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.key, db)
            agent['accountKey'] = 'new-account'
            resume['dueAt'] = time.time() - 1
            resume['failedAt'] = time.time() - 120
            self.runtime.usage_resume_save(db, agent, resume)
            self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'cancelled')

    def failed_worker(self):
        worker = self.runtime.create({'name': 'Worker', 'prompt': 'Child task', 'role': 'reviewer'}, parent=self.key)
        self.runtime.dispatch()
        fixture.eventually(lambda: self.runtime.agent(worker['id']).get('turnId'))
        worker_server = self.runtime.servers['default']
        worker_agent = self.runtime.agent(worker['id'])
        reads = sum(method == 'account/rateLimits/read' for method, _ in worker_server.calls)
        worker_server.notify({'method': 'turn/completed', 'params': {'threadId': worker_agent['threadId'], 'turn': {
            'id': worker_agent['turnId'], 'status': 'failed',
                'error': {'message': 'Rate limit reached', 'codexErrorInfo': 'rateLimitExceeded'},
        }}})
        # Let the failure's background limits read land before the test changes limits.
        fixture.eventually(lambda: sum(method == 'account/rateLimits/read' for method, _ in worker_server.calls) > reads)
        return worker, worker_server

    def test_same_account_team_resumes_lead_and_worker(self):
        worker, worker_server = self.failed_worker()
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'scheduled')
        self.assertEqual(self.runtime.agent(worker['id'])['usageResume']['status'], 'scheduled')
        worker_server.limit_result = {'rateLimits': {'primary': {
            'usedPercent': 10, 'resetsAt': time.time() + 10000}}}
        with self.runtime.lock, self.runtime.db() as db:
            for key in (self.key, worker['id']):
                agent = self.runtime.agent(key, db)
                agent['usageResume']['dueAt'] = time.time() - 1
                agent['usageResume']['failedAt'] = time.time() - 120
                self.runtime.usage_resume_save(db, agent, agent['usageResume'])
                self.runtime.put(db, 'agents', agent)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(self.key)['usageResume']['status'], 'started')
        self.assertEqual(self.runtime.agent(worker['id'])['usageResume']['status'], 'started')
        self.assertFalse(self.runtime.agent(self.key).get('nativeFailureHold'))
        self.assertFalse(self.runtime.agent(worker['id']).get('nativeFailureHold'))
        self.runtime.dispatch()
        fixture.eventually(lambda: len([1 for method, _ in worker_server.calls if method == 'turn/start']) == 4)

    def test_user_stopped_parent_holds_worker_resume(self):
        worker, worker_server = self.failed_worker()
        worker_server.limit_result = {'rateLimits': {'primary': {
            'usedPercent': 10, 'resetsAt': time.time() + 10000}}}
        self.runtime.stop(self.key, False, 'Stopped by user')
        with self.runtime.lock, self.runtime.db() as db:
            child = self.runtime.agent(worker['id'], db)
            child['usageResume']['dueAt'] = time.time() - 1
            child['usageResume']['failedAt'] = time.time() - 120
            self.runtime.usage_resume_save(db, child, child['usageResume'])
            self.runtime.put(db, 'agents', child)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(worker['id'])['usageResume']['status'], 'cancelled')
        self.assertTrue(self.runtime.agent(worker['id'])['nativeFailureHold'])


if __name__ == '__main__':
    unittest.main()
