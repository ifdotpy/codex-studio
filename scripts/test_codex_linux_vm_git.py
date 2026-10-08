"""Host Git identity transfer through the actual guest file and receipt service."""
import asyncio
import base64
import copy
import importlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from typing import Any, cast
from unittest.mock import patch

from codex_linux_vm_credentials import sync_credentials

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'vm/guest'))
Service = importlib.import_module("service").Service


class GuestClient:
    def __init__(self, service: Any) -> None:
        self.service, self.drop_reply = service, False
        self.calls: list[tuple[str, dict[str, Any]]] = []

    def request(self, method: str, params: dict[str, Any], *, request_id: str, timeout: int) -> dict[str, Any]:
        self.calls.append((request_id, copy.deepcopy(params)))
        async def run() -> dict[str, Any]:
            async def emit(*args: Any) -> None:
                raise AssertionError('A file write must not emit provider events')
            return cast(dict[str, Any], await self.service.request({'id': request_id, 'method': method, 'params': params}, emit))
        response = asyncio.run(run())
        if 'error' in response:
            raise RuntimeError(response['error']['code'])
        if self.drop_reply:
            self.drop_reply = False
            raise TimeoutError('The file write response was lost')
        return cast(dict[str, Any], response['result'])


class GitIdentity(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix='linux-git-identity-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.host_config = self.root / 'host.gitconfig'
        self.host_config.touch()
        self.guest_home = self.root / 'guest-home'
        self.guest_home.mkdir()
        self.service = Service(self.root / 'guest-state', self.root / 'store', self.root / 'projects', self.guest_home)
        self.addCleanup(self.service.close)
        self.client = GuestClient(self.service)
        self.runtime = SimpleNamespace(root=self.root)
        self.enterContext(patch.dict(os.environ, {'GIT_CONFIG_GLOBAL': str(self.host_config), 'GIT_CONFIG_NOSYSTEM': '1'}))
        self.enterContext(patch('codex_linux_vm_credentials.read_credentials', return_value={
            'path': '.claude/profile.json', 'data': base64.b64encode(b'{}').decode()}))
        self.enterContext(patch('codex_linux_vm_auth.account_snapshot', return_value={'provider': 'claude'}))

    def git(self, *args: str) -> bytes:
        return subprocess.run(['git', *args], check=True, capture_output=True).stdout

    def host(self, key: str, value: str) -> None:
        self.git('config', '--file', str(self.host_config), key, value)

    def sync(self, previous_hash: str | None = None) -> str:
        return cast(str, sync_credentials(self.runtime, self.client, 'fixture-account', previous_hash=previous_hash))

    def identity_calls(self) -> list[tuple[str, dict[str, Any]]]:
        return [(key, params) for key, params in self.client.calls
                if any(file['path'] == '.gitconfig' for file in params['files'])]

    def guest(self, key: str) -> str:
        return self.git('config', '--file', str(self.guest_home / '.gitconfig'), '--null', '--get', key)[:-1].decode()

    def test_only_global_identity_keys_cross_and_quotes_round_trip(self) -> None:
        name, email = '  A "quoted" \\ name # ; [credential]  ', 'user"quoted"@example.test'
        self.host('user.name', name)
        self.host('user.email', email)
        for key in ('credential.helper', 'gpg.program', 'core.sshCommand', 'url.ext::bad.insteadOf', 'user.signingKey'):
            self.host(key, 'must-not-cross')
        self.host('include.path', str(self.root / 'absent-included-config'))
        self.sync()
        self.assertTrue((self.guest_home / '.gitconfig').exists())
        self.assertEqual(self.guest('user.name'), name)
        self.assertEqual(self.guest('user.email'), email)
        keys = self.git('config', '--file', str(self.guest_home / '.gitconfig'), '--name-only', '--list').decode().splitlines()
        self.assertEqual(keys, ['user.name', 'user.email'])
        self.assertEqual((self.guest_home / '.gitconfig').stat().st_mode & 0o777, 0o600)

    def test_absent_identity_does_not_write_or_change_guest_config(self) -> None:
        path = self.guest_home / '.gitconfig'
        path.write_text('[user]\n name = Existing guest\n')
        original = path.read_bytes()
        self.sync()
        self.assertEqual(path.read_bytes(), original)
        self.assertEqual(self.identity_calls(), [])

    def test_partial_identity_copies_only_the_set_key(self) -> None:
        self.host('user.name', 'Only name')
        self.sync()
        self.assertEqual(self.guest('user.name'), 'Only name')
        self.assertEqual(self.git('config', '--file', str(self.guest_home / '.gitconfig'), '--name-only', '--list'), b'user.name\n')

    def test_newlines_are_rejected_before_any_guest_write(self) -> None:
        for key in ('user.name', 'user.email'):
            for value in ('Bad\n[credential]\nhelper=bad', 'Bad\rName'):
                with self.subTest(key=key, value=value):
                    self.host(key, value)
                    with self.assertRaisesRegex(ValueError, 'Git identity'):
                        self.sync()
                    self.assertEqual(self.client.calls, [])
                    self.assertFalse((self.guest_home / '.gitconfig').exists())
                    self.git('config', '--file', str(self.host_config), '--unset', key)

    def test_nul_is_rejected_before_any_guest_write(self) -> None:
        from codex_linux_vm_git import read_git_identity
        with patch('codex_linux_vm_git.subprocess.run', return_value=SimpleNamespace(returncode=0, stdout=b'Bad\0value\0')):
            with self.assertRaisesRegex(ValueError, 'Git identity'):
                read_git_identity()
        self.assertEqual(self.client.calls, [])

    def test_guest_git_commit_uses_host_identity(self) -> None:
        self.host('user.name', 'Fixture Author')
        self.host('user.email', 'fixture@example.test')
        self.sync()
        repo = self.guest_home / 'repo'
        repo.mkdir()
        env = {key: value for key, value in os.environ.items()
               if not key.startswith(('GIT_AUTHOR_', 'GIT_COMMITTER_', 'GIT_CONFIG_KEY_', 'GIT_CONFIG_VALUE_'))
               and key != 'GIT_CONFIG_COUNT'}
        env['GIT_CONFIG_GLOBAL'] = str(self.guest_home / '.gitconfig')
        for args in (('init', '-q'), ('-c', 'commit.gpgSign=false', '-c', 'core.hooksPath=/dev/null',
                    'commit', '--allow-empty', '-qm', 'Identity fixture')):
            subprocess.run(['git', '-C', str(repo), *args], env=env, check=True, capture_output=True)
        result = subprocess.run(['git', '-C', str(repo), 'log', '-1', '--format=%an%x00%ae'],
                                env=env, check=True, capture_output=True).stdout
        self.assertEqual(result, b'Fixture Author\0fixture@example.test\n')

    def test_changed_identity_refreshes_with_unchanged_credentials_and_can_return_to_old_value(self) -> None:
        self.host('user.email', 'fixture@example.test')
        digests = []
        previous: str | None = None
        for name in ('First', 'Second', 'First'):
            self.host('user.name', name)
            previous = self.sync(previous)
            digests.append(previous)
            self.assertEqual(self.guest('user.name'), name)
        self.assertEqual(len(set(digests)), 1)
        calls = self.identity_calls()
        self.assertEqual(len(calls), 3)
        self.assertEqual(len({key for key, _ in calls}), 3)
        self.sync(previous)
        self.assertEqual(len(self.identity_calls()), 3)

    def test_lost_reply_reuses_persisted_identity_before_new_host_value(self) -> None:
        self.host('user.name', 'First')
        self.client.drop_reply = True
        with self.assertRaises(TimeoutError):
            self.sync()
        original_id = self.identity_calls()[0][0]
        self.runtime = SimpleNamespace(root=self.root)
        self.host('user.name', 'Second')
        self.sync()
        calls = self.identity_calls()
        self.assertEqual(calls[1][0], original_id)
        self.assertEqual(calls[1][1], calls[0][1])
        self.assertNotEqual(calls[2][0], original_id)
        self.assertEqual(self.guest('user.name'), 'Second')


if __name__ == '__main__':
    unittest.main()
