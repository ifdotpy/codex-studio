"""Supervisor installation checks use private state and fake service managers."""
from __future__ import annotations

import importlib.util
import io
import json
from pathlib import Path
import plistlib
import sqlite3
import subprocess
import sys
import tempfile
from typing import Any
import unittest
from unittest.mock import patch

import codex_supervisor_service as service
from codex_layout import REPOSITORY_ROOT


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
        units = service.linux_units(self.resources, self.state, sys.executable, 4720,
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

    def rust_server(self) -> Path:
        server = self.root / 'bin/studio-runtime-server'
        server.parent.mkdir(parents=True, exist_ok=True)
        server.write_text('#!/bin/sh\n')
        server.chmod(0o755)
        return server

    def test_backend_selection_matches_the_desktop_owner(self) -> None:
        owner = (REPOSITORY_ROOT / 'workspaces/client/apps/desktop/backend.cjs').read_text()
        self.assertIn(f'const backendSelectionEnv = "{service.BACKEND_SELECTION_ENV}";', owner)
        self.assertIn('selected !== "python" && selected !== "rust"', owner)
        self.assertEqual(service.backend_selection({}), 'python')
        self.assertEqual(service.backend_selection({service.BACKEND_SELECTION_ENV: 'rust'}), 'rust')
        with self.assertRaisesRegex(ValueError, 'python or rust'):
            service.backend_selection({service.BACKEND_SELECTION_ENV: 'go'})

    def test_rust_unit_runs_the_server_without_the_python_supervisor(self) -> None:
        server = self.rust_server()
        unit = service.rust_linux_unit(self.resources, self.state, server, 4620, {
            'PATH': '/usr/bin', 'CODEX_CANVAS_PUBLIC_ORIGIN': 'https://studio.example', 'UNRELATED': 'x'}).decode()
        self.assertIn(f'ExecStart="{server}" "--port" "4620"', unit)
        self.assertIn('Environment="CODEX_CANVAS_PUBLIC_ORIGIN=https://studio.example"', unit)
        self.assertIn(f'Environment="CODEX_AGENTS_STATE_DIR={self.state}"', unit)
        self.assertNotIn('SUPERVISOR', unit)
        self.assertNotIn('UNRELATED', unit)
        self.assertNotIn('python', unit.lower())

    def test_rust_enable_refuses_a_foreign_backend_and_changes_nothing(self) -> None:
        server = self.rust_server()
        environment = {service.BACKEND_SELECTION_ENV: 'rust', 'CODEX_RUNTIME_SERVER': str(server)}
        with patch.object(sys, 'platform', 'linux'), patch.dict('os.environ', environment), \
                patch.object(service, 'desktop', return_value={'pid': 123, 'backendKind': 'rust'}), \
                patch.object(service, 'run', return_value=subprocess.CompletedProcess([], 0, '0')) as run, \
                patch.object(Path, 'home', return_value=self.root):
            with self.assertRaisesRegex(RuntimeError, 'not codex-studio.service'):
                service.enable(self.resources, self.state, 4620)
        self.assertEqual(len(run.call_args_list), 1)
        self.assertFalse((self.root / '.config/systemd/user').exists())

    def test_rust_enable_installs_and_starts_the_service(self) -> None:
        server = self.rust_server()
        environment = {service.BACKEND_SELECTION_ENV: 'rust', 'CODEX_RUNTIME_SERVER': str(server)}
        ready = {'pid': 456, 'backendKind': 'rust', 'backendBuild': 'b' * 64}
        with patch.object(sys, 'platform', 'linux'), patch.dict('os.environ', environment), \
                patch.object(service, 'desktop', side_effect=[None, ready]), \
                patch.object(service, 'run', return_value=subprocess.CompletedProcess([], 0, '')) as run, \
                patch.object(Path, 'home', return_value=self.root):
            result = service.enable(self.resources, self.state, 4620)
        self.assertEqual(result['pid'], 456)
        self.assertEqual(result['backend'], 'rust')
        unit = (self.root / '.config/systemd/user/codex-studio.service').read_text()
        self.assertIn(str(server), unit)
        self.assertFalse((self.root / '.config/systemd/user/codex-studio-supervisor.service').exists())
        self.assertIn(['systemctl', '--user', 'start', 'codex-studio.service'], [c.args[0] for c in run.call_args_list])

    def test_rust_enable_requires_a_built_server(self) -> None:
        environment = {service.BACKEND_SELECTION_ENV: 'rust', 'CODEX_RUNTIME_SERVER': str(self.root / 'missing')}
        with patch.object(sys, 'platform', 'linux'), patch.dict('os.environ', environment), \
                patch.object(service, 'run') as run:
            with self.assertRaisesRegex(ValueError, 'Build the Rust server first'):
                service.enable(self.resources, self.state, 4620)
        run.assert_not_called()

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

    def test_backend_identity_requires_the_current_checkout_launcher(self) -> None:
        current = {'pid': 123, 'supervisorMode': True, 'supervisor': {'protocol': 1}}
        for launcher, accepted in (
            ('workspaces/runtime/apps/server/src/codex-canvas', True),
            ('scripts/codex-canvas', False),
            ('elsewhere/codex-canvas', False),
        ):
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
                patch.object(service, 'run', side_effect=subprocess.CalledProcessError(113, ['print'])), \
                patch('shutil.which', return_value='/usr/bin/codex'), patch('os.kill') as kill:
            result = service.enable(self.resources, self.state, 4720)
        self.assertTrue(result['supervisorMode'])
        kill.assert_not_called()

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
        units = service.linux_units(resources, self.state, sys.executable, 4720)
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


    def rust_binaries(self) -> dict[str, str]:
        supervisor = self.root / 'bin/codex-process-supervisor'
        supervisor.parent.mkdir(parents=True, exist_ok=True)
        supervisor.write_text('#!/bin/sh\n')
        supervisor.chmod(0o755)
        return {service.BACKEND_SELECTION_ENV: 'rust', 'CODEX_RUNTIME_SERVER': str(self.rust_server()),
                'CODEX_PROCESS_SUPERVISOR': str(supervisor)}

    def fake_launchd(self, registered: dict[str, list[str]]) -> tuple[list[list[str]], Any]:
        """A launchctl stand-in keyed by job kind; registered jobs print their arguments."""
        commands: list[list[str]] = []
        labels = service._macos_labels(self.state)

        def run(arguments: list[str]) -> subprocess.CompletedProcess[str]:
            commands.append(arguments)
            if arguments[1] == 'print':
                kind = next((kind for kind, label in labels.items() if arguments[2].endswith('/' + label)), None)
                if kind not in registered:
                    raise subprocess.CalledProcessError(113, arguments)
                listed = '\n'.join('\t\t' + item for item in registered[kind])
                return subprocess.CompletedProcess(arguments, 0, f'{arguments[2]} = {{\n\targuments = {{\n{listed}\n\t}}\n}}\n')
            if arguments[1] == 'bootstrap':
                plist = plistlib.loads(Path(arguments[3]).read_bytes())
                kind = next(kind for kind, label in labels.items() if plist['Label'] == label)
                registered[kind] = plist['ProgramArguments']
                return subprocess.CompletedProcess(arguments, 0, '')
            raise AssertionError('Unexpected launchctl command: ' + ' '.join(arguments))
        return commands, run

    def python_jobs(self) -> dict[str, list[str]]:
        source = self.resources / 'workspaces/runtime/apps/server/src'
        return {'supervisor': [sys.executable, '-B', str(source / 'codex_process_supervisor.py'), '--state',
                               str(self.state), '--wait-for-lease'],
                'recovery': [sys.executable, '-B', str(self.resources / 'workspaces/client/apps/desktop/recover_backend.py'),
                             '--config', str(self.state / 'background-recovery.json')]}

    def save_config(self, **changes: Any) -> bytes:
        data = (json.dumps(self.config | changes) + '\n').encode()
        (self.state / 'background-recovery.json').write_bytes(data)
        return data

    def test_recovery_backend_selection_matches_the_recovery_owner(self) -> None:
        spec = importlib.util.spec_from_file_location(
            'recover_backend_owner', REPOSITORY_ROOT / 'workspaces/client/apps/desktop/recover_backend.py')
        assert spec and spec.loader
        owner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(owner)
        self.assertEqual(service.RECOVERY_BACKEND_SELECTIONS, owner.BACKEND_SELECTIONS)

    def test_rust_macos_installs_the_rust_supervisor_and_recovery_selection(self) -> None:
        environment = self.rust_binaries()
        commands, run = self.fake_launchd({})
        self.save_config(port=4620)
        ready = {'pid': 456, 'backendKind': 'rust', 'supervisorMode': True, 'backendBuild': 'b' * 64}
        with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', environment | {'CODEX_NODE': '/node'}), \
                patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                patch.object(service, 'desktop', side_effect=[None, ready]):
            result = service.enable(self.resources, self.state, 4620)
        self.assertEqual((result['backend'], result['pid'], result['supervisorMode']), ('rust', 456, True))
        saved = json.loads((self.state / 'background-recovery.json').read_text())
        self.assertEqual((saved['backend'], saved['supervisorEnabled'], saved['enabled']), ('rust', True, True))
        self.assertEqual(saved['runtimeServer'], environment['CODEX_RUNTIME_SERVER'])
        self.assertEqual(saved['processSupervisor'], environment['CODEX_PROCESS_SUPERVISOR'])
        self.assertEqual(saved['environment']['CODEX_NODE'], '/node')
        self.assertEqual(saved['environment']['CODEX_HOME'], '/private/profile')
        agents = self.root / 'Library/LaunchAgents'
        jobs = {plistlib.loads(path.read_bytes())['Label'].split('.')[3]: plistlib.loads(path.read_bytes())
                for path in agents.glob('*.plist')}
        self.assertEqual(jobs['supervisor']['ProgramArguments'],
                         [environment['CODEX_PROCESS_SUPERVISOR'], '--state', str(self.state), '--wait-for-lease'])
        self.assertEqual(jobs['recovery']['ProgramArguments'], self.python_jobs()['recovery'])
        self.assertTrue(all(job['KeepAlive'] and job['AbandonProcessGroup'] for job in jobs.values()))
        self.assertEqual(sorted(command[1] for command in commands), ['bootstrap'] * 2 + ['print'] * 6)

    def test_rust_macos_over_registered_python_jobs_changes_nothing(self) -> None:
        environment = self.rust_binaries()
        before = self.save_config(port=4620, supervisorEnabled=True)
        commands, run = self.fake_launchd(self.python_jobs())
        with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', environment), \
                patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                patch.object(service, 'desktop') as desktop, patch('os.kill') as kill:
            result = service.enable(self.resources, self.state, 4620)
        self.assertTrue(result['swapRequired'])
        self.assertEqual([change['job'] for change in result['changes']], ['supervisor', 'recovery'])
        self.assertTrue(any('bootout' in step for step in result['swap']))
        self.assertEqual((self.state / 'background-recovery.json').read_bytes(), before)
        self.assertFalse((self.root / 'Library/LaunchAgents').exists())
        self.assertTrue(all(command[1] == 'print' for command in commands))
        desktop.assert_not_called()
        kill.assert_not_called()

    def test_unknown_registered_supervisor_never_authorizes_rust(self) -> None:
        environment = self.rust_binaries()
        commands, run = self.fake_launchd({'supervisor': []})
        with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', environment), \
                patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                patch.object(service, 'desktop') as desktop:
            result = service.enable(self.resources, self.state, 4620)
        self.assertEqual(result['changes'], [{'job': 'supervisor', 'registered': 'unknown', 'selected': 'rust'}])
        self.assertFalse((self.state / 'background-recovery.json').exists())
        desktop.assert_not_called()

    def test_python_selection_over_registered_rust_jobs_changes_nothing(self) -> None:
        environment = self.rust_binaries()
        rust_jobs = self.python_jobs() | {'supervisor': [environment['CODEX_PROCESS_SUPERVISOR'], '--state',
                                                         str(self.state), '--wait-for-lease']}
        before = self.save_config(port=4620, supervisorEnabled=True, backend='rust',
                                  runtimeServer=environment['CODEX_RUNTIME_SERVER'],
                                  processSupervisor=environment['CODEX_PROCESS_SUPERVISOR'])
        commands, run = self.fake_launchd(rust_jobs)
        with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', {service.BACKEND_SELECTION_ENV: ''}), \
                patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                patch.object(service, 'desktop') as desktop:
            result = service.enable(self.resources, self.state, 4620)
        self.assertEqual([change['job'] for change in result['changes']], ['supervisor', 'recovery'])
        self.assertEqual((self.state / 'background-recovery.json').read_bytes(), before)
        self.assertTrue(all(command[1] == 'print' for command in commands))
        desktop.assert_not_called()

    def test_python_selection_restores_the_python_jobs_after_the_swap(self) -> None:
        environment = self.rust_binaries()
        self.save_config(port=4620, supervisorEnabled=True, backend='rust',
                         runtimeServer=environment['CODEX_RUNTIME_SERVER'],
                         processSupervisor=environment['CODEX_PROCESS_SUPERVISOR'])
        commands, run = self.fake_launchd({})
        current = {'pid': 789, 'supervisorMode': True, 'supervisor': {'protocol': 1}}
        with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', {service.BACKEND_SELECTION_ENV: 'python'}), \
                patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                patch.object(service, 'desktop', side_effect=[None, current, current]), \
                patch.object(service, 'wait_ready', return_value={'handles': []}), patch('os.kill') as kill:
            result = service.enable(self.resources, self.state, 4620)
        self.assertTrue(result['supervisorMode'])
        saved = json.loads((self.state / 'background-recovery.json').read_text())
        self.assertEqual(saved['backend'], 'python')
        self.assertNotIn('runtimeServer', saved)
        self.assertNotIn('processSupervisor', saved)
        agents = self.root / 'Library/LaunchAgents'
        jobs = {plistlib.loads(path.read_bytes())['Label'].split('.')[3]: plistlib.loads(path.read_bytes())['ProgramArguments']
                for path in agents.glob('*.plist')}
        self.assertEqual(jobs, self.python_jobs())
        kill.assert_not_called()

    def busy_canvas(self) -> None:
        with sqlite3.connect(self.state / 'canvas.sqlite3') as db:
            db.execute('CREATE TABLE runtime_agents (record TEXT)')
            db.execute('INSERT INTO runtime_agents VALUES (?)', (json.dumps({'status': 'running', 'inFlight': True}),))

    def test_rollback_after_a_failed_rust_start_does_not_require_idle_agents(self) -> None:
        # The Python supervisor served this state before the Rust attempt. Its journal and the
        # saved supervisor selection remain; the swap already ended every provider process.
        environment = self.rust_binaries()
        self.save_config(port=4620, supervisorEnabled=True, backend='rust',
                         runtimeServer=environment['CODEX_RUNTIME_SERVER'],
                         processSupervisor=environment['CODEX_PROCESS_SUPERVISOR'])
        (self.state / 'supervisor.sqlite3').touch()
        self.busy_canvas()
        commands, run = self.fake_launchd({})
        current = {'pid': 789, 'supervisorMode': True, 'supervisor': {'protocol': 1}}
        with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', {service.BACKEND_SELECTION_ENV: ''}), \
                patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                patch.object(service, 'desktop', side_effect=[None, current]), \
                patch.object(service, 'wait_ready', return_value={'handles': []}), patch('os.kill') as kill:
            result = service.enable(self.resources, self.state, 4620)
        self.assertTrue(result['supervisorMode'])
        self.assertEqual(json.loads((self.state / 'background-recovery.json').read_text())['backend'], 'python')
        self.assertIn('bootstrap', [command[1] for command in commands])
        kill.assert_not_called()

    def test_first_cutover_with_active_agents_still_refuses(self) -> None:
        self.busy_canvas()
        for supervised, journal in ((False, True), (True, False)):
            with self.subTest(supervised=supervised, journal=journal):
                before = self.save_config(port=4620, supervisorEnabled=supervised)
                (self.state / 'supervisor.sqlite3').unlink(missing_ok=True)
                if journal:
                    (self.state / 'supervisor.sqlite3').touch()
                commands, run = self.fake_launchd({})
                with patch.object(sys, 'platform', 'darwin'), \
                        patch.dict('os.environ', {service.BACKEND_SELECTION_ENV: ''}), \
                        patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                        patch.object(service, 'desktop', return_value=None):
                    with self.assertRaisesRegex(RuntimeError, 'first cutover requires an idle backend'):
                        service.enable(self.resources, self.state, 4620)
                self.assertEqual((self.state / 'background-recovery.json').read_bytes(), before)
                self.assertTrue(all(command[1] == 'print' for command in commands))

    def test_rust_macos_keeps_registered_rust_jobs(self) -> None:
        environment = self.rust_binaries()
        rust_jobs = self.python_jobs() | {'supervisor': [environment['CODEX_PROCESS_SUPERVISOR'], '--state',
                                                         str(self.state), '--wait-for-lease']}
        self.save_config(port=4620, supervisorEnabled=True, backend='rust',
                         runtimeServer=environment['CODEX_RUNTIME_SERVER'],
                         processSupervisor=environment['CODEX_PROCESS_SUPERVISOR'])
        commands, run = self.fake_launchd(rust_jobs)
        current = {'pid': 456, 'backendKind': 'rust', 'supervisorMode': True}
        with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', environment), \
                patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                patch.object(service, 'desktop', return_value=current), patch('os.kill') as kill:
            result = service.enable(self.resources, self.state, 4620)
        self.assertEqual((result['pid'], result['previousPid']), (456, 456))
        self.assertTrue(all(command[1] == 'print' for command in commands))
        self.assertFalse((self.root / 'Library/LaunchAgents').exists())
        kill.assert_not_called()

    def test_rust_macos_refuses_a_python_backend_or_missing_supervisor(self) -> None:
        environment = self.rust_binaries()
        for changes, backend, error in (
            ({}, {'pid': 123}, 'A Python backend serves this state'),
            ({'CODEX_PROCESS_SUPERVISOR': str(self.root / 'missing')}, None, 'Build the Rust supervisor first'),
        ):
            with self.subTest(error=error):
                commands, run = self.fake_launchd({})
                with patch.object(sys, 'platform', 'darwin'), patch.dict('os.environ', environment | changes), \
                        patch.object(service, 'run', run), patch.object(Path, 'home', return_value=self.root), \
                        patch.object(service, 'desktop', return_value=backend):
                    with self.assertRaisesRegex((RuntimeError, ValueError), error):
                        service.enable(self.resources, self.state, 4720)
                self.assertTrue(all(command[1] == 'print' for command in commands))
                self.assertFalse((self.state / 'background-recovery.json').exists())
                self.assertFalse((self.root / 'Library/LaunchAgents').exists())

    def test_swap_report_exits_with_its_own_status(self) -> None:
        plan = {'swapRequired': True, 'changed': False, 'changes': [], 'swap': []}
        with patch.object(service, 'enable', return_value=plan), \
                patch.object(sys, 'argv', ['codex-supervisor', 'enable', '--state', str(self.state)]), \
                patch('sys.stdout', io.StringIO()) as output:
            with self.assertRaises(SystemExit) as stopped:
                service.main()
        self.assertEqual(stopped.exception.code, service.SWAP_REQUIRED_EXIT)
        self.assertTrue(json.loads(output.getvalue())['swapRequired'])


if __name__ == '__main__':
    unittest.main()
