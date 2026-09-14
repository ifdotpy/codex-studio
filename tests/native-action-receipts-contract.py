#!/usr/bin/env python3
"""Lost native action replies cannot create another model operation."""
import copy
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('prepare-steer-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ReceiptsContract(fixture.PrepareSteerContract):
    def idle(self):
        a = self.lead()
        self.server.complete(a['threadId'], a['turnId'])
        return self.runtime.agent(a['id'])

    def finish(self, a):
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(a['id'], db)
            current.update(status='completed', inFlight=False, turnId=None, startAttempt=None)
            self.runtime.put(db, 'agents', current)

    def test_completed_action_replay_once_and_new_explicit_action_runs(self):
        a = self.idle()
        for action, method in [('review', 'review/start'), ('compact', 'thread/compact/start')]:
            self.server.hold.add(method)
            context = {name: a[name] for name in ('accountKey', 'threadId', 'epoch')}
            first = self.runtime.native_action(a['id'], action, action + '-operation', context)
            self.finish(a)
            second = self.runtime.native_action(a['id'], action, action + '-operation', context)
            self.assertEqual(first['receipt'], second['receipt'])
            self.assertTrue(second['replayed'])
            self.assertEqual(self.count(method), 1)
            self.runtime.native_action(a['id'], action, action + '-next-operation', context)
            self.assertEqual(self.count(method), 2)
            self.finish(a)

    def test_changed_action_agent_or_context_rejects_identity(self):
        a = self.idle()
        self.server.hold.add('review/start')
        self.runtime.native_action(a['id'], 'review', 'exact-id', {'epoch': a['epoch']})
        self.finish(a)
        other = self.runtime.create({'name': 'Other', 'cwd': str(self.root), 'prompt': 'Other'}, defer=True)
        for key, action, context in [(a['id'], 'compact', {'epoch': a['epoch']}),
                                      (other['id'], 'review', {'epoch': a['epoch']}),
                                      (a['id'], 'review', {'epoch': a['epoch'] + 1})]:
            with self.assertRaisesRegex(ValueError, 'different input'):
                self.runtime.native_action(key, action, 'exact-id', context)
        self.assertEqual(self.count('review/start'), 1)
        self.assertEqual(self.count('thread/compact/start'), 0)

    def test_stale_account_thread_epoch_never_submits(self):
        a = self.idle()
        for context in [{'epoch': a['epoch'] + 1}, {'threadId': 'other'}, {'accountKey': 'other'}]:
            with self.assertRaisesRegex(ValueError, 'conversation changed'):
                self.runtime.native_action(a['id'], 'review', json.dumps(context), context)
        self.assertEqual(self.count('review/start'), 0)

    def test_uncertain_reply_and_restart_keep_receipt(self):
        a = self.idle()
        self.server.hold.add('review/start')
        first = self.runtime.native_action(a['id'], 'review', 'restart-id')
        self.assertTrue(first['result']['pending'])
        retry = self.runtime.native_action(a['id'], 'review', 'restart-id')
        self.assertEqual(first['receipt'], retry['receipt'])
        self.assertEqual(self.count('review/start'), 1)
        self.runtime.close()
        self.runtime = fixture.Runtime(self.root, fixture.DelayedServer)
        retry = self.runtime.native_action(a['id'], 'review', 'restart-id')
        self.assertEqual(first['receipt'], retry['receipt'])
        self.assertFalse(any(method == 'review/start' for server in self.runtime.servers.values() for method, _ in server.calls))

    def test_late_ack_updates_only_exact_durable_operation(self):
        a = self.idle()
        self.server.hold.add('review/start')
        first = self.runtime.native_action(a['id'], 'review', 'late-id')
        self.assertEqual(first['outcome']['status'], 'unknown')
        entry = next(row for row in self.server.delayed if row['method'] == 'review/start')
        entry['future'].set_result({'turn': {'id': 'late-turn', 'status': 'inProgress'}})
        fixture.eventually(lambda: self.runtime.native_action(a['id'], 'review', 'late-id')['outcome']['status'] == 'acknowledged')
        self.assertEqual(self.count('review/start'), 1)
        self.assertEqual(first['receipt'], self.runtime.native_action(a['id'], 'review', 'late-id')['receipt'])

    def test_receipt_thread_cannot_change_before_native_submission(self):
        a = self.idle()
        original = self.runtime.run_native_action
        def moved(key, attempt):
            with self.runtime.lock, self.runtime.db() as db:
                changed = self.runtime.agent(key, db)
                changed['threadId'] = 'replacement-thread'
                self.runtime.put(db, 'agents', changed)
            return original(key, attempt)
        with patch.object(self.runtime, 'run_native_action', side_effect=moved):
            result = self.runtime.native_action(a['id'], 'review', 'moved-id')
        self.assertEqual(result['outcome']['status'], 'failed')
        self.assertIn('earlier account, thread or epoch', result['outcome']['error'])
        self.assertEqual(self.count('review/start'), 0)
        self.assertEqual(self.runtime.native_action(a['id'], 'review', 'moved-id')['outcome'], result['outcome'])

    def test_completed_repair_exception_requires_exact_source_and_attempt(self):
        from codex_native_action_receipts import assert_identity
        identity = {'accountKey': 'default', 'threadId': 'original', 'epoch': 3}
        attempt = {'id': 'reserved-action', 'actionRequestId': 'request', 'actionIdentity': identity}
        repaired = {'id': 'agent', 'accountKey': 'default', 'threadId': 'repaired', 'epoch': 3,
                    'startAttempt': {**copy.deepcopy(attempt), 'threadId': 'repaired'},
                    'contextRepair': {'id': 'repair', 'agent': 'agent', 'phase': 'completed',
                        'newThreadId': 'repaired', 'source': {**identity, 'id': 'agent', 'attemptId': attempt['id']}}}
        assert_identity(repaired, attempt)
        self.assertEqual(attempt['actionIdentity']['threadId'], 'original')
        for path, value in [(('contextRepair', 'phase'), 'unknown'),
                            (('contextRepair', 'agent'), 'other-agent'),
                            (('contextRepair', 'source', 'id'), 'other-agent'),
                            (('contextRepair', 'source', 'attemptId'), 'other-attempt'),
                            (('contextRepair', 'source', 'epoch'), 2),
                            (('contextRepair', 'source', 'accountKey'), 'other-account'),
                            (('contextRepair', 'source', 'threadId'), 'other-source'),
                            (('contextRepair', 'newThreadId'), 'other-result'),
                            (('startAttempt', 'id'), 'other-attempt'),
                            (('startAttempt', 'threadId'), 'other-result'),
                            (('startAttempt', 'actionRequestId'), 'other-request'),
                            (('epoch',), 4), (('accountKey',), 'other-account')]:
            with self.subTest(path=path):
                changed = copy.deepcopy(repaired)
                target = changed
                for field in path[:-1]:
                    target = target[field]
                target[path[-1]] = value
                with self.assertRaisesRegex(ValueError, 'earlier account, thread or epoch'):
                    assert_identity(changed, attempt)

    def test_repair_return_cannot_replace_reserved_action_attempt(self):
        a = self.idle()
        self.server.hold.add('review/start')
        def replaced(runtime, agent):
            with runtime.lock, runtime.db() as db:
                current = runtime.agent(agent['id'], db)
                current['startAttempt']['id'] = 'other-reserved-attempt'
                runtime.put(db, 'agents', current)
                return current
        with patch('codex_context_repair.repair_before_start', side_effect=replaced):
            result = self.runtime.native_action(a['id'], 'review', 'replaced-attempt-id')
        self.assertEqual(self.count('review/start'), 0, result)
        self.assertEqual(result['outcome']['status'], 'failed')

    def test_failure_before_submission_remains_visible_on_retry(self):
        a = self.idle()
        with patch.object(self.runtime, 'prepare', side_effect=ValueError('settings failed')):
            first = self.runtime.native_action(a['id'], 'review', 'failed-id')
        retry = self.runtime.native_action(a['id'], 'review', 'failed-id')
        self.assertEqual(first['outcome'], retry['outcome'])
        self.assertEqual(retry['outcome'], {'status': 'failed', 'error': 'settings failed'})
        self.assertEqual(self.count('review/start'), 0)

    def test_settings_ack_keeps_same_operation(self):
        a = self.idle()
        self.server.hold.update({'thread/settings/update', 'review/start'})
        first = self.runtime.native_action(a['id'], 'review', 'settings-id')
        retry = self.runtime.native_action(a['id'], 'review', 'settings-id')
        self.assertEqual(first['receipt'], retry['receipt'])
        self.assertEqual(self.count('review/start'), 0)
        entry = next(row for row in self.server.delayed if row['method'] == 'thread/settings/update')
        entry['future'].set_result({})
        fixture.eventually(lambda: self.count('review/start') == 1)
        self.runtime.native_action(a['id'], 'review', 'settings-id')
        self.assertEqual(self.count('review/start'), 1)


if __name__ == '__main__':
    suite = unittest.TestSuite(ReceiptsContract(name) for name in ReceiptsContract.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
