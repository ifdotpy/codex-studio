#!/usr/bin/env python3
"""Passive authentication must not show dialogs or leave credential readers."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

import contextlib
import ctypes
import io
import json
import os
from pathlib import Path
import subprocess
import signal
import sys
import tempfile
import time
import unittest
from unittest.mock import MagicMock, patch

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_claude
import codex_claude_passive_auth as passive


class PassiveAuth(unittest.TestCase):
    def test_native_credentials_keep_identity_without_cli_or_keychain_dialogs(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            (directory / '.claude.json').write_text(json.dumps({
                'oauthAccount': {'emailAddress': 'fixture@example.test'},
            }))
            credential = json.dumps({'claudeAiOauth': {
                'accessToken': 'fixture-secret-never-emitted',
                'scopes': ['user:inference'], 'subscriptionType': 'max',
            }}).encode()
            buffer = ctypes.create_string_buffer(credential)
            security = MagicMock()
            calls = []
            security.SecKeychainSetUserInteractionAllowed.side_effect = lambda allowed: calls.append(('ui', allowed)) or 0
            def find(_keychain, _service_size, service, _account_size, account, size, data, _item):
                calls.append(('read', service, account))
                ctypes.cast(size, ctypes.POINTER(ctypes.c_uint32))[0] = len(credential)
                ctypes.cast(data, ctypes.POINTER(ctypes.c_void_p))[0] = ctypes.addressof(buffer)
                return 0
            security.SecKeychainFindGenericPassword.side_effect = find
            with patch.dict(os.environ, {'CLAUDE_CONFIG_DIR': str(directory), 'USER': 'fixture'}), \
                    patch.object(passive.ctypes, 'CDLL', return_value=security), \
                    patch.object(subprocess, 'Popen', side_effect=AssertionError('No CLI startup')):
                os.environ.pop('CLAUDE_SECURESTORAGE_CONFIG_DIR', None)
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    passive.main()
                result = json.loads(output.getvalue())
                self.assertEqual(result['status'], 'ready')
                self.assertEqual(result['accountId'], 'claude:fixture@example.test')
                self.assertEqual(result['plan'], 'max')
                self.assertNotIn('fixture-secret', output.getvalue())
                self.assertEqual(calls[0], ('ui', False))
                self.assertEqual(calls[1][1:], (
                    passive.claude_keychain_service(str(directory)).encode(), b'fixture',
                ))
                self.assertEqual(security.SecKeychainItemFreeContent.call_count, 1)
                (directory / '.credentials.json').write_bytes(credential)
                for denied in (-25308, -25293, -128, -25291):
                    security.SecKeychainFindGenericPassword.side_effect = None
                    security.SecKeychainFindGenericPassword.return_value = denied
                    output = io.StringIO()
                    with contextlib.redirect_stdout(output):
                        passive.main()
                    error = json.loads(output.getvalue())
                    self.assertEqual(error['status'], 'error')
                    self.assertEqual(error['_authErrorKind'], 'keychain')
                    self.assertIsNone(error['accountId'])
                security.SecKeychainFindGenericPassword.return_value = -50
                output = io.StringIO()
                with contextlib.redirect_stdout(output):
                    passive.main()
                self.assertEqual(json.loads(output.getvalue())['_authErrorKind'], 'parser')
                security.SecKeychainFindGenericPassword.return_value = -25300
                self.assertEqual(passive.metadata()['accountId'], result['accountId'])
                with patch.object(passive, 'read_json', side_effect=PermissionError('Fixture read denied')):
                    output = io.StringIO()
                    with contextlib.redirect_stdout(output):
                        passive.main()
                    self.assertEqual(json.loads(output.getvalue())['_authErrorKind'], 'parser')
                (directory / '.credentials.json').unlink()
                self.assertEqual(passive.metadata()['status'], 'signedOut')
                security.SecKeychainSetUserInteractionAllowed.return_value = -1
                security.SecKeychainSetUserInteractionAllowed.side_effect = None
                security.SecKeychainFindGenericPassword.reset_mock()
                with self.assertRaises(OSError):
                    passive.metadata()
                security.SecKeychainFindGenericPassword.assert_not_called()

    @unittest.skipIf(os.name == 'nt', 'POSIX process groups')
    def test_auth_probe_timeout_reaps_its_credential_reader(self):
        with tempfile.TemporaryDirectory() as scratch:
            directory = Path(scratch)
            child_path = directory / 'child.pid'
            executable = directory / 'claude'
            executable.write_text('#!' + sys.executable + '\n'
                'import subprocess, sys, time\n'
                'child=subprocess.Popen([sys.executable,"-c","import time;time.sleep(60)"])\n'
                f'open({str(child_path)!r},"w").write(str(child.pid))\n'
                'time.sleep(60)\n')
            executable.chmod(0o700)
            def cleanup_child():
                if child_path.exists():
                    try:
                        os.kill(int(child_path.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
            self.addCleanup(cleanup_child)
            with patch.object(codex_claude.sys, 'platform', 'linux'), \
                    patch.object(codex_claude, 'installed', return_value=str(executable)):
                result = codex_claude.auth_metadata(force=True)
            self.assertEqual(result['status'], 'error')
            self.assertEqual(result['_authErrorKind'], 'timeout')
            child = int(child_path.read_text())
            for _ in range(40):
                status = subprocess.run(['ps', '-o', 'stat=', '-p', str(child)], capture_output=True, text=True)
                if status.returncode or status.stdout.strip().startswith('Z'):
                    break
                time.sleep(.05)
            else:
                self.fail('The credential reader survived the auth probe timeout')


if __name__ == '__main__':
    unittest.main()
