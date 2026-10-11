"""The operator command for a project's VM main line, without a VM (codex_layr_exec.py)."""
from __future__ import annotations

import base64
import hashlib
import io
from typing import Any
import unittest
from unittest.mock import patch

import codex_layr_exec


class Client:
    calls: list[tuple[str, dict[str, Any], str, float]] = []

    def call(self, method: str, params: dict[str, Any], *, request_id: str, timeout: float) -> dict[str, Any]:
        self.calls.append((method, params, request_id, timeout))
        return {'stdout': base64.b64encode(b'out\n').decode(), 'stderr': '', 'exitCode': 3, 'reason': 'exited'}


class ExecTests(unittest.TestCase):
    def test_the_command_goes_to_the_project_of_the_folder_with_a_reusable_identity(self) -> None:
        Client.calls = []
        out, err = io.BytesIO(), io.BytesIO()
        stdout, stderr = io.TextIOWrapper(out), io.TextIOWrapper(err)
        with patch('codex_linux_vm.Client', Client), patch('sys.stdout', stdout), patch('sys.stderr', stderr):
            code = codex_layr_exec.main(['--operation-id', 'op-1', '--subpath', 'web', '/Users/me/app',
                                         '--', 'pnpm', 'install'])
        stdout.flush()
        stderr.flush()
        self.assertEqual(code, 3)
        self.assertEqual(out.getvalue(), b'out\n')
        self.assertIn(b'operation op-1', err.getvalue())
        self.assertEqual(Client.calls, [('project.exec', {
            'projectId': hashlib.sha256(b'/Users/me/app').hexdigest(), 'argv': ['pnpm', 'install'],
            'subpath': 'web', 'timeoutSeconds': 300}, 'op-1', 330)])

    def test_an_option_after_the_folder_is_not_run_as_the_command(self) -> None:
        Client.calls = []
        out, err = io.BytesIO(), io.BytesIO()
        with patch('codex_linux_vm.Client', Client), patch('sys.stdout', io.TextIOWrapper(out)), \
                patch('sys.stderr', io.TextIOWrapper(err)):
            codex_layr_exec.main(['/Users/me/app', '--operation-id', 'op-2', '--', 'id'])
            for arguments in (['/Users/me/app', 'id'], ['/Users/me/app', '--']):
                with self.subTest(arguments=arguments), self.assertRaises(SystemExit):
                    codex_layr_exec.main(arguments)
        self.assertEqual([(call[1]['argv'], call[2]) for call in Client.calls], [(['id'], 'op-2')])

if __name__ == '__main__':
    unittest.main()
