#!/usr/bin/env python3
"""Portable transfer state machine, with native servers and archive I/O isolated."""
import copy
import concurrent.futures
import importlib.util
from pathlib import Path
import sys
import types
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('transfer_fixture', Path(__file__).with_name('account-transfer-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class PortableTransfers(unittest.TestCase):
    def setUp(self):
        self.t = f.TransferContract('test_transfer_keeps_identity_history_queue_and_settings')
        self.t.setUp()
        self.rt, self.store = self.t.runtime, self.t.store
        self.aid = self.t.lead_agent['id']
        self.providers = {'default': 'codex', self.t.other_key: 'claude'}
        original_get = self.rt.accounts.get
        self.rt.accounts.get = lambda key: {**original_get(key), 'provider': self.providers.get(key, 'codex')}
        original_catalog = self.rt.catalog
        self.rt.catalog = lambda key='default': self.claude_catalog() if self.providers.get(key) == 'claude' else original_catalog(key)
        self.descriptor = {'version': 1, 'path': '/fixture/history.json', 'sha256': 'a' * 64}
        self.export = Mock(return_value=self.descriptor)
        module = types.ModuleType('codex_portable_history')
        module.export_history = self.export
        module.history_context = lambda *args: '\nComplete portable history: /fixture/history.json\n'
        self.modules = patch.dict(sys.modules, {'codex_portable_history': module})
        self.modules.start()
        original_call = self.t.source_server.call
        def call(method, params, timeout=60):
            result = original_call(method, params, timeout)
            if method == 'thread/read' and self.providers['default'] == 'claude':
                result['thread'].pop('path', None)
            return result
        self.t.source_server.call = call

    def tearDown(self):
        self.t.tearDown()
        self.modules.stop()

    @staticmethod
    def claude_catalog():
        return {'data': [{'model': name, 'isDefault': name == 'default',
                         'defaultReasoningEffort': 'medium',
                         'supportedReasoningEfforts': [{'reasoningEffort': 'medium'}, {'reasoningEffort': 'high'}],
                         'serviceTiers': []} for name in ('default', 'sonnet')]}

    def agent(self):
        return self.rt.agent(self.aid)

    def member(self, op):
        return self.t.receipt(op['id'])['members'][self.aid]

    def submit(self):
        op = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(len(self.t.pending), 1, self.member(op))
        self.assertEqual(self.t.pending[0][0], 'thread/start')
        self.assertNotIn('input', self.t.pending[0][1])
        self.assertNotIn('threadId', self.t.pending[0][1])
        return op

    def finish(self, op):
        self.t.complete_fork()
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')
        self.assertFalse(any(method in {'turn/start', 'turn/steer'} for method, _ in self.t.native_calls))
        self.assertEqual(self.agent()['portableHistory'], self.descriptor)
        self.assertEqual(self.member(op)['nativeParams'], self.t.pending[0][1])

    def test_codex_to_claude_preserves_chat_items_source_and_workers(self):
        child = self.rt.create({'name': 'Worker', 'prompt': 'Task'}, parent=self.aid, defer=True)
        child_before = self.rt.agent(child['id'])
        self.t.set_agent(self.aid, effort='ultra', fastMode=True,
                         pendingSettings={'model': 'gpt-6-astra', 'effort': 'ultra', 'fastMode': True},
                         pendingSettingsAccountKey='default')
        with self.rt.db() as db:
            self.rt.item(db, self.aid, 'user-text', 'user', 'Keep this message')
            before = list(db.execute('SELECT * FROM runtime_items WHERE agent=?', (self.aid,)))
        source = self.agent()
        op = self.submit()
        self.finish(op)
        agent = self.agent()
        self.assertEqual(agent['id'], source['id'])
        self.assertEqual(agent['epoch'], source['epoch'])
        self.assertEqual(agent['model'], 'default')
        self.assertIsNone(agent['effort'])
        self.assertEqual(agent['nativeEffort'], 'medium')
        self.assertFalse(agent['fastMode'])
        self.assertEqual(agent['provider'], 'claude')
        self.assertEqual(agent['workerDefaults']['model'], 'sonnet')
        self.assertNotIn('pendingSettings', agent)
        self.assertEqual(agent['accountHistory'][-1]['threadId'], 'native-source')
        self.assertEqual(agent['accountHistory'][-1]['settingsDiscarded']['reason'], 'provider_changed')
        self.assertEqual(self.rt.agent(child['id']), child_before)
        self.assertEqual(set(op['members']), {self.aid})
        with self.rt.db() as db:
            self.assertEqual(list(db.execute('SELECT * FROM runtime_items WHERE agent=?', (self.aid,))), before)
        self.assertEqual(self.member(op)['pendingSettings'], source['pendingSettings'])
        self.export.assert_called_once()

    def test_claude_to_codex_needs_no_codex_rollout_path(self):
        self.providers.update(default='claude', **{self.t.other_key: 'codex'})
        self.t.set_agent(self.aid, provider='claude', model='sonnet', effort='high')
        op = self.submit()
        self.finish(op)
        self.assertEqual(self.agent()['provider'], 'codex')
        self.assertEqual(self.agent()['model'], 'gpt-6-astra')
        self.assertEqual(self.agent()['accountHistory'][-1]['provider'], 'claude')
        self.t.copy_patch.target.copy_history.assert_not_called()

    def test_claude_profiles_preserve_supported_model_and_pending_choice(self):
        self.providers['default'] = 'claude'
        self.t.set_agent(self.aid, provider='claude', model='sonnet', effort='high',
                         pendingSettings={'model': 'default', 'effort': 'medium', 'fastMode': False},
                         pendingSettingsAccountKey='default')
        op = self.submit()
        self.finish(op)
        self.assertEqual(self.agent()['model'], 'sonnet')
        self.assertEqual(self.agent()['pendingSettings']['model'], 'default')
        self.assertEqual(self.agent()['pendingSettingsAccountKey'], self.t.other_key)

    def test_same_provider_profile_remaps_unavailable_future_worker_model(self):
        self.providers['default'] = 'claude'
        self.t.set_agent(self.aid, provider='claude', model='sonnet', effort='high',
                         workerDefaults={'model': 'old-profile-model', 'effort': 'ultra', 'fastMode': True},
                         claudeOptions={'permissionMode': 'plan'})
        op = self.submit()
        self.finish(op)
        self.assertEqual(self.agent()['workerDefaults'], {'model': 'sonnet', 'effort': None, 'fastMode': False})
        self.assertEqual(self.agent()['claudeOptions'], {'permissionMode': 'plan'})

    def test_same_provider_profile_preserves_inherited_worker_model(self):
        self.providers['default'] = 'claude'
        self.t.set_agent(self.aid, provider='claude', model='sonnet', effort='high',
                         workerDefaults={'model': None, 'effort': 'high', 'fastMode': False})
        op = self.submit()
        self.finish(op)
        self.assertEqual(self.agent()['workerDefaults'], {'model': None, 'effort': 'high', 'fastMode': False})

    def test_provider_roundtrip_clears_old_permission_bypass(self):
        self.providers.update(default='claude', **{self.t.other_key: 'codex'})
        self.t.set_agent(self.aid, provider='claude', model='sonnet', effort='high', yoloMode=True,
                         claudeOptions={'permissionMode': 'bypassPermissions', 'fastMode': True})
        first = self.submit()
        self.finish(first)
        self.assertNotIn('claudeOptions', self.agent())
        self.assertEqual(self.agent()['accountHistory'][-1]['providerOptionsDiscarded']['claudeOptions']['permissionMode'], 'bypassPermissions')
        self.t.set_agent(self.aid, yoloMode=False)
        source_call = self.t.target_server.call
        def call(method, params, timeout=60):
            if method == 'thread/read':
                return {'thread': {'id': params['threadId'], 'status': {'type': 'idle'}}}
            if method in {'thread/queue/list', 'thread/backgroundTerminals/list'}:
                return {'data': []}
            if method == 'thread/unsubscribe':
                return {}
            return source_call(method, params, timeout)
        self.t.target_server.call = call
        self.t.target_server.after_events = lambda cb: cb()
        target_submit = self.t.source_server.submit
        def submit(method, params):
            if method != 'thread/start':
                return target_submit(method, params)
            future = concurrent.futures.Future()
            self.t.pending.append((method, params, future))
            return future
        self.t.source_server.submit = submit
        second = self.store.request(self.aid, 'default', str(f.uuid.uuid4()))
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(len(self.t.pending), 2, self.member(second))
        params = self.t.pending[1][1]
        self.assertEqual(params['approvalPolicy'], 'on-request')
        self.assertEqual(params['sandbox'], 'workspace-write')
        self.assertEqual(params['claude'], {})
        self.t.complete_fork(1)
        self.assertEqual(self.member(second)['phase'], 'completed')
        self.assertEqual(self.agent()['provider'], 'claude')
        self.assertNotIn('claudeOptions', self.agent())

    def test_real_archive_connects_full_history_to_destination_context(self):
        archive_spec = importlib.util.spec_from_file_location('real_portable_archive',
            Path(__file__).resolve().parents[1] / 'scripts' / 'codex_portable_history.py')
        archive = importlib.util.module_from_spec(archive_spec)
        archive_spec.loader.exec_module(archive)
        module = sys.modules['codex_portable_history']
        module.export_history = archive.export_history
        module.history_context = archive.history_context
        full_text = 'Full assistant history. ' * 1500
        original = self.t.source_server.call
        def call(method, params, timeout=60):
            if method == 'thread/turns/list':
                return {'data': [{'id': 'turn-1', 'itemsView': 'full', 'items': [
                    {'id': 'native-message', 'type': 'agentMessage', 'text': full_text}]}], 'nextCursor': None}
            return original(method, params, timeout)
        self.t.source_server.call = call
        with self.rt.db() as db:
            self.rt.item(db, self.aid, 'owner-request', 'user', 'Keep the source task')
        op = self.submit()
        self.descriptor = self.member(op)['portableHistory']
        content = Path(self.descriptor['path']).read_text()
        self.assertIn(full_text, content)
        self.assertIn('Keep the source task', content)
        context = self.t.pending[0][1]['developerInstructions']
        self.assertIn(self.descriptor['path'], context)
        self.assertIn('Keep the source task', context)
        self.assertNotIn(full_text, context)
        self.finish(op)

    def test_local_history_exports_without_native_source_thread(self):
        self.t.set_agent(self.aid, threadId=None)
        op = self.submit()
        self.finish(op)
        self.assertIsNone(self.export.call_args.args[3])
        self.assertFalse(any(method == 'thread/read' for method, _ in self.t.native_calls))

    def test_lost_native_receipt_and_retry_never_create_second_session(self):
        op = self.submit()
        self.t.pending[0][2].set_exception(RuntimeError('Native response lost'))
        self.store.action(op['id'], 'retry')
        for _ in range(3):
            self.t.tick()
        self.assertEqual(self.member(op)['phase'], 'unknown')
        self.assertEqual(len(self.t.pending), 1)
        self.assertEqual(self.agent()['accountKey'], 'default')
        self.assertEqual(self.member(op)['portableHistory'], self.descriptor)

    def test_restart_marks_submitted_start_unknown_without_repeating_it(self):
        from codex_account_transfer import AccountTransfers
        op = self.submit()
        restarted = AccountTransfers(self.rt)
        restarted.action(op['id'], 'retry')
        with self.rt.db() as db:
            restarted.tick(self.rt.records(db, 'agents'))
        self.assertEqual(self.member(op)['phase'], 'unknown')
        self.assertEqual(len(self.t.pending), 1)
        self.assertEqual(self.member(op)['portableHistory'], self.descriptor)
        self.assertEqual(self.member(op)['nativeParams'], self.t.pending[0][1])
        restarted.close()

    def test_settings_race_reuses_receipt_archive_and_native_parameters(self):
        op = self.submit()
        submitted = copy.deepcopy(self.member(op)['nativeParams'])
        self.t.set_agent(self.aid, effort='high', workerDefaults={'model': 'gpt-5.6-sol', 'effort': 'high', 'fastMode': False})
        self.t.complete_fork()
        self.assertEqual(self.member(op)['phase'], 'blocked')
        self.assertEqual(self.agent()['accountKey'], 'default')
        self.store.action(op['id'], 'retry')
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(self.member(op)['phase'], 'completed')
        self.assertEqual(len(self.t.pending), 1)
        self.assertEqual(self.member(op)['nativeParams'], submitted)
        self.export.assert_called_once()
        self.assertEqual(self.agent()['portableHistory'], self.descriptor)
        self.assertEqual(self.agent()['effort'], 'high')

    def test_settings_race_before_submission_preserves_newer_choice(self):
        self.export.side_effect = lambda *args: (self.t.set_agent(self.aid, effort='high') and self.descriptor)
        op = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(self.member(op)['phase'], 'blocked')
        self.assertEqual(self.t.pending, [])
        self.assertEqual(self.agent()['effort'], 'high')

    def test_native_input_arriving_during_export_blocks_submission(self):
        original = self.t.source_server.call
        exported = False
        def export(*args):
            nonlocal exported
            exported = True
            return self.descriptor
        self.export.side_effect = export
        def call(method, params, timeout=60):
            if method == 'thread/queue/list' and exported:
                return {'data': [{'id': 'late-input'}]}
            return original(method, params, timeout)
        self.t.source_server.call = call
        op = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(self.member(op)['phase'], 'blocked')
        self.assertTrue(self.member(op)['archiveInvalidated'])
        with self.assertRaisesRegex(ValueError, 'Cancel this transfer'):
            self.store.action(op['id'], 'retry')
        self.assertEqual(self.t.pending, [])

    def test_native_history_change_during_export_invalidates_archive(self):
        original = self.t.source_server.call
        revision = 1
        def call(method, params, timeout=60):
            result = original(method, params, timeout)
            if method == 'thread/read':
                result['thread']['updatedAt'] = revision
            return result
        def export(*args):
            nonlocal revision
            revision = 2
            return self.descriptor
        self.t.source_server.call = call
        self.export.side_effect = export
        op = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertTrue(self.member(op)['archiveInvalidated'])
        self.assertEqual(self.t.pending, [])
        with self.assertRaisesRegex(ValueError, 'Cancel this transfer'):
            self.store.action(op['id'], 'retry')

    def test_source_queue_after_native_start_does_not_adopt_stale_receipt(self):
        op = self.submit()
        original = self.t.source_server.call
        def call(method, params, timeout=60):
            if method == 'thread/queue/list':
                return {'data': [{'id': 'after-start'}]}
            return original(method, params, timeout)
        self.t.source_server.call = call
        self.t.complete_fork()
        self.assertTrue(self.member(op)['archiveInvalidated'])
        self.assertEqual(self.member(op)['result']['thread']['id'], 'target-thread-0')
        self.assertEqual(self.agent()['accountKey'], 'default')
        self.assertEqual(len(self.t.pending), 1)

    def test_source_check_timeout_retries_known_receipt_without_new_session(self):
        op = self.submit()
        original = self.t.source_server.call
        def call(method, params, timeout=60):
            if method == 'thread/read':
                raise TimeoutError('Source history read timed out')
            return original(method, params, timeout)
        self.t.source_server.call = call
        self.t.complete_fork()
        self.assertEqual(self.member(op)['phase'], 'blocked')
        self.assertEqual(self.agent()['accountKey'], 'default')
        self.t.source_server.call = original
        self.store.action(op['id'], 'retry')
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(self.member(op)['phase'], 'completed')
        self.assertEqual(len(self.t.pending), 1)
        self.export.assert_called_once()

    def test_history_version_change_invalidates_same_timestamp_export(self):
        original = self.t.source_server.call
        version = 'first-history'
        def call(method, params, timeout=60):
            result = original(method, params, timeout)
            if method == 'thread/read':
                result['thread'].update(updatedAt=123, historyVersion=version)
            return result
        def export(*args):
            nonlocal version
            version = 'second-history'
            return self.descriptor
        self.t.source_server.call = call
        self.export.side_effect = export
        op = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: not self.store.running)
        self.assertTrue(self.member(op)['archiveInvalidated'])
        self.assertEqual(self.t.pending, [])

    def test_paused_chat_does_not_enqueue_automatic_continuation(self):
        self.t.set_agent(self.aid, status='interrupted', autoWake=False)
        op = self.submit()
        self.finish(op)
        self.assertEqual(self.agent()['status'], 'interrupted')
        with self.rt.db() as db:
            self.assertFalse(db.execute("SELECT 1 FROM runtime_events WHERE agent=? AND status='pending'", (self.aid,)).fetchone())

    def test_failed_auto_resume_enqueues_one_continuation_without_replay(self):
        self.t.set_agent(self.aid, status='failed', autoWake=True)
        op = self.submit()
        self.finish(op)
        self.t.tick()
        with self.rt.db() as db:
            rows = list(db.execute("SELECT * FROM runtime_events WHERE agent=? AND status='pending'", (self.aid,)))
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]['kind'], 'followup')
        self.assertIn('saved context', rows[0]['text'])


if __name__ == '__main__':
    unittest.main()
