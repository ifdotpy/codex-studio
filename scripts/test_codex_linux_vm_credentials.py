"""The guest receives access credentials only. Refresh remains on the host."""
import base64
import json
from pathlib import Path
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

from codex_linux_vm_credentials import claude_keychain_service, profile_path, read_credentials, sync_credentials
from codex_linux_vm_auth import codex_access, bootstrap_codex, refresh_request, claude_access


def token(expires, label='old'):
    claims = {'exp': expires, 'label': label, 'https://api.openai.com/auth': {'chatgpt_account_id': 'fixture-account'}}
    return 'eyJhbGciOiJub25lIn0.' + base64.urlsafe_b64encode(json.dumps(claims).encode()).decode().rstrip('=') + '.signature'


class Credentials(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='linux-credentials-')
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name).resolve()
        self.account = {'provider': 'codex', 'accountId': 'fixture-account', 'home': str(self.home)}
        self.host = Mock()
        self.runtime = SimpleNamespace(root=self.home, accounts=SimpleNamespace(
            get=lambda key: self.account, home=lambda key: self.home),
            connect=lambda key: self.host, reply=Mock())
        self.client = Mock()

    def auth(self, expires=None, label='old'):
        value = {'tokens': {'access_token': token(expires or time.time()+3600, label),
                           'refresh_token': 'never-export-this-refresh-token', 'account_id': 'fixture-account'}}
        (self.home / 'auth.json').write_text(json.dumps(value))
        return value

    def claude(self, expires=None):
        self.account = {'provider': 'claude', 'accountId': 'claude:fixture@example.test', 'email': 'fixture@example.test',
                        'claudeOptions': {'configDir': str(self.home)}}
        value = {'accessToken':'fixture-access', 'refreshToken':'never-export-this-refresh-token',
                 'expiresAt':expires or (time.time()+3600)*1000, 'scopes':['user:inference'],
                 'subscriptionType':'max', 'unknownSecret':'never-export-this-other-secret'}
        (self.home / '.credentials.json').write_text(json.dumps({'claudeAiOauth':value}))
        return value

    def test_codex_external_login_and_sync_never_export_refresh_or_id_token(self):
        self.auth()
        digest = sync_credentials(self.runtime, self.client, 'host-account')
        self.assertEqual(sync_credentials(self.runtime, self.client, 'host-account', previous_hash=digest), digest)
        file = self.client.request.call_args.args[1]['files'][0]
        self.assertEqual(file['path'], profile_path('host-account','codex')+'/auth.json')
        self.assertEqual(json.loads(base64.b64decode(file['data'])), {})
        guest = Mock()
        bootstrap_codex(self.runtime, guest, 'host-account')
        payload = guest.call.call_args.args[1]
        self.assertEqual(payload['type'], 'chatgptAuthTokens')
        self.assertEqual(set(payload), {'type','accessToken','chatgptAccountId'})
        self.assertNotIn('never-export', json.dumps(payload))
        self.assertEqual(self.client.request.call_count,1)

    def test_host_refresh_changes_sync_identity_and_guest_access(self):
        self.auth()
        first = sync_credentials(self.runtime,self.client,'host-account')
        self.auth(time.time()-1)
        self.host.call.side_effect = lambda *args, **kwargs: self.auth(label='fresh')
        second = sync_credentials(self.runtime,self.client,'host-account',previous_hash=first)
        self.assertNotEqual(second,first)
        self.host.call.assert_called_once_with('account/read',{'refreshToken':True},timeout=45)
        self.assertEqual(self.client.request.call_count,2)
        self.assertNotEqual(self.client.request.call_args_list[0].kwargs['request_id'],
                            self.client.request.call_args_list[1].kwargs['request_id'])
        self.assertNotIn('never-export', str(self.client.request.call_args_list))

    def test_external_refresh_callback_returns_only_access_token_and_identity(self):
        self.auth()
        self.host.call.side_effect = lambda *args, **kwargs: self.auth(label='fresh')
        refresh_request(self.runtime, {'id': 99, 'params':{'previousAccountId':'fixture-account'}}, 'host-account','linux-connection')
        response,key,connection = self.runtime.reply.call_args.args
        self.assertEqual((key,connection),('host-account','linux-connection'))
        self.assertEqual(set(response['result']),{'accessToken','chatgptAccountId'})
        self.assertNotIn('never-export',json.dumps(response))

    def test_external_refresh_rejects_other_account_before_host_refresh(self):
        self.auth()
        refresh_request(self.runtime, {'id':99, 'params':{'previousAccountId':'other'}},
                        'host-account','linux-connection')
        self.host.call.assert_not_called()
        self.assertIn('error',self.runtime.reply.call_args.args[0])

    def test_failed_host_refresh_is_clear_without_guest_rotation(self):
        self.auth(time.time()-1)
        self.host.call.side_effect = TimeoutError('native timeout')
        refresh_request(self.runtime, {'id': 99, 'params':{'previousAccountId':'fixture-account'}}, 'host-account','linux-connection')
        response = self.runtime.reply.call_args.args[0]
        self.assertIn('guest has no refresh token',response['error']['message'])
        self.client.request.assert_not_called()

    def test_claude_payload_allowlist_keeps_access_scopes_and_expiry_only(self):
        self.claude()
        ready = {'status':'ready','accountId':self.account['accountId']}
        with patch('codex_claude.auth_metadata',return_value=ready):
            file = read_credentials(self.runtime,'claude-profile')
        value = json.loads(base64.b64decode(file['data']))['claudeAiOauth']
        self.assertEqual(set(value),{'accessToken','expiresAt','scopes','subscriptionType'})
        self.assertNotIn('never-export',json.dumps(value))

    def test_claude_host_refresh_without_host_agent_then_push_restores_access(self):
        self.claude(time.time()-1)
        ready = {'status':'ready','accountId':self.account['accountId']}
        def save(account):
            fresh = self.claude()
            fresh['accessToken'] = 'fresh-access'
            (self.home / '.credentials.json').write_text(json.dumps({'claudeAiOauth':fresh}))
        with patch('codex_claude.auth_metadata',return_value=ready), patch('codex_linux_vm_auth.refresh_claude',side_effect=save) as refresh:
            sync_credentials(self.runtime,self.client,'claude-profile')
        refresh.assert_called_once()
        data = self.client.request.call_args.args[1]['files'][0]['data']
        self.assertEqual(json.loads(base64.b64decode(data))['claudeAiOauth']['accessToken'],'fresh-access')
        self.assertNotIn('refreshToken',base64.b64decode(data).decode())
        self.host.call.assert_not_called()

    def test_claude_failed_refresh_does_not_push_expired_credentials(self):
        self.claude(time.time()-1)
        with patch('codex_linux_vm_auth.refresh_claude',side_effect=ValueError('Host refresh failed')):
            with self.assertRaisesRegex(ValueError,'Host refresh failed'):
                sync_credentials(self.runtime,self.client,'claude-profile')
        self.client.request.assert_not_called()

    def test_custom_keychain_name_matches_observed_cli_profile(self):
        directory='/Users/igor/.local/state/codex-agents/accounts/claude-ifdotpy'
        self.assertEqual(claude_keychain_service(directory),'Claude Code-credentials-d5853ba6')
        self.assertEqual(claude_keychain_service(directory+'/'),claude_keychain_service(directory))
        self.assertEqual(claude_keychain_service(),'Claude Code-credentials')

    def test_custom_keychain_read_uses_selected_directory_and_omits_refresh_token(self):
        self.claude()
        data=(self.home/'.credentials.json').read_bytes()
        (self.home/'.credentials.json').unlink()
        ready={'status':'ready','accountId':self.account['accountId']}
        with patch('codex_claude.auth_metadata',return_value=ready), patch('sys.platform','darwin'), \
                patch('codex_linux_vm_auth.subprocess.run',return_value=SimpleNamespace(returncode=0,stdout=data)) as read:
            file=read_credentials(self.runtime,'claude-profile')
        self.assertEqual(read.call_args.args[0],['security','find-generic-password','-s',claude_keychain_service(str(self.home)),'-w'])
        self.assertNotIn('refreshToken',base64.b64decode(file['data']).decode())

    def test_claude_identity_change_is_rejected_before_guest_write(self):
        self.claude()
        with patch('codex_claude.auth_metadata',return_value={'status':'ready','accountId':'different'}):
            with self.assertRaisesRegex(ValueError,'identity changed'):
                sync_credentials(self.runtime,self.client,'claude-profile')
        self.client.request.assert_not_called()

    def test_invalid_credentials_do_not_include_content_in_error(self):
        (self.home/'auth.json').write_text('fixture-secret-invalid-json')
        with self.assertRaises(ValueError) as caught:
            sync_credentials(self.runtime,self.client,'host-account')
        self.assertNotIn('fixture-secret',str(caught.exception))
        self.client.request.assert_not_called()


if __name__=='__main__':
    unittest.main()
