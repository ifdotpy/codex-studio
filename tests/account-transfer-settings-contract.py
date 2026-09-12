#!/usr/bin/env python3
"""Account transfers resolve target settings without overwriting newer choices."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('transfer_fixture', Path(__file__).with_name('account-transfer-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class AccountTransferSettingsContract(unittest.TestCase):
    def setUp(self):
        self.t = fixture.TransferContract('test_transfer_keeps_identity_history_queue_and_settings')
        self.t.setUp()
        self.runtime = self.t.runtime
        self.key = self.t.lead_agent['id']
        self.catalog = self.runtime.catalog
        self.target_default = 'low'
        self.target_supported = ['low', 'high']
        self.target_models = None
        self.runtime.catalog = self.destination_catalog

    def tearDown(self):
        self.t.tearDown()

    def destination_catalog(self, account='default'):
        result = copy.deepcopy(self.catalog(account))
        if account == self.t.other_key:
            result['data'] = [row for row in result['data'] if self.target_models is None or row['model'] in self.target_models]
            for row in result['data']:
                row['defaultReasoningEffort'] = self.target_default
                row['supportedReasoningEfforts'] = [{'reasoningEffort': effort} for effort in self.target_supported]
        return result

    def agent(self):
        return self.runtime.agent(self.key)

    def pending(self, model='gpt-5.6-sol', effort=None, identity='pending-choice'):
        return self.runtime.conversation_settings(self.key, {'next_turn': True, 'request_id': identity,
                                                            'model': model, 'effort': effort})

    def start(self):
        op = self.t.start_transfer()
        self.t.tick()
        return op

    def member(self, op):
        return self.t.receipt(op['id'])['members'][self.key]

    def test_null_effort_resolves_for_destination_and_retains_user_preference(self):
        self.t.set_agent(self.key, effort=None, nativeEffort='high')
        self.target_supported = ['low']
        before = self.agent()
        op = self.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        params = self.t.pending[0][1]
        self.assertEqual(params['config']['model_reasoning_effort'], 'low')
        self.assertEqual(self.agent()['nativeEffort'], 'high', 'Do not change the source before the receipt')
        self.t.complete_fork()
        after = self.agent()
        self.assertIsNone(after['effort'])
        self.assertEqual(after['nativeEffort'], 'low')
        self.assertEqual(after['accountKey'], self.t.other_key)
        for field in ('model', 'fastMode', 'yoloMode', 'role', 'autoWake', 'epoch'):
            self.assertEqual(after[field], before[field], field)
        self.assertEqual(self.member(op)['phase'], 'completed')

    def test_queued_choice_moves_to_destination_and_next_turn_uses_it(self):
        self.t.set_agent(self.key, effort=None, nativeEffort='high')
        self.pending()
        self.target_supported = ['low']
        op = self.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        a = self.agent()
        self.assertEqual(a['pendingSettingsAccountKey'], self.t.other_key)
        self.assertEqual(a['pendingSettings']['model'], 'gpt-5.6-sol')
        self.assertIsNone(a['pendingSettings']['effort'])
        self.assertEqual(a['pendingSettings']['nativeEffort'], 'low')
        self.runtime.send(self.key, 'Continue on destination', 'destination-message')
        self.runtime.dispatch()
        self.t.until(lambda: self.agent().get('turnId'))
        starts = [params for method, params in self.t.target_server.calls if method == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual((starts[0]['model'], starts[0]['effort']), ('gpt-5.6-sol', 'low'))
        self.assertNotIn('pendingSettings', self.agent())
        self.assertEqual(self.member(op)['phase'], 'completed')

    def test_unsupported_active_or_queued_choice_blocks_before_native_mutation(self):
        self.t.set_agent(self.key, effort='high', nativeEffort='high')
        self.target_supported = ['low']
        op = self.start()
        self.t.until(lambda: self.member(op)['phase'] == 'blocked')
        self.assertEqual(self.t.pending, [])
        self.assertFalse(any(method == 'thread/unsubscribe' for method, _ in self.t.native_calls))
        self.assertEqual(self.agent()['accountKey'], 'default')
        self.runtime.conversation_settings(self.key, {'effort': None})
        self.pending(model='gpt-5.6-sol')
        self.target_models = {'gpt-6-astra'}
        self.t.store.action(op['id'], 'retry')
        self.t.tick()
        self.t.until(lambda: self.member(op)['phase'] == 'blocked')
        self.assertIn('not available', self.member(op)['error'])
        self.assertEqual(self.t.pending, [])
        self.assertEqual(self.agent()['pendingSettings']['model'], 'gpt-5.6-sol')

    def test_settings_change_during_catalog_validation_blocks_without_overwrite(self):
        self.t.set_agent(self.key, effort=None, nativeEffort='high')
        original = self.runtime.catalog
        changed = False
        def catalog(account='default'):
            nonlocal changed
            result = original(account)
            if account == self.t.other_key and not changed:
                changed = True
                self.pending(identity='during-validation')
            return result
        self.runtime.catalog = catalog
        op = self.start()
        self.t.until(lambda: self.member(op)['phase'] == 'blocked')
        self.assertIn('newer choice is preserved', self.member(op)['error'])
        self.assertEqual(self.agent()['pendingSettings']['model'], 'gpt-5.6-sol')
        self.assertEqual(self.t.pending, [])
        self.assertFalse(any(method == 'thread/unsubscribe' for method, _ in self.t.native_calls))

    def test_late_fork_receipt_keeps_newer_choice_then_retry_reuses_same_fork(self):
        self.t.set_agent(self.key, effort=None, nativeEffort='high')
        self.pending(model='gpt-6-astra', identity='before-fork')
        op = self.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.pending(model='gpt-5.6-sol', identity='after-submit')
        self.t.complete_fork()
        self.assertEqual(self.member(op)['phase'], 'blocked')
        self.assertEqual(self.agent()['accountKey'], 'default')
        self.assertEqual(self.agent()['pendingSettings']['model'], 'gpt-5.6-sol')
        self.assertEqual(self.member(op)['result']['thread']['id'], 'target-thread-0')
        self.t.store.action(op['id'], 'retry')
        self.t.tick()
        self.t.until(lambda: self.member(op)['phase'] == 'completed')
        self.assertEqual(len(self.t.pending), 1, 'A confirmed fork must not be repeated')
        self.assertEqual(self.agent()['threadId'], 'target-thread-0')
        self.assertEqual(self.agent()['pendingSettings']['model'], 'gpt-5.6-sol')
        self.assertEqual(self.agent()['pendingSettingsAccountKey'], self.t.other_key)
        self.assertEqual(self.agent()['pendingSettings']['nativeEffort'], 'low')

    def test_retry_revalidates_saved_legacy_receipt_without_new_native_mutation(self):
        self.t.set_agent(self.key, effort=None, nativeEffort='high')
        op = self.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        with self.runtime.lock, self.runtime.db() as db:
            stored = self.t.store.get(db, op['id'])
            stored['members'][self.key].pop('targetSettings')
            self.t.store.save(db, stored)
        self.t.complete_fork()
        self.assertEqual(self.member(op)['phase'], 'blocked')
        self.t.store.action(op['id'], 'retry')
        self.t.tick()
        self.t.until(lambda: self.member(op)['phase'] == 'completed')
        self.assertEqual(len(self.t.pending), 1)
        self.assertEqual(self.agent()['nativeEffort'], 'low')

    def test_restart_during_saved_receipt_validation_never_repeats_fork(self):
        self.t.set_agent(self.key, effort=None, nativeEffort='high')
        op = self.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.pending(identity='changed-during-fork')
        self.t.complete_fork()
        self.assertEqual(self.member(op)['phase'], 'blocked')
        with self.runtime.lock, self.runtime.db() as db:
            stored = self.t.store.get(db, op['id'])
            stored['members'][self.key].update(phase='reading', nextCheck=0)
            self.t.store.save(db, stored)
        from codex_account_transfer import AccountTransfers
        self.t.store = AccountTransfers(self.runtime)
        self.runtime._account_transfers = self.t.store
        self.assertEqual(self.member(op)['phase'], 'waiting')
        self.t.tick()
        self.t.until(lambda: self.member(op)['phase'] == 'completed')
        self.assertEqual(len(self.t.pending), 1)
        self.assertEqual(self.agent()['pendingSettingsAccountKey'], self.t.other_key)


if __name__ == '__main__':
    unittest.main()
