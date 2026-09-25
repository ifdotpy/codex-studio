#!/usr/bin/env python3
"""Automatic usage-limit continuation keeps the exact thread and queued input."""
import importlib.util
import json
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
            self.runtime.usage_resume_save(db, agent, resume)
            self.runtime.put(db, 'agents', agent)

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
        worker_server.notify({'method': 'turn/completed', 'params': {'threadId': worker_agent['threadId'], 'turn': {
            'id': worker_agent['turnId'], 'status': 'failed',
                'error': {'message': 'Rate limit reached', 'codexErrorInfo': 'rateLimitExceeded'},
        }}})
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
            self.runtime.usage_resume_save(db, child, child['usageResume'])
            self.runtime.put(db, 'agents', child)
        self.runtime.usage_resume_tick()
        self.assertEqual(self.runtime.agent(worker['id'])['usageResume']['status'], 'cancelled')
        self.assertTrue(self.runtime.agent(worker['id'])['nativeFailureHold'])


if __name__ == '__main__':
    unittest.main()
