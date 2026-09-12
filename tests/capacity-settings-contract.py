#!/usr/bin/env python3
"""A verified capacity continuation consumes exactly one queued settings choice."""
import copy
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('capacity_fixture', Path(__file__).with_name('capacity-retry-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class CapacitySettingsContract(unittest.TestCase):
    def setUp(self):
        self.t = fixture.CapacityContract('test_four_delays_then_manual_only_without_input_replay')
        self.t.setUp()
        self.runtime = self.t.runtime
        self.key = self.t.key

    def tearDown(self):
        self.t.tearDown()

    def select(self, model='gpt-5.6-sol', request_id='select-sol'):
        return self.runtime.conversation_settings(self.key, {'next_turn': True, 'request_id': request_id,
            'model': model, 'effort': 'high', 'fast_mode': True})

    def assert_selection(self, retry):
        fixture.fixture.eventually(lambda: len(self.t.starts()) == 2 and self.t.agent().get('turnId'))
        current = self.t.agent()
        params = self.t.starts()[-1]
        self.assertEqual(params['model'], 'gpt-5.6-sol')
        self.assertEqual(params['effort'], 'high')
        self.assertEqual(params['serviceTier'], 'priority')
        self.assertEqual(params['input'], [])
        self.assertNotIn('clientUserMessageId', params)
        self.assertEqual(params['threadId'], retry['threadId'])
        for field in ('id', 'threadId', 'turnId', 'epoch', 'accountKey'):
            self.assertEqual(current['capacityRetry'][field], retry[field], field)
        self.assertEqual(current['capacityRetry']['settings'], self.runtime.preparation_settings(current))
        for field in ('approvalPolicy', 'sandboxPolicy'):
            self.assertEqual(params[field], self.t.starts()[0][field])

    def test_manual_retry_consumes_queued_choice_and_duplicate_does_not_reapply(self):
        self.select()
        retry = self.t.fail()
        self.t.retry(retry)
        self.assert_selection(retry)
        self.assertNotIn('pendingSettings', self.t.agent())
        self.assertNotIn('pendingSettingsAccountKey', self.t.agent())
        self.select('gpt-6-astra', 'next-choice')
        self.t.retry(retry)
        self.assertEqual(len(self.t.starts()), 2)
        self.assertEqual(self.t.agent()['pendingSettings']['model'], 'gpt-6-astra')

    def test_automatic_retry_also_consumes_the_queued_choice(self):
        self.select()
        retry = self.t.fail()
        self.t.expire()
        self.assert_selection(retry)
        self.assertNotIn('pendingSettings', self.t.agent())

    def test_wrong_account_refuses_without_consuming_or_claiming(self):
        self.select()
        retry = self.t.fail()
        self.t.mutate(pendingSettingsAccountKey='other-account')
        before = copy.deepcopy(self.t.agent())
        with self.assertRaisesRegex(ValueError, 'account changed'):
            self.t.retry(retry)
        self.assertEqual(self.t.agent(), before)
        self.assertEqual(len(self.t.starts()), 1)
        self.assertNotIn('claimedAt', self.t.agent()['capacityRetry'])

    def test_original_failed_turn_guard_runs_before_consuming_new_choice(self):
        self.select()
        retry = self.t.fail()
        self.t.mutate(lastCompletedTurn='different-failed-turn')
        before = copy.deepcopy(self.t.agent())
        with self.assertRaisesRegex(ValueError, 'earlier agent state'):
            self.t.retry(retry)
        self.assertEqual(self.t.agent(), before)
        self.assertEqual(len(self.t.starts()), 1)

    def test_later_queued_choice_does_not_change_claimed_continuation(self):
        self.select()
        retry = self.t.fail()
        with patch.object(self.runtime.pool, 'submit') as submit:
            self.t.retry(retry)
        function, key, attempt = submit.call_args.args
        self.select('gpt-6-astra', 'queued-after-claim')
        function(key, attempt)
        self.assert_selection(retry)
        self.assertEqual(self.t.agent()['pendingSettings']['model'], 'gpt-6-astra')

    def test_active_settings_change_after_claim_prevents_native_submission(self):
        self.select()
        retry = self.t.fail()
        with patch.object(self.runtime.pool, 'submit') as submit:
            self.t.retry(retry)
        function, key, attempt = submit.call_args.args
        self.t.mutate(model='gpt-6-astra')
        function(key, attempt)
        self.assertEqual(len(self.t.starts()), 1)
        self.assertEqual(self.t.agent()['capacityRetry']['status'], 'failed')
        self.assertIn('earlier agent state', self.t.agent()['capacityRetry']['reason'])


if __name__ == '__main__':
    unittest.main()
