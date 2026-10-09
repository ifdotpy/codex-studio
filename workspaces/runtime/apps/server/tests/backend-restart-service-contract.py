#!/usr/bin/env python3
"""A planned restart needs a verified recovery service before any signal."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import hashlib
import io
import json
import os
from pathlib import Path
import plistlib
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = REPOSITORY_ROOT
SOURCE = (SERVER_SOURCE_ROOT / "restart-backend-v2.sh").read_text().split("<<'PY'\n", 1)[1].rsplit('\nPY', 1)[0]


class RestartService(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='studio-restart-service-')
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name).resolve()
        self.state = self.home / 'state'
        self.state.mkdir()
        self.resources = self.home / 'Codex Studio.app/Contents/Resources/workspace'
        self.resources.mkdir(parents=True)
        (self.resources.parent / 'recover_backend.py').touch()
        for relative in ('scripts/codex-canvas', 'web/dist/index.html'):
            target = self.resources / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.touch()
        self.label = 'local.codex.agents.recovery.' + hashlib.sha256(str(self.state).encode()).hexdigest()[:16]
        self.service = f'gui/{os.getuid()}/{self.label}'
        self.config_file = self.state / 'background-recovery.json'
        self.config = {'version': 1, 'enabled': True, 'supervisorEnabled': True,
                       'stateDir': str(self.state), 'port': 4620, 'resources': str(self.resources),
                       'python': sys.executable, 'codex': sys.executable}
        self.save_config()
        self.plist_file = self.home / 'Library/LaunchAgents' / (self.label + '.plist')
        self.plist_file.parent.mkdir(parents=True)
        self.arguments = [sys.executable, '-B', str(self.resources.parent / 'recover_backend.py'),
                          '--config', str(self.config_file)]
        self.plist = {'Label': self.label, 'ProgramArguments': self.arguments,
                      'KeepAlive': True, 'RunAtLoad': True}
        self.save_plist()
        self.registered = True
        self.probe_error = None
        self.bootstrap_error = None
        self.apply_failed_bootstrap = False
        self.registered_arguments = self.arguments
        self.calls = []
        self.before = {'application': 'codex-agents', 'stateDir': str(self.state), 'pid': 12345,
                       'supervisorMode': True, 'supervisor': {'protocol': 1, 'stateDir': str(self.state),
                                                           'handles': [{'id': 'account:default'}]}}
        self.after = {**self.before, 'pid': 23456}

    def save_config(self):
        self.config_file.write_text(json.dumps(self.config))

    def save_plist(self):
        self.plist_file.write_bytes(plistlib.dumps(self.plist))

    def run_launchctl(self, argv, **kwargs):
        self.assertEqual(argv[0], '/bin/launchctl')
        self.assertLessEqual(kwargs['timeout'], 5)
        self.calls.append(argv[1:])
        if argv[1:] == ['print', self.service]:
            if self.probe_error:
                raise self.probe_error
            if not self.registered:
                raise subprocess.CalledProcessError(113, argv, stderr='Service not found')
            arguments = '\n'.join('\t\t' + item for item in self.registered_arguments)
            return subprocess.CompletedProcess(argv, 0,
                stdout=f'{self.service} = {{\n\tpath = {self.plist_file}\n\targuments = {{\n{arguments}\n\t}}\n}}\n')
        self.assertEqual(argv[1:], ['bootstrap', f'gui/{os.getuid()}', str(self.plist_file)])
        if self.bootstrap_error:
            if self.apply_failed_bootstrap:
                self.registered = True
            raise self.bootstrap_error
        self.registered = True
        return subprocess.CompletedProcess(argv, 0, stdout='')

    def execute(self, arguments=(), snapshots=None, platform='darwin'):
        values = snapshots or [self.before, self.before, self.after]
        responses = [io.BytesIO(json.dumps(value).encode()) for value in values]
        output = io.StringIO()
        with patch.dict(os.environ, {'CODEX_AGENTS_STATE_DIR': str(self.state), 'CODEX_DESKTOP_PORT': '4620'}), \
                patch.object(sys, 'argv', ['-', *arguments]), patch.object(sys, 'platform', platform), \
                patch.object(Path, 'home', return_value=self.home), \
                patch('urllib.request.urlopen', side_effect=responses), \
                patch('subprocess.run', side_effect=self.run_launchctl), \
                patch('os.kill') as kill, patch('time.sleep'), patch('sys.stdout', output):
            self.kill = kill
            exec(compile(SOURCE, str(SERVER_SOURCE_ROOT / "restart-backend-v2.sh"), 'exec'), {})
        return output.getvalue()

    def assert_preflight_rejected(self, pattern):
        with self.assertRaisesRegex(SystemExit, pattern):
            self.execute()
        self.kill.assert_not_called()

    def test_registered_recovery_is_verified_before_signalling_only_the_backend(self):
        self.execute()
        self.assertEqual(self.calls, [['print', self.service]])
        self.kill.assert_called_once_with(self.before['pid'], signal.SIGTERM)

    def test_missing_recovery_is_registered_and_read_back_before_the_signal(self):
        self.registered = False
        self.execute()
        self.assertEqual(self.calls, [['print', self.service],
                                     ['bootstrap', f'gui/{os.getuid()}', str(self.plist_file)],
                                     ['print', self.service]])
        self.kill.assert_called_once_with(self.before['pid'], signal.SIGTERM)

    def test_disabled_recovery_preserves_the_current_backend(self):
        self.config['enabled'] = False
        self.save_config()
        self.assert_preflight_rejected('Recovery service preflight failed')
        self.assertEqual(self.calls, [])

    def test_foreign_state_port_and_provider_mode_preserve_the_current_backend(self):
        for field, value in [('stateDir', str(self.home / 'foreign')), ('port', 4630),
                             ('supervisorEnabled', False)]:
            with self.subTest(field=field):
                previous = self.config[field]
                self.config[field] = value
                self.save_config()
                self.assert_preflight_rejected('Recovery service preflight failed')
                self.config[field] = previous
        self.assertEqual(self.calls, [])

    def test_unknown_service_probe_does_not_register_or_signal(self):
        self.probe_error = subprocess.CalledProcessError(5, [], stderr='Unknown service state')
        self.assert_preflight_rejected('Recovery service preflight failed')
        self.assertEqual(self.calls, [['print', self.service]])

    def test_failed_bootstrap_preserves_the_current_backend(self):
        self.registered = False
        self.bootstrap_error = subprocess.CalledProcessError(5, [], stderr='Cannot register service')
        self.assert_preflight_rejected('Recovery service preflight failed')
        self.assertEqual(len(self.calls), 3)

    def test_lost_bootstrap_response_uses_the_confirmed_registration(self):
        self.registered = False
        self.bootstrap_error = subprocess.TimeoutExpired([], 5)
        self.apply_failed_bootstrap = True
        self.execute()
        self.assertEqual(len(self.calls), 3)
        self.kill.assert_called_once_with(self.before['pid'], signal.SIGTERM)

    def test_registered_foreign_configuration_preserves_the_current_backend(self):
        self.registered_arguments = [*self.arguments[:-1], str(self.home / 'foreign.json')]
        self.assert_preflight_rejected('Recovery service preflight failed')

    def test_invalid_service_file_is_not_registered(self):
        self.registered = False
        self.plist['ProgramArguments'][-1] = str(self.home / 'foreign.json')
        self.save_plist()
        self.assert_preflight_rejected('Recovery service preflight failed')
        self.assertEqual(self.calls, [])

    def test_backend_replaced_during_preflight_is_not_signalled(self):
        with self.assertRaisesRegex(SystemExit, 'Backend identity changed during preflight'):
            self.execute(snapshots=[self.before, self.after])
        self.kill.assert_not_called()

    def test_missing_backend_launcher_preserves_the_current_backend(self):
        (self.resources / 'scripts/codex-canvas').unlink()
        self.assert_preflight_rejected('Recovery service preflight failed')
        self.assertEqual(self.calls, [])

    def test_missing_saved_interpreter_preserves_the_current_backend(self):
        self.config['python'] = str(self.home / 'missing-python')
        self.save_config()
        self.assert_preflight_rejected('Recovery service preflight failed')
        self.assertEqual(self.calls, [])

    def test_registered_job_can_retain_its_previous_interpreter(self):
        self.registered_arguments = ['/usr/bin/python3', *self.arguments[1:]]
        with patch('os.access', return_value=True):
            self.execute()
        self.kill.assert_called_once_with(self.before['pid'], signal.SIGTERM)

    def test_probe_timeout_preserves_the_current_backend(self):
        self.probe_error = subprocess.TimeoutExpired([], 5)
        self.assert_preflight_rejected('Recovery service preflight failed')
        self.assertEqual(self.calls, [['print', self.service]])

    def test_malformed_service_response_preserves_the_current_backend(self):
        self.registered_arguments = []
        self.assert_preflight_rejected('Recovery service preflight failed')

    def test_linux_restart_does_not_require_launchctl(self):
        self.execute(platform='linux')
        self.assertEqual(self.calls, [])
        self.kill.assert_called_once_with(self.before['pid'], signal.SIGTERM)

    def test_check_only_verifies_recovery_without_any_signal(self):
        with self.assertRaises(SystemExit) as result:
            self.execute(['--check-only'])
        self.assertEqual(result.exception.code, 0)
        self.kill.assert_not_called()
        self.assertEqual(self.calls, [['print', self.service]])


if __name__ == '__main__':
    unittest.main()
