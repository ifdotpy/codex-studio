"""The project secrets command (docs/project-secrets.md): the value comes from stdin, never from
the command line, and no output shows it."""
from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import io
from pathlib import Path
import tempfile
import unittest
from typing import Any
from unittest.mock import patch

import codex_project_secrets as secrets
from codex_layr_migration import project_id
from codex_linux_vm import LinuxVMError

VALUE = 'cli-value-91d2e4'


class FakeClient:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any], str | None]] = []
        self.names: set[str] = set()

    def call(self, method: str, params: dict[str, Any] | None = None, *, request_id: str | None = None,
             timeout: float = 60) -> dict[str, Any]:
        params = params or {}
        self.calls.append((method, params, request_id))
        if params.get('name') == 'PATH':
            raise LinuxVMError('Linux guest secrets.set: {...}', code='invalid_params',
                               detail='The name PATH is reserved: Studio or the provider sets it')
        if method == 'secrets.set':
            self.names.add(str(params['name']))
        if method == 'secrets.remove':
            self.names.discard(str(params['name']))
        return {'projectId': params['projectId'], 'names': sorted(self.names)}


class TerminalInput(io.StringIO):
    def isatty(self) -> bool:
        return True


class CommandTests(unittest.TestCase):
    def run_command(self, arguments: list[str], stdin: io.StringIO) -> tuple[int, str, str, FakeClient]:
        client = FakeClient()
        output, errors = io.StringIO(), io.StringIO()
        with patch('codex_linux_vm.Client', return_value=client), patch('sys.stdin', stdin), \
                redirect_stdout(output), redirect_stderr(errors):
            try:
                code = secrets.main(arguments)
            except SystemExit as exit_:
                code = int(exit_.code or 0)
        return code, output.getvalue(), errors.getvalue(), client

    def test_set_reads_the_value_from_stdin_and_prints_names_only(self) -> None:
        with tempfile.TemporaryDirectory() as name:
            folder = Path(name)
            code, output, errors, client = self.run_command(
                ['set', str(folder), 'ZULIP_APEIRO_SHOWROOM_API_KEY'], io.StringIO(VALUE + '\n'))
        self.assertEqual(code, 0, errors)
        method, params, request_id = client.calls[0]
        self.assertEqual((method, params), ('secrets.set', {
            'projectId': project_id(folder), 'name': 'ZULIP_APEIRO_SHOWROOM_API_KEY', 'value': VALUE}))
        operation = errors.split()[1]
        self.assertEqual(request_id, 'secrets-set:' + operation)
        self.assertEqual(output, 'ZULIP_APEIRO_SHOWROOM_API_KEY\n')
        self.assertNotIn(VALUE, output + errors)

    def test_a_terminal_asks_without_echo(self) -> None:
        with patch('getpass.getpass', return_value=VALUE) as ask:
            code, output, errors, client = self.run_command(['set', '/tmp/project', 'TOKEN'], TerminalInput())
        self.assertEqual(code, 0, errors)
        ask.assert_called_once()
        self.assertEqual(client.calls[0][1]['value'], VALUE)
        self.assertNotIn(VALUE, output + errors)

    def test_a_repeat_uses_the_given_operation_and_remove_and_list_take_no_value(self) -> None:
        code, _output, errors, client = self.run_command(
            ['remove', '/tmp/project', 'TOKEN', '--operation-id', 'op-7'], io.StringIO(VALUE))
        self.assertEqual(code, 0, errors)
        self.assertEqual(client.calls, [('secrets.remove', {'projectId': project_id(Path('/tmp/project')),
                                                            'name': 'TOKEN'}, 'secrets-remove:op-7')])
        code, output, _errors, client = self.run_command(['list', '/tmp/project'], io.StringIO(VALUE))
        self.assertEqual((code, output), (0, ''))
        self.assertEqual(client.calls[0][0], 'secrets.list')

    def test_bad_input_stops_before_the_vm(self) -> None:
        for arguments, stdin in ((['set', '/tmp/project', 'lower'], VALUE), (['set', '/tmp/project'], VALUE),
                                 (['set', '/tmp/project', 'TOKEN'], ''), (['set', '/tmp/project', 'TOKEN'], '\n'),
                                 (['set', '/tmp/project', 'TOKEN'], 'x' * (secrets.MAX_PROJECT_SECRET_VALUE_BYTES + 1))):
            with self.subTest(arguments=arguments, size=len(stdin)):
                code, output, errors, client = self.run_command(arguments, io.StringIO(stdin))
                self.assertEqual(code, 2)
                self.assertEqual(client.calls, [])
                self.assertNotIn(VALUE, output + errors)

    def test_stdin_accepts_exact_utf8_byte_limit(self) -> None:
        value = 'é' * (secrets.MAX_PROJECT_SECRET_VALUE_BYTES // 2)
        self.assertEqual(len(value.encode('utf-8')), secrets.MAX_PROJECT_SECRET_VALUE_BYTES)
        self.assertEqual(secrets.read_value(io.StringIO(value)), value)

    def test_a_refused_name_shows_the_reason_of_the_vm(self) -> None:
        code, output, errors, _client = self.run_command(['set', '/tmp/project', 'PATH'], io.StringIO(VALUE))
        self.assertEqual(code, 1)
        self.assertIn('The name PATH is reserved', errors)
        self.assertNotIn(VALUE, output + errors)


if __name__ == '__main__':
    unittest.main()
