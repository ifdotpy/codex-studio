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
            file = self.resources / 'workspaces/runtime/apps/server/src' / name
            file.parent.mkdir(parents=True, exist_ok=True)
            file.touch()
        recovery = self.resources / 'workspaces/client/apps/desktop/recover_backend.py'
        recovery.parent.mkdir(parents=True, exist_ok=True)
        recovery.touch()
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
        self.assertIn(str(self.resources / 'workspaces/runtime/apps/server/src/codex_process_supervisor.py'), supervisor)
        self.assertIn(str(self.resources / 'workspaces/runtime/apps/server/src/codex-supervisor'), backend)
        self.assertIn(str(self.resources / 'workspaces/runtime/apps/server/src/codex-canvas'), backend)
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
                patch('codex_process_supervisor.process_launch_command', return_value=[str(self.resources / 'workspaces/runtime/apps/server/src/codex-canvas')]), \
                patch.object(service, 'wait_ready', return_value={'pid': 456, 'handles': ['live']}), \
                patch.object(service, 'idle') as idle:
            result = service.enable(self.resources, self.state, 4720)
        self.assertTrue(result['supervisorMode'])
        idle.assert_not_called()
        self.assertFalse(any('restart' in call.args[0] for call in run.call_args_list))

    def test_backend_started_through_legacy_launcher_keeps_its_identity(self) -> None:
        current = {'pid': 123, 'supervisorMode': True, 'supervisor': {'protocol': 1}}
        for launcher, accepted in (('scripts/codex-canvas', True), ('elsewhere/codex-canvas', False)):
            with self.subTest(launcher=launcher), patch.object(sys, 'platform', 'linux'), \
                    patch.object(service, 'desktop', return_value=current), \
                    patch.object(service, 'run', return_value=subprocess.CompletedProcess([], 0, '123')), \
                    patch.object(Path, 'home', return_value=self.root), \
                    patch('codex_process_supervisor.process_start_time', return_value='birth'), \
                    patch('codex_process_supervisor.process_launch_command', return_value=[str(self.resources / launcher)]), \
                    patch.object(service, 'wait_ready', return_value={'pid': 456, 'handles': ['live']}), \
                    patch.object(service, 'idle'):
                if accepted:
                    self.assertTrue(service.enable(self.resources, self.state, 4720)['supervisorMode'])
                else:
                    with self.assertRaisesRegex(RuntimeError, 'Cannot prove the backend process identity'):
                        service.enable(self.resources, self.state, 4720)

    def test_first_cutover_refuses_existing_supervisor_handles(self) -> None:
        (self.state / 'supervisor.sock').touch()
        current = {'pid': 123, 'supervisorMode': False}
        with patch.object(sys, 'platform', 'linux'), patch.object(service, 'desktop', return_value=current), \
                patch.object(service, 'run', return_value=subprocess.CompletedProcess([], 0, '123')) as run, \
                patch('codex_process_supervisor.process_start_time', return_value='birth'), \
                patch('codex_process_supervisor.process_launch_command', return_value=[str(self.resources / 'workspaces/runtime/apps/server/src/codex-canvas')]), \
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
                patch('codex_process_supervisor.process_launch_command', return_value=[str(self.resources / 'workspaces/runtime/apps/server/src/codex-canvas')]):
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

    def test_pre_move_resources_keep_their_existing_service_launcher_paths(self) -> None:
        legacy = self.root / 'legacy'
        for name in ('codex_process_supervisor.py', 'codex-canvas', 'codex-supervisor'):
            path = legacy / 'scripts' / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch()
        units = service.linux_units(legacy, self.state, '/usr/bin/python3', 4720)
        supervisor, backend = (units[name].decode() for name in
                               ('codex-studio-supervisor.service', 'codex-studio.service'))
        self.assertIn(str(legacy / 'scripts/codex_process_supervisor.py'), supervisor)
        self.assertIn(str(legacy / 'scripts/codex-supervisor'), backend)
        self.assertIn(str(legacy / 'scripts/codex-canvas'), backend)

    def test_packaged_workspace_resources_keep_their_launcher_and_recovery_paths(self) -> None:
        resources = self.root / 'Contents/Resources/workspace'
        scripts = resources / 'scripts'
        scripts.mkdir(parents=True)
        for name in ('codex_process_supervisor.py', 'codex-canvas', 'codex-supervisor'):
            (scripts / name).touch()
        recovery = resources.parent / 'recover_backend.py'
        recovery.touch()
        self.assertEqual(service._runtime_source(resources), scripts)
        self.assertEqual(service._recovery_script(resources), recovery)
        units = service.linux_units(resources, self.state, '/usr/bin/python3', 4720)
        supervisor = units['codex-studio-supervisor.service'].decode()
        backend = units['codex-studio.service'].decode()
        self.assertIn(str(scripts / 'codex_process_supervisor.py'), supervisor)
        self.assertIn(str(scripts / 'codex-supervisor'), backend)
        self.assertIn(str(scripts / 'codex-canvas'), backend)

    def test_new_macos_launch_agents_use_relocated_source_paths(self) -> None:
        registered: set[str] = set()

        def run(arguments: list[str]) -> subprocess.CompletedProcess[str]:
            if arguments[1] == 'print':
                label = arguments[2].rsplit('/', 1)[-1]
                if label not in registered:
                    raise subprocess.CalledProcessError(113, arguments)
            elif arguments[1] == 'bootstrap':
                registered.add(Path(arguments[3]).stem)
            return subprocess.CompletedProcess(arguments, 0, '')

        config = self.config | {'supervisorEnabled': True}
        with patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root):
            service.install_macos(self.resources, self.state, sys.executable, config)

        agents = self.root / 'Library/LaunchAgents'
        jobs = [plistlib.loads(path.read_bytes()) for path in agents.glob('*.plist')]
        supervisor = next(job for job in jobs if '.supervisor.' in job['Label'])
        recovery = next(job for job in jobs if '.recovery.' in job['Label'])
        self.assertIn(str(self.resources / 'workspaces/runtime/apps/server/src/codex_process_supervisor.py'),
                      supervisor['ProgramArguments'])
        self.assertIn(str(self.resources / 'workspaces/client/apps/desktop/recover_backend.py'),
                      recovery['ProgramArguments'])


if __name__ == '__main__':
    unittest.main()
