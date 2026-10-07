"""Credential copies retain host account identity and omit token diagnostics."""
import base64
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from codex_linux_vm_credentials import claude_keychain_service, profile_path, read_credentials, sync_credentials


class Credentials(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='linux-credentials-')
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.account = {'provider': 'codex', 'accountId': 'fixture-account'}
        self.runtime = SimpleNamespace(accounts=SimpleNamespace(
            get=lambda key: self.account, home=lambda key: self.home))
        self.client = Mock()

    def auth(self, value):
        (self.home / 'auth.json').write_text(json.dumps({'tokens': {'access_token': value}}))

    def test_changed_host_token_is_copied_once_with_content_identity(self):
        self.auth('fixture-token-one')
        first = sync_credentials(self.runtime, self.client, 'host-account')
        self.assertEqual(sync_credentials(self.runtime, self.client, 'host-account', previous_hash=first), first)
        self.assertEqual(self.client.request.call_count, 1)
        self.auth('fixture-token-two')
        second = sync_credentials(self.runtime, self.client, 'host-account', previous_hash=first)
        self.assertNotEqual(second, first)
        self.assertEqual(self.client.request.call_count, 2)
        calls = self.client.request.call_args_list
        self.assertNotEqual(calls[0].kwargs['request_id'], calls[1].kwargs['request_id'])
        file = calls[1].args[1]['files'][0]
        self.assertEqual(file['path'], profile_path('host-account', 'codex') + '/auth.json')
        self.assertEqual(json.loads(base64.b64decode(file['data']))['tokens']['access_token'], 'fixture-token-two')
        self.assertNotIn('fixture-token', calls[1].kwargs['request_id'])

    def test_claude_file_keeps_the_exact_selected_profile(self):
        self.account = {'provider': 'claude', 'accountId': 'claude:fixture@example.test'}
        (self.home / '.credentials.json').write_text(json.dumps({'claudeAiOauth': {'accessToken': 'fixture'}}))
        ready = {'status': 'ready', 'accountId': self.account['accountId']}
        with patch('codex_claude.auth_metadata', return_value=ready), patch('codex_claude.profile_options', return_value={'configDir': str(self.home)}):
            file = read_credentials(self.runtime, 'claude-profile')
        self.assertEqual(file['path'], profile_path('claude-profile', 'claude') + '/.credentials.json')

    def test_custom_keychain_name_matches_observed_cli_profile(self):
        directory = '/Users/igor/.local/state/codex-agents/accounts/claude-ifdotpy'
        self.assertEqual(claude_keychain_service(directory), 'Claude Code-credentials-d5853ba6')
        self.assertEqual(claude_keychain_service(directory + '/'), claude_keychain_service(directory))
        self.assertEqual(claude_keychain_service(), 'Claude Code-credentials')

    def test_custom_keychain_read_uses_selected_cli_directory(self):
        self.account = {'provider':'claude', 'accountId':'claude:fixture@example.test'}
        ready = {'status':'ready', 'accountId':self.account['accountId']}
        credential = json.dumps({'claudeAiOauth':{'accessToken':'fixture'}}).encode()
        with patch('codex_claude.auth_metadata', return_value=ready), \
                patch('codex_claude.subscription_env', return_value={'CLAUDE_CONFIG_DIR':str(self.home)}), \
                patch('codex_linux_vm_credentials.sys.platform', 'darwin'), \
                patch('codex_linux_vm_credentials.subprocess.run', return_value=SimpleNamespace(returncode=0, stdout=credential)) as read:
            file = read_credentials(self.runtime, 'claude-profile')
        self.assertEqual(read.call_args.args[0], ['security', 'find-generic-password', '-s', claude_keychain_service(str(self.home)), '-w'])
        self.assertEqual(json.loads(base64.b64decode(file['data']))['claudeAiOauth']['accessToken'], 'fixture')

    def test_claude_identity_change_is_rejected_before_guest_write(self):
        self.account = {'provider': 'claude', 'accountId': 'claude:fixture@example.test'}
        with patch('codex_claude.auth_metadata', return_value={'status': 'ready', 'accountId': 'different'}):
            with self.assertRaisesRegex(ValueError, 'identity changed'):
                sync_credentials(self.runtime, self.client, 'claude-profile')
        self.client.request.assert_not_called()

    def test_invalid_credentials_do_not_copy_or_include_content_in_error(self):
        (self.home / 'auth.json').write_text('fixture-secret-invalid-json')
        with self.assertRaises(ValueError) as caught:
            sync_credentials(self.runtime, self.client, 'host-account')
        self.assertNotIn('fixture-secret', str(caught.exception))
        self.client.request.assert_not_called()


if __name__ == '__main__':
    unittest.main()
