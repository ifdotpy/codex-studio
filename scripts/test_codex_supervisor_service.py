"""Supervisor installation checks use private state and fake service managers."""
from __future__ import annotations

import json
from pathlib import Path
import plistlib
import sqlite3
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import codex_supervisor_service as service


class Services(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.state = self.root / 'state'
        self.state.mkdir()
        self.resources = self.root / 'resources'
        for name in ('codex_process_supervisor.py', 'codex-canvas', 'codex-supervisor'):
            file = self.resources / 'scripts' / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.touch()
        self.config = {'version': 1, 'stateDir': str(self.state), 'resources': str(self.resources),
                       'python': sys.executable, 'port': 4720, 'codex': '/usr/bin/codex',
                       'enabled': True, 'supervisorEnabled': False, 'environment': {'CODEX_HOME': '/private/profile'}}

    def test_idle_refuses_each_active_kind_and_preserves_database(self) -> None:
        for table, record in (('runtime_agents', {'inFlight': True}), ('runtime_agents', {'prepareAttempt': 'a', 'status': 'starting'}),
                              ('runtime_monitors', {'status': 'running'}), ('user_terminals', {'status': 'running'}),
                              ('runtime_server_exec', {'status': 'starting'})):
            with self.subTest(table=table, record=record):
                file = self.state / 'canvas.sqlite3'
                with sqlite3.connect(file) as db:
                    db.execute('CREATE TABLE ' + table + ' (record TEXT)')
                    db.execute('INSERT INTO ' + table + ' VALUES (?)', (json.dumps(record),))
                before = file.read_bytes()
                with self.assertRaisesRegex(RuntimeError, 'idle backend'):
                    service.idle(self.state)
                self.assertEqual(file.read_bytes(), before)
                file.unlink()

    def test_finished_prepare_attempt_does_not_block_cutover(self) -> None:
        with sqlite3.connect(self.state / 'canvas.sqlite3') as db:
            db.execute('CREATE TABLE runtime_agents (record TEXT)')
            db.execute('INSERT INTO runtime_agents VALUES (?)',
                       (json.dumps({'status': 'paused', 'inFlight': False, 'prepareAttempt': 'saved-history'}),))
        self.assertEqual(service.idle(self.state), {'runtime_agents': 0})

    def test_idle_refuses_uncertain_input(self) -> None:
        with sqlite3.connect(self.state / 'canvas.sqlite3') as db:
            db.execute('CREATE TABLE runtime_events (status TEXT)')
            db.execute("INSERT INTO runtime_events VALUES ('uncertain')")
        with self.assertRaisesRegex(RuntimeError, 'idle backend'):
            service.idle(self.state)

    def test_linux_units_keep_supervisor_independent(self) -> None:
        units = service.linux_units(self.resources, self.state, '/usr/bin/python3', 4720,
                                    {'CODEX_HOME': '/private/profile'})
        supervisor, backend = (units[name].decode() for name in
                               ('codex-studio-supervisor.service', 'codex-studio.service'))
        for text in (supervisor, backend):
            self.assertIn('Restart=always', text)
            self.assertIn('KillMode=process', text)
            self.assertIn('CODEX_AGENTS_STATE_DIR=', text)
        self.assertIn('CODEX_AGENTS_SUPERVISOR_MODE=1', backend)
        self.assertIn('After=network-online.target codex-studio-supervisor.service', backend)
        self.assertIn('WorkingDirectory=' + str(self.resources), backend)
        self.assertNotIn('WorkingDirectory=\"', backend)
        self.assertIn('ExecStartPre=', backend)
        self.assertIn('"wait"', backend)
        self.assertIn('CODEX_HOME=/private/profile', backend)
        self.assertNotIn('PartOf=', supervisor)
        self.assertNotIn('BindsTo=', backend)
        self.assertNotIn('Requires=', backend)

    def test_service_quote_preserves_literal_paths_and_refuses_lines(self) -> None:
        self.assertEqual(service.systemd_quote('/a b/%/$/"/\\', command=True), '"/a b/%%/$$/\\"/\\\\"')
        self.assertEqual(service.systemd_quote('CODEX_HOME=/a/$home'), '"CODEX_HOME=/a/$home"')
        for value in ('a\nb', 'a\rb', 'a\0b'):
            with self.assertRaises(ValueError):
                service.systemd_quote(value)

    def test_launch_agent_keeps_children_and_correct_identity(self) -> None:
        arguments = ['/python', '-B', '/supervisor', '--state', '/state', '--wait-for-lease']
        plist = plistlib.loads(service.launch_agent('test.supervisor', arguments, self.state / 'log'))
        self.assertEqual(plist['ProgramArguments'], arguments)
        self.assertTrue(plist['AbandonProcessGroup'])
        self.assertTrue(plist['KeepAlive'])

    def test_macos_keeps_registered_owners_and_saved_environment(self) -> None:
        commands: list[list[str]] = []
        def run(args: list[str]) -> subprocess.CompletedProcess[str]:
            commands.append(args)
            return subprocess.CompletedProcess(args, 0, '')
        config = self.config | {'supervisorEnabled': True}
        with patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root):
            service.install_macos(self.resources, self.state, sys.executable, config)
        saved = json.loads((self.state / 'background-recovery.json').read_text())
        self.assertEqual(saved, config)
        self.assertEqual((self.state / 'background-recovery.json').stat().st_mode & 0o777, 0o600)
        self.assertTrue(all(command[1] == 'print' for command in commands))

    def test_active_cutover_changes_no_services(self) -> None:
        with patch.object(sys, 'platform', 'linux'), patch.object(service, 'desktop', return_value=None), \
                patch.object(service, 'idle', side_effect=RuntimeError('busy')), patch.object(service, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'busy'):
                service.enable(self.resources, self.state, 4720)
            run.assert_not_called()

    def test_ready_backend_is_idempotent_with_live_handles(self) -> None:
        current = {'pid': 123, 'supervisorMode': True, 'supervisor': {'protocol': 1}}
        with patch.object(sys, 'platform', 'linux'), patch.object(service, 'desktop', return_value=current), \
                patch.object(service, 'run', return_value=subprocess.CompletedProcess([], 0, '123')) as run, patch.object(Path, 'home', return_value=self.root), \
                patch('codex_process_supervisor.process_start_time', return_value='birth'), \
                patch('codex_process_supervisor.process_launch_command', return_value=[str(self.resources / 'scripts/codex-canvas')]), \
                patch.object(service, 'wait_ready', return_value={'pid': 456, 'handles': ['live']}), \
                patch.object(service, 'idle') as idle:
            result = service.enable(self.resources, self.state, 4720)
        self.assertTrue(result['supervisorMode'])
        idle.assert_not_called()
        self.assertFalse(any('restart' in call.args[0] for call in run.call_args_list))

    def test_first_cutover_refuses_existing_supervisor_handles(self) -> None:
        (self.state / 'supervisor.sock').touch()
        current = {'pid': 123, 'supervisorMode': False}
        with patch.object(sys, 'platform', 'linux'), patch.object(service, 'desktop', return_value=current), \
                patch.object(service, 'run', return_value=subprocess.CompletedProcess([], 0, '123')) as run, \
                patch('codex_process_supervisor.process_start_time', return_value='birth'), \
                patch('codex_process_supervisor.process_launch_command', return_value=[str(self.resources / 'scripts/codex-canvas')]), \
                patch.object(service, 'health', return_value={'handles': ['live']}):
            with self.assertRaisesRegex(RuntimeError, 'empty supervisor handle journal'):
                service.enable(self.resources, self.state, 4720)
        self.assertEqual(len(run.call_args_list), 1)
        self.assertIn('show', run.call_args.args[0])
        self.assertFalse((self.state / 'background-recovery.json').exists())

    def test_first_cutover_refuses_unmanaged_linux_backend(self) -> None:
        current = {'pid': 123, 'supervisorMode': False}
        with patch.object(sys, 'platform', 'linux'), patch.object(service, 'desktop', return_value=current), \
                patch.object(service, 'run', return_value=subprocess.CompletedProcess([], 0, '456')) as run, \
                patch('codex_process_supervisor.process_start_time', return_value='birth'), \
                patch('codex_process_supervisor.process_launch_command', return_value=[str(self.resources / 'scripts/codex-canvas')]):
            with self.assertRaisesRegex(RuntimeError, 'not owned by codex-studio.service'):
                service.enable(self.resources, self.state, 4720)
        self.assertEqual(len(run.call_args_list), 1)
        self.assertFalse((self.root / '.config/systemd/user').exists())

    def test_first_macos_setup_accepts_recovery_start_before_cutover(self) -> None:
        current = {'pid': 123, 'supervisorMode': True, 'supervisor': {'protocol': 1}}
        with patch.object(sys, 'platform', 'darwin'), \
                patch.object(service, 'desktop', side_effect=[None, current, current]), \
                patch.object(service, 'install_macos'), patch.object(service, 'wait_ready', return_value={'handles': []}), \
                patch('shutil.which', return_value='/usr/bin/codex'), patch('os.kill') as kill:
            result = service.enable(self.resources, self.state, 4720)
        self.assertTrue(result['supervisorMode'])
        kill.assert_not_called()


if __name__ == '__main__':
    unittest.main()
