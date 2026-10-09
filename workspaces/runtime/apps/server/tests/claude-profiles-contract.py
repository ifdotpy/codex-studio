#!/usr/bin/env python3
"""Native Claude profile isolation, safe launch options, durable registration."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
import uuid
from unittest.mock import patch
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_claude as c
from codex_accounts import AccountStore

AUTH = {'status': 'ready', 'accountId': 'claude:a@test', 'email': 'a@test',
        '_credentialIdentity': 'claude:a@test', 'plan': 'max'}

class Profiles(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = patch.dict(os.environ, {'CODEX_HOME': str(self.root / 'codex')})
        self.env.start(); self.addCleanup(self.env.stop)
        c._cache.clear()

    def test_subscription_environment_keeps_home_and_isolates_config(self):
        with patch.dict(os.environ, {'HOME': '/native/home', 'CLAUDE_CONFIG_DIR': '/old',
                 'ANTHROPIC_API_KEY': 'secret', 'CLAUDE_CODE_OAUTH_TOKEN': 'other'}):
            env = c.subscription_env({'configDir': str(self.root)})
        self.assertEqual(env['HOME'], '/native/home')
        self.assertEqual(env['CLAUDE_CONFIG_DIR'], str(self.root.resolve()))
        self.assertNotIn('ANTHROPIC_API_KEY', env)
        self.assertNotIn('CLAUDE_CODE_OAUTH_TOKEN', env)

    def test_cache_separates_config_and_binary(self):
        def run(executable, env, **kwargs):
            email = env.get('CLAUDE_CONFIG_DIR', '') + executable
            return {**AUTH, 'accountId': 'claude:' + email, 'email': email,
                    '_credentialIdentity': 'claude:' + email}
        with patch.object(c, 'installed', side_effect=lambda p=None: (p or {}).get('binaryPath', '/bin/a')), patch.object(c, '_auth_status', side_effect=run) as native:
            a = c.auth_metadata({'configDir': '/tmp/a'})
            b = c.auth_metadata({'configDir': '/tmp/b'})
            d = c.auth_metadata({'configDir': '/tmp/a', 'binaryPath': '/bin/b'})
            self.assertEqual(a, c.auth_metadata({'configDir': '/tmp/a'}))
            self.assertNotEqual(a['accountId'], b['accountId'])
            self.assertNotEqual(a['accountId'], d['accountId'])
            self.assertEqual(native.call_count, 3)

    def test_native_options_and_rejected_transport_overrides(self):
        options = c.bridge_options({'launchArgs': '--chrome --add-dir "/tmp/a b"',
             'autoCompactWindow': '300000', 'customModels': ['sonnet[1m]']})
        self.assertEqual(options['extraArgs'], {'chrome': None, 'add-dir': '/tmp/a b'})
        self.assertEqual(options['autoCompactWindow'], 300000)
        for args in ('--output-format text', '--settings bad.json', '--permission-mode bypassPermissions',
                     '--resume another', '--api-key x', 'echo unsafe'):
            with self.subTest(args=args), self.assertRaises(ValueError):
                c.profile_options({'launchArgs': args})
        for window in (True, 99999, 1000001, '1e6'):
            with self.assertRaises(ValueError): c.profile_options({'autoCompactWindow': window})
        with self.assertRaises(ValueError): c.profile_options({'env': {'HOME': '/wrong'}})

    def test_registration_retry_identity_and_updates(self):
        store = AccountStore(self.root / 'state')
        opts = {'configDir': str(self.root / 'claude'), 'binaryPath': '/bin/claude'}
        with patch.object(c, 'installed', return_value='/bin/claude'), patch.object(c, 'auth_metadata', return_value=AUTH):
            key = store.register_claude(opts, 'Work')
            self.assertEqual(key, store.register_claude(opts, 'Work'))
            store.delete(key, str(uuid.uuid4()))
            self.assertNotIn(key, [account['id'] for account in store.list()])
            self.assertEqual(key, store.register_claude(opts, 'Work'))
            self.assertIn(key, [account['id'] for account in store.list()])
            store.update_claude(key, {**opts, 'autoCompactWindow': 200000})
            reloaded = AccountStore(self.root / 'state')
            self.assertEqual(reloaded.get(key)['claudeOptions']['autoCompactWindow'], 200000)
            with self.assertRaises(ValueError): store.update_claude(key, {**opts, 'configDir': '/another'})
        with patch.object(c, 'auth_metadata', return_value={**AUTH, 'accountId': 'claude:other'}):
            self.assertEqual(store.get(key)['status'], 'changed')

    def test_missing_auth_keeps_original_identity_and_recovers(self):
        store = AccountStore(self.root / 'state')
        with patch.object(c, 'installed', return_value='/bin/claude'), \
             patch.object(c, 'auth_metadata', return_value=AUTH):
            key = store.register_claude({'configDir': str(self.root / 'claude')}, 'Work')
        for metadata in (
            {'status': 'signedOut', 'accountId': None, 'email': None,
             '_credentialIdentity': None, 'plan': None},
            {'status': 'error', 'accountId': None, 'email': None,
             '_credentialIdentity': None, 'error': 'Cannot read Claude Code sign-in status'},
        ):
            with self.subTest(status=metadata['status']), \
                 patch.object(c, 'auth_metadata', return_value=metadata):
                row = store.get(key)
                self.assertEqual(row['status'], metadata['status'])
                self.assertEqual(row['accountId'], AUTH['accountId'])
                self.assertEqual(row['email'], AUTH['email'])
                self.assertEqual(row['plan'], AUTH['plan'])
                self.assertEqual(store.data['accounts'][key]['_credentialIdentity'], AUTH['_credentialIdentity'])
                self.assertEqual(row.get('error'), metadata.get('error'))
                store._save()
                store = AccountStore(self.root / 'state')
                self.assertEqual(store.get(key)['status'], metadata['status'])
        with patch.object(c, 'auth_metadata', return_value=AUTH):
            self.assertEqual(store.get(key)['status'], 'ready')
            self.assertIsNone(store.get(key).get('error'))
        with patch.object(c, 'auth_metadata', return_value={
                **AUTH, 'accountId': 'claude:other', '_credentialIdentity': 'claude:other'}):
            self.assertEqual(store.get(key)['status'], 'changed')

    def test_concurrent_native_confirmation_does_not_return_old_keychain_error(self):
        store = AccountStore(self.root / 'state')
        with patch.object(c, 'installed', return_value='/bin/claude'), \
             patch.object(c, 'auth_metadata', return_value=AUTH):
            key = store.register_claude({'configDir': str(self.root / 'claude')}, 'Work')
        denied = {'status': 'error', 'accountId': None, 'email': None, 'plan': None,
                  '_authErrorKind': 'keychain', 'error': 'Keychain interaction is unavailable'}
        entered, release = threading.Event(), threading.Event()
        results = []
        errors = []

        def proof(account_key, observed):
            if threading.current_thread() is worker:
                entered.set()
                if not release.wait(5):
                    raise TimeoutError('Concurrent refresh did not finish')
                # The runtime skips its read if another refresh already confirmed the account.
                if not store.allow_native_auth_attempt(account_key, observed):
                    return None
            return store.confirm_native_auth(account_key, observed, {
                'account': {'type': 'claude', 'email': AUTH['email'], 'planType': AUTH['plan']},
            })

        def refresh():
            try:
                results.append(store.get(key))
            except Exception as error:
                errors.append(error)

        worker = threading.Thread(target=refresh)
        store.native_auth_proof = proof
        with patch.object(c, 'auth_metadata', return_value=denied):
            worker.start()
            try:
                self.assertTrue(entered.wait(5))
                current = store.get(key)
                self.assertEqual(current['status'], 'ready')
            finally:
                release.set()
                worker.join(5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(results[0]['status'], 'ready')
        self.assertNotIn('error', results[0])
        self.assertEqual(results[0]['accountId'], AUTH['accountId'])

    def test_two_native_reads_can_confirm_the_same_account(self):
        store = AccountStore(self.root / 'state')
        with patch.object(c, 'installed', return_value='/bin/claude'), \
             patch.object(c, 'auth_metadata', return_value=AUTH):
            key = store.register_claude({'configDir': str(self.root / 'claude')}, 'Work')
        denied = {'status': 'error', 'accountId': None, 'email': None, 'plan': None,
                  '_authErrorKind': 'keychain', 'error': 'Keychain interaction is unavailable'}
        with patch.object(c, 'auth_metadata', return_value=denied):
            observed = store.get(key)
        proof = {'account': {'type': 'claude', 'email': AUTH['email'], 'planType': AUTH['plan']}}
        first = store.confirm_native_auth(key, observed, proof)
        self.assertTrue(store.allow_native_auth_attempt(key, observed))
        second = store.confirm_native_auth(key, observed, proof)
        self.assertEqual(first, second)
        self.assertEqual(second['status'], 'ready')
        self.assertNotIn('canAttemptNativeProof', second)
        self.assertFalse(store.allow_native_auth_attempt(key, second))

        row = store.data['accounts'][key]
        mutations = {'home': '/another/home', 'claudeOptions': {'configDir': '/another/config'},
                     'email': 'other@test', 'accountId': 'claude:other@test',
                     '_credentialIdentity': 'claude:other@test', 'provider': 'codex',
                     'deleted': True, 'disconnected': True, 'duplicateOf': 'other',
                     'status': 'changed'}
        for field, value in mutations.items():
            with self.subTest(field=field), patch.dict(row, {field: value}):
                self.assertFalse(store.allow_native_auth_attempt(key, observed))
                with self.assertRaises(ValueError):
                    store.confirm_native_auth(key, observed, proof)
        for account in ({'type': 'claude', 'email': 'other@test', 'planType': 'max'},
                        {'type': 'claude', 'email': AUTH['email'], 'planType': ''},
                        {'type': 'api', 'email': AUTH['email'], 'planType': 'max'}):
            with self.subTest(account=account), self.assertRaises(ValueError):
                store.confirm_native_auth(key, observed, {'account': account})

    def test_profile_change_during_native_confirmation_does_not_return_old_proof(self):
        store = AccountStore(self.root / 'state')
        with patch.object(c, 'installed', return_value='/bin/claude'), \
             patch.object(c, 'auth_metadata', return_value=AUTH):
            key = store.register_claude({'configDir': str(self.root / 'claude')}, 'Work')
        denied = {'status': 'error', 'accountId': None, 'email': None, 'plan': None,
                  '_authErrorKind': 'keychain', 'error': 'Keychain interaction is unavailable'}
        next_options = c.profile_options({'configDir': str(self.root / 'other')})

        def proof(account_key, observed):
            old_proof = store.confirm_native_auth(account_key, observed, {
                'account': {'type': 'claude', 'email': AUTH['email'], 'planType': AUTH['plan']},
            })
            with store.lock:
                store.data['accounts'][key]['claudeOptions'] = next_options
                store.data['accounts'][key].update(status='error', _authErrorKind='parser',
                                                 error='The new profile cannot be verified')
            return old_proof

        store.native_auth_proof = proof
        with patch.object(c, 'auth_metadata', side_effect=[denied, {
                **denied, '_authErrorKind': 'parser', 'error': 'The new profile cannot be verified',
        }]) as native:
            current = store.get(key)
        self.assertEqual(native.call_count, 2)
        self.assertEqual(current['status'], 'error')
        self.assertEqual(current['claudeOptions'], next_options)
        self.assertNotIn('canAttemptNativeProof', current)

    def test_ready_without_pinned_identity_fails_closed(self):
        store = AccountStore(self.root / 'state')
        with patch.object(c, 'installed', return_value='/bin/claude'), \
             patch.object(c, 'auth_metadata', return_value=AUTH):
            key = store.register_claude({'configDir': str(self.root / 'claude')}, 'Work')
        for missing in ('accountId', '_credentialIdentity'):
            with self.subTest(missing=missing), \
                 patch.object(c, 'auth_metadata', return_value={**AUTH, missing: None}):
                self.assertEqual(store.get(key)['status'], 'error')
                with self.assertRaisesRegex(ValueError, 'Cannot verify'):
                    store.home(key)
                self.assertEqual(store.data['accounts'][key][missing], AUTH[missing])

    def test_explicit_add_restores_discovered_claude_local_identity(self):
        store = AccountStore(self.root / 'state')
        home = self.root / 'home'
        with patch.object(Path, 'home', return_value=home), \
             patch.object(c, 'installed', return_value='/bin/claude'), \
             patch.object(c, 'auth_metadata', return_value=AUTH):
            store.discover()
            row = store.data['accounts']['claude-local']
            row['deleted'] = True
            store._save()
            self.assertNotIn('claude-local', [a['id'] for a in store.list()])

            key = store.register_claude({}, 'Personal Claude')

        self.assertEqual(key, 'claude-local')
        visible = [a['id'] for a in store.list()]
        self.assertIn('claude-local', visible)
        self.assertEqual(visible.count('claude-local'), 1)
        self.assertFalse(any(key.startswith('claude-profile-') for key in visible))
        self.assertEqual(store.get(key)['label'], 'Personal Claude')

    def test_default_add_preserves_existing_claude_options_on_active_and_deleted_local(self):
        store = AccountStore(self.root / 'state')
        home = self.root / 'home'
        saved_options = c.profile_options({
            'customModels': ['sonnet[1m]'],
            'autoCompactWindow': 300000,
            'launchArgs': '--no-chrome',
        })
        with patch.object(Path, 'home', return_value=home), \
             patch.object(c, 'installed', return_value='/bin/claude'), \
             patch.object(c, 'auth_metadata', return_value=AUTH):
            store.discover()
            local = store.data['accounts']['claude-local']
            local['claudeOptions'] = saved_options
            store._save()

            self.assertEqual(store.register_claude({}, 'Personal Claude'), 'claude-local')
            self.assertEqual(store.get('claude-local')['claudeOptions'], saved_options)

            conflicting = {
                'customModels': ['opus'],
                'autoCompactWindow': 400000,
                'launchArgs': '--no-chrome',
            }
            with self.assertRaisesRegex(ValueError, 'already exists'):
                store.register_claude(conflicting, 'Personal Claude')

            store.delete('claude-local', str(uuid.uuid4()))
            self.assertNotIn('claude-local', [a['id'] for a in store.list()])
            self.assertEqual(store.register_claude({}, 'Personal Claude'), 'claude-local')
            self.assertEqual(store.get('claude-local')['claudeOptions'], saved_options)

    def test_existing_default_profile_accepts_full_row(self):
        self.assertEqual(c.profile_options({'provider': 'claude', 'id': 'claude-local', **AUTH}),
                         {'customModels': [], 'launchArgs': ''})

if __name__ == '__main__': unittest.main()
