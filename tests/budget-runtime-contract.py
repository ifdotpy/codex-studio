#!/usr/bin/env python3
"""Real Runtime budget deferral, fake native requests, isolated account files."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('budget_runtime_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class Server(f.FakeServer):
    def __init__(self, root, notify, request, died, homes):
        super().__init__(root, notify, request, died)
        self.home = homes[root.name if root.parent.name == 'account-servers' else 'default']
        (self.home / 'sessions').mkdir(exist_ok=True)
        self.total = {}

    def path(self, thread):
        return self.home / 'sessions' / ('rollout-' + thread + '.jsonl')

    def append(self, thread, kind, payload):
        with self.path(thread).open('a') as handle:
            handle.write(json.dumps({'timestamp': time.time(), 'type': kind, 'payload': payload}) + '\n')

    def call(self, method, params, timeout=60):
        if method == 'review/start':
            self.calls.append((method, dict(params)))
            turn = {'id': str(uuid.uuid4()), 'status': 'inProgress'}
            self.notify({'method': 'turn/started', 'params': {'threadId': params['threadId'], 'turn': turn}})
            return {'turn': turn}
        if method == 'thread/compact/start':
            self.calls.append((method, dict(params)))
            return {}
        result = super().call(method, params, timeout)
        if method == 'thread/start':
            result['thread']['id'] = str(uuid.uuid4())
            self.append(result['thread']['id'], 'session_meta', {'id': result['thread']['id']})
        if method == 'turn/start':
            thread, turn = params['threadId'], result['turn']['id']
            self.append(thread, 'event_msg', {'type': 'task_started', 'turn_id': turn})
            self.total[thread] = self.total.get(thread, 0) + 10
            self.append(thread, 'token_usage_record', {'thread_id': thread, 'turn_id': turn,
                'response_id': str(uuid.uuid4()), 'usage': {'total_tokens': 10},
                'thread_token_usage': {'total_tokens': self.total[thread]}})
        return result

    def complete(self, thread, turn, text='Done'):
        super().complete(thread, turn, text)
        self.append(thread, 'event_msg', {'type': 'task_complete', 'turn_id': turn})


class BudgetRuntimeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.home, self.other = self.root / 'native', self.root / 'other-native'
        for home in (self.home, self.other):
            home.mkdir()
            (home / 'auth.json').write_text(json.dumps({'tokens': {'account_id': home.name, 'access_token': 'fixture'}}))
        self.env = patch.dict(os.environ, {'CODEX_HOME': str(self.home)})
        self.env.start()
        self.homes = {'default': self.home}
        self.runtime = f.Runtime(self.root / 'state', lambda *args: Server(*args, self.homes))
        self.other_key = self.runtime.accounts.register(str(self.other))
        self.homes[self.other_key] = self.other

    def tearDown(self):
        self.runtime.close()
        self.env.stop()
        self.temp.cleanup()

    def lead(self, account='default'):
        a = self.runtime.create({'name': 'Budget lead', 'cwd': str(self.root), 'prompt': 'First', 'account_key': account})
        f.eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        self.a = self.runtime.agent(a['id'])
        self.server = self.runtime.connect(account)
        f.eventually(lambda: any(m == 'turn/start' for m, _ in self.server.calls) and self.server.path(self.a['threadId']).exists())
        self.server.complete(self.a['threadId'], self.a['turnId'])
        self.runtime.analytics_history_step()
        self.runtime.configure(self.a['id'], {'tokenBudget': 1000})
        return self.runtime.agent(self.a['id'])

    def count(self, method):
        return sum(m == method for m, _ in self.server.calls)

    def lower_during_prepare(self):
        original = self.runtime.prepare
        invoked = []
        def prepare(a):
            result = original(a)
            if not invoked:
                invoked.append(a['startAttempt']['id'])
                self.runtime.configure(a['id'], {'tokenBudget': 1})
            return result
        return patch.object(self.runtime, 'prepare', side_effect=prepare), invoked

    def test_second_turn_waits_for_budget_then_keeps_attempt_and_input(self):
        a = self.lead()
        wrapper, attempts = self.lower_during_prepare()
        with wrapper:
            self.runtime.send(a['id'], 'Second exact input', message_id='second-exact')
            f.eventually(lambda: bool(self.runtime.agent(a['id']).get('budgetStartWait')))
            self.assertEqual(self.count('turn/start'), 1)
            self.runtime.configure(a['id'], {'tokenBudget': 1000})
            f.eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        current = self.runtime.agent(a['id'])
        self.assertEqual(current['startAttempt']['id'], attempts[0])
        self.assertEqual(self.count('turn/start'), 2)
        calls = [p for m, p in self.server.calls if m == 'turn/start']
        self.assertEqual(calls[-1]['clientUserMessageId'], 'second-exact')
        self.runtime.dispatch()
        self.assertEqual(self.count('turn/start'), 2)

    def test_repaired_thread_waits_for_real_importer_then_resumes_same_attempt(self):
        a = self.lead()
        old = a['threadId']
        new = str(uuid.uuid4())
        attempts = []
        original_text = self.server.path(old).read_text()
        records = [json.loads(line) for line in original_text.splitlines()]
        records[0]['payload']['id'] = new
        def repair(runtime, stale):
            with runtime.lock, runtime.db() as db:
                current = runtime.agent(stale['id'], db)
                if not attempts:
                    attempts.append(current['startAttempt']['id'])
                    current['threadId'] = new
                    current['startAttempt']['threadId'] = new
                    current['contextRepair'] = {'agent': current['id'], 'phase': 'completed', 'newThreadId': new,
                        'source': {'id': current['id'], 'accountKey': current['accountKey'], 'epoch': current['epoch'],
                                   'threadId': old, 'attemptId': attempts[0]}, 'snapshot': {}}
                    runtime.loaded.discard(current['id'])
                    runtime.put(db, 'agents', current)
                return current
        with patch('codex_context_repair.repair_before_start', side_effect=repair):
            self.runtime.send(a['id'], 'Continue after repair', message_id='after-repair')
            f.eventually(lambda: bool(self.runtime.agent(a['id']).get('budgetStartWait')))
            self.assertEqual(self.count('turn/start'), 1)
            self.server.path(new).write_text(''.join(json.dumps(row) + '\n' for row in records))
            self.runtime._analytics_history_paths.clear()
            self.runtime.analytics_history_start()
            f.eventually(lambda: self.runtime.agent(a['id'])['status'] == 'running')
        current = self.runtime.agent(a['id'])
        self.assertEqual(current['startAttempt']['id'], attempts[0])
        self.assertEqual(current['threadId'], new)
        self.assertEqual(self.count('turn/start'), 2)
        self.assertEqual([p for m, p in self.server.calls if m == 'turn/start'][-1]['clientUserMessageId'], 'after-repair')

    def _assert_action_wait(self, action):
        a = self.lead(self.other_key)
        wrapper, attempts = self.lower_during_prepare()
        with wrapper:
            result = self.runtime.native_action(a['id'], action, request_id=action + '-exact', context={})
            self.assertEqual(result['outcome']['status'], 'pending')
            self.assertTrue(self.runtime.agent(a['id']).get('budgetActionWait'))
            method = 'review/start' if action == 'review' else 'thread/compact/start'
            previous = self.count(method)
            self.runtime.configure(a['id'], {'tokenBudget': 1000})
            f.eventually(lambda: self.count(method) == previous + 1)
            f.eventually(lambda: self.runtime.native_action(a['id'], action, request_id=action + '-exact', context={})['outcome']['status'] == 'acknowledged')
        current = self.runtime.agent(a['id'])
        self.assertEqual(current['startAttempt']['id'], attempts[0])
        retry = self.runtime.native_action(a['id'], action, request_id=action + '-exact', context={})
        self.assertEqual(retry['receipt']['attemptId'], attempts[0])
        self.assertTrue(retry['replayed'])
        self.runtime.dispatch()
        self.assertEqual(self.count(method), previous + 1)

    def test_review_keeps_nondefault_account_receipt(self):
        self._assert_action_wait('review')

    def test_compact_keeps_nondefault_account_receipt(self):
        self._assert_action_wait('compact')

    def test_dispatch_does_not_restore_a_superseded_wait(self):
        a = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            current.update(status='queued', inFlight=False, tokenBudget=None,
                           startAttempt={'id': 'newer', 'epoch': current['epoch'], 'submitted': False, 'events': []},
                           budgetActionWait={'attemptId': 'old', 'agentId': a['id'], 'epoch': current['epoch'],
                               'accountKey': current['accountKey'], 'threadId': current['threadId'],
                               'events': [], 'action': 'review', 'error': 'old budget denial'})
            self.runtime.put(db, 'agents', current)
            db.commit()
            self.runtime.dispatch()
        current = self.runtime.agent(a['id'])
        self.assertNotIn('budgetActionWait', current)
        self.assertEqual(current['lastBudgetWait']['status'], 'superseded')
        self.assertEqual(current['startAttempt']['id'], 'newer')
        self.assertEqual(self.count('turn/start'), 1)


if __name__ == '__main__':
    unittest.main()
