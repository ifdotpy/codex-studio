#!/usr/bin/env python3
"""Native Claude profile isolation, safe launch options, durable registration."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import uuid
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
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
        def run(argv, **kwargs):
            email = kwargs['env'].get('CLAUDE_CONFIG_DIR', '') + argv[0]
            return subprocess.CompletedProcess(argv, 0, json.dumps({'loggedIn': True,
                'authMethod': 'claude.ai', 'email': email, 'subscriptionType': 'max'}))
        with patch.object(c, 'installed', side_effect=lambda p=None: (p or {}).get('binaryPath', '/bin/a')), patch.object(c.subprocess, 'run', side_effect=run) as native:
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
