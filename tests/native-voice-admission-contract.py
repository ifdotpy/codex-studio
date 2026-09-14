#!/usr/bin/env python3
"""Paid voice admission uses the current budget and exact native identity."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('voice_fixture', Path(__file__).with_name('native-voice-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class NativeVoiceAdmission(unittest.TestCase):
    setUp = fixture.NativeVoiceContract.setUp
    tearDown = fixture.NativeVoiceContract.tearDown
    until = fixture.NativeVoiceContract.until
    start = fixture.NativeVoiceContract.start

    def failed(self):
        self.until(lambda: self.voice.session('lead', 'session')['state'] == 'failed')
        return self.voice.session('lead', 'session')

    def test_exhausted_budget_preserves_receipt_without_native_request_or_wake(self):
        self.runtime.actors['lead'].update(tokenBudget=10, autoWake=False)
        status = {'reached': True, 'tokenBudget': 10, 'incomplete': []}
        with patch('codex_budget.budget_status', return_value=status):
            self.voice.start('lead', 'session', 'v=0\noffer')
            result = self.failed()
        self.assertIn('budget reached', result['error'])
        self.assertEqual(self.server.submissions, [])
        self.assertFalse(self.runtime.actors['lead']['autoWake'])
        self.runtime.actors['lead']['tokenBudget'] = None
        self.assertEqual(self.voice.start('lead', 'session', 'v=0\noffer'), result)
        self.assertEqual(self.server.submissions, [])

    def test_unknown_accounting_denies_voice(self):
        self.runtime.actors['lead']['tokenBudget'] = 10
        status = {'reached': False, 'tokenBudget': 10, 'incomplete': [{'reason': 'history not imported'}]}
        with patch('codex_budget.budget_status', return_value=status):
            self.voice.start('lead', 'session', 'v=0\noffer')
            self.assertIn('cannot be verified', self.failed()['error'])
        self.assertEqual(self.server.submissions, [])

    def test_exact_success_retry_does_not_recheck_or_resubmit(self):
        operation = self.start()
        operation[2].set_result({})
        with patch('codex_budget.budget_admission', side_effect=AssertionError('A receipt is not a new request')):
            self.voice.start('lead', 'session', 'v=0\noffer')
            self.voice._submit_start('session', 'v=0\noffer')
        self.assertEqual(len(self.server.submissions), 1)

    def test_context_repair_blocks_new_voice_before_reservation(self):
        for phase in ('preparing', 'submitted', 'unknown', 'ready'):
            with self.subTest(phase=phase):
                self.runtime.actors['lead']['contextRepair'] = {'id': 'repair', 'phase': phase}
                with self.assertRaisesRegex(ValueError, 'Context repair'):
                    self.voice.start('lead', 'session', 'v=0\noffer')
        self.assertEqual(self.server.submissions, [])
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM voice_sessions').fetchone()[0], 0)

    def deny_between_prepare_and_submit(self, mutate):
        original = self.voice._submit_start
        def submit(*args, **kwargs):
            mutate()
            return original(*args, **kwargs)
        self.runtime.actors['lead']['autoWake'] = False
        with patch.object(self.voice, '_submit_start', side_effect=submit):
            self.voice.start('lead', 'session', 'v=0\noffer')
            self.failed()
        self.assertEqual(self.server.submissions, [])
        self.assertFalse(self.runtime.actors['lead']['autoWake'])

    def test_epoch_change_before_submission(self):
        self.deny_between_prepare_and_submit(lambda: self.runtime.actors['lead'].update(epoch=3))

    def test_account_change_before_submission(self):
        self.deny_between_prepare_and_submit(lambda: self.runtime.actors['lead'].update(accountKey='other'))

    def test_thread_change_before_submission(self):
        self.deny_between_prepare_and_submit(lambda: self.runtime.actors['lead'].update(threadId='other-thread'))

    def test_connection_change_before_submission(self):
        self.deny_between_prepare_and_submit(lambda: self.runtime.connection_ids.update(chosen='new-process'))

    def test_repair_change_before_submission(self):
        self.deny_between_prepare_and_submit(lambda: self.runtime.actors['lead'].update(contextRepair={'id': 'repair', 'phase': 'unknown'}))

    def test_thread_change_during_prepare_is_not_silently_adopted(self):
        def prepare(actor):
            self.runtime.actors['lead']['threadId'] = 'replacement-thread'
        with patch.object(self.runtime, 'prepare', side_effect=prepare):
            self.voice.start('lead', 'session', 'v=0\noffer')
            self.failed()
        self.assertEqual(self.server.submissions, [])

    def legacy_queued_start(self):
        with patch('codex_native_voice.threading.Thread.start'):
            self.voice.start('lead', 'session', 'v=0\noffer')
        context = self.voice.connections['session']
        context.pop('account')
        context.pop('initial_thread')

    def test_legacy_queued_start_survives_live_patch_once(self):
        self.legacy_queued_start()
        self.voice._start_native('lead', 'session', 'v=0\noffer')
        self.assertEqual(len(self.server.submissions), 1)
        self.assertEqual(self.server.submissions[0][0], 'thread/realtime/start')
        self.assertEqual(self.voice.connections['session']['account'], 'chosen')
        self.voice.start('lead', 'session', 'v=0\noffer')
        self.assertEqual(len(self.server.submissions), 1)
        self.assertEqual([item[0] for item in self.server.calls].count('account/read'), 1)

    def test_legacy_queued_start_preserves_epoch_guard(self):
        self.legacy_queued_start()
        self.runtime.actors['lead'].update(epoch=3, autoWake=False)
        self.voice._start_native('lead', 'session', 'v=0\noffer')
        self.assertEqual(self.voice.session('lead', 'session')['state'], 'failed')
        self.assertFalse(self.runtime.actors['lead']['autoWake'])
        self.assertEqual(self.server.submissions, [])

    def test_budget_change_before_feature_reload_blocks_retry(self):
        operation = self.start()
        with patch('codex_budget.budget_admission', side_effect=ValueError('Team token budget reached')):
            operation[2].set_exception(ValueError('thread does not support realtime conversation'))
            self.failed()
        self.assertEqual(len(self.server.submissions), 1)
        self.assertNotIn('thread/unsubscribe', [item[0] for item in self.server.calls])

    def test_thread_change_during_feature_reload_blocks_retry(self):
        operation = self.start()
        def prepare(actor):
            self.runtime.actors['lead']['threadId'] = 'replacement-thread'
        with patch.object(self.runtime, 'prepare_locked', side_effect=prepare):
            operation[2].set_exception(ValueError('thread does not support realtime conversation'))
            self.failed()
        self.assertEqual(len(self.server.submissions), 1)

    def test_budget_change_during_feature_reload_blocks_retry(self):
        operation = self.start()
        def prepare(actor):
            self.runtime.actors['lead']['tokenBudget'] = 10
        status = {'reached': True, 'tokenBudget': 10, 'incomplete': []}
        with patch.object(self.runtime, 'prepare_locked', side_effect=prepare), patch('codex_budget.budget_status', return_value=status):
            operation[2].set_exception(ValueError('thread does not support realtime conversation'))
            self.failed()
        self.assertEqual(len(self.server.submissions), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
