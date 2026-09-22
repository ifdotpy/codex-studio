#!/usr/bin/env python3
"""Native Claude profile isolation, safe launch options, durable registration."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
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
            store.update_claude(key, {**opts, 'autoCompactWindow': 200000})
            reloaded = AccountStore(self.root / 'state')
            self.assertEqual(reloaded.get(key)['claudeOptions']['autoCompactWindow'], 200000)
            with self.assertRaises(ValueError): store.update_claude(key, {**opts, 'configDir': '/another'})
        with patch.object(c, 'auth_metadata', return_value={**AUTH, 'accountId': 'claude:other'}):
            self.assertEqual(store.get(key)['status'], 'changed')

    def test_existing_default_profile_accepts_full_row(self):
        self.assertEqual(c.profile_options({'provider': 'claude', 'id': 'claude-local', **AUTH}),
                         {'customModels': [], 'launchArgs': ''})

if __name__ == '__main__': unittest.main()
