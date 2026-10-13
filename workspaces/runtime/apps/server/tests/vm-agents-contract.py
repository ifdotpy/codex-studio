#!/usr/bin/env python3
"""Exercise layr chat callers with an isolated guest and native transport."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import base64
import copy
import json
import ast
import importlib.util
import os
from pathlib import Path
import shutil
import sys
import subprocess
import tempfile
import time
from types import ModuleType, MethodType
import unittest
from unittest.mock import patch
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
spec = importlib.util.spec_from_file_location('vm_fixture', Path(__file__).with_name('vm-native-fixture.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_layout import REPOSITORY_ROOT
from codex_vm_agents import flush_turns, prepare, spawn_spec, workspace_mode
import codex_linux_vm_share as share
from codex_linux_vm import LinuxVMError


class Guest(fixture.NativeGuest):
    def __init__(self, root, project):
        super().__init__(root)
        self.project = project
        self.lines = {}
        self.receipts = {}
        self.imports = 0
        self.saves = 0
        self.lost_save = False
        # A guest whose line.save fails for good: 'old' (it runs `layr layer repair`) or a code.
        self.save_failure = None
        self.error_receipts = {}
        self.exec_argv = []
        self.merges = 0
        self.state_dir = root / 'vm-state'
        self.during_import = []
        self.fail_import = False
        self.readiness = lambda: None
        self.script.write_text(fixture.FAKE_NATIVE.replace("import json,sys", "import json,sys,os")
            .replace("'linux-thread'", "'linux-'+str(os.getpid())"))

    def reboot(self):
        """A guest reboot: the providers end with the old kernel, and the new boot's
        supervisor recovers its journal before the guest service accepts requests."""
        from codex_process_supervisor import Journal, Supervisor
        old = self.supervisor
        for child in old.children.values():
            child.process.kill()
            child.process.wait(timeout=5)
            self.close_streams(child)
        old.journal.close()
        self.supervisor = Supervisor(old.root)
        self.supervisor.journal = Journal(self.supervisor.root)
        self.supervisor.recover_orphaned_children()

    def ensure_layr_project(self, project_id, source, owner, request_id, started=None):
        # The host records of import_project, under its project lock, for the sidebar state.
        directory = self.state_dir / 'layr-projects' / project_id
        with share._project_lock(directory):
            if started:
                started()
            self.during_import.append(started and self.readiness())
            if self.fail_import:
                raise ConnectionError('The import upload was lost')
            if not self.project.exists():
                shutil.copytree(source, self.project)
                self.imports += 1
            (directory / 'project.json').write_text(json.dumps({'projectId': project_id, 'source': source}))
        return {'projectId': project_id, 'cwd': str(self.project), 'owner': 'fixture-lead'}

    def request(self, method, params=None, **options):
        if method.startswith('provider.') or method == 'health':
            return super().request(method, params, **options)
        self.calls.append((method, copy.deepcopy(params), options))
        identity = options.get('request_id')
        if identity in self.error_receipts:
            raise self.error_receipts[identity]
        if method not in {'agent.context', 'line.evidence'} and identity in self.receipts:
            return copy.deepcopy(self.receipts[identity])
        if method == 'line.save' and self.save_failure:
            # The guest keeps failures in its receipts as well.
            code = 'operation_failed' if self.save_failure == 'old' else self.save_failure
            self.error_receipts[identity] = LinuxVMError('Linux guest line.save', code=code)
            raise self.error_receipts[identity]
        if method in {'line.bind', 'line.branch'}:
            agent = params['agentId']
            if method == 'line.bind':
                line, cwd, owner = 'main', self.project, 'fixture-lead'
            else:
                line, cwd, owner = agent, self.project.parent / agent, 'fixture-worker'
                if not cwd.exists():
                    shutil.copytree(self.project, cwd)
            result = {'agentId': agent, 'projectId': params['projectId'], 'line': line,
                      'cwd': str(cwd), 'path': str(cwd), 'home': str(self.project.parent / owner),
                      'owner': owner, 'stateId': str(uuid.uuid4()), 'readOnly': params.get('readOnly', False)}
            self.lines[agent] = result
        elif method == 'agent.context':
            result = self.lines[params['agentId']]
        elif method == 'line.save':
            self.saves += 1
            result = {'stateId': str(uuid.uuid4()), 'turnId': params['turnId']}
        elif method == 'line.evidence':
            result = self.lines[params['agentId']]
            if params['stateId'] != result['stateId']:
                raise ValueError('The submitted state is not the line head')
        elif method == 'line.merge':
            source = self.lines[params['sourceAgentId']]
            if source['stateId'] != params['expectedStateId']:
                raise ValueError('The reviewed state changed')
            self.merges += 1
            shutil.copyfile(Path(source['cwd']) / 'file', self.project / 'file')
            result = {'stateId': source['stateId'], 'reviewedStateId': source['stateId']}
        elif method == 'line.remove':
            line = self.lines.pop(params['agentId'])
            shutil.rmtree(line['cwd'])
            result = {'state': 'removed'}
        elif method == 'layr.credentials.sync':
            result = {'synced': True}
        elif method == 'layr.exec':
            self.exec_argv.append(params['argv'])
            stdout = b'state-from-exec\n' if params['argv'][1] == 'rev-parse' else b''
            result = {'exitCode': 0, 'stdout': base64.b64encode(stdout).decode(), 'stderr': '', 'reason': None}
        else:
            raise AssertionError(method)
        if method not in {'agent.context', 'line.evidence'}:
            self.receipts[identity] = copy.deepcopy(result)
        if method == 'line.save' and self.lost_save:
            self.lost_save = False
            raise ConnectionError('The save response was lost')
        return copy.deepcopy(result)


class VmAgents(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='vm-agents-')
        self.root = Path(self.temp.name).resolve()
        self.project = self.root / 'source'
        self.project.mkdir()
        (self.project / 'file').write_text('base')
        self.runtime = fixture.f.ControlledRuntime(self.root / 'state', fixture.f.f.FakeServer)
        self.runtime.catalog = lambda account='default': copy.deepcopy(fixture.f.CATALOG)
        self.guest = Guest(self.root, self.root / 'guest-main')
        self.runtime._linux_vm_client = self.guest
        self.addCleanup(patch.stopall)
        patch('codex_vm_agents.platform.system', return_value='Darwin').start()
        patch('codex_linux_vm_credentials.sync_credentials', return_value='credential-digest').start()
        patch('codex_linux_vm_auth.bootstrap_codex').start()
        host_source = os.environ.get('STUDIO_HOST_EXEC_TEST_SOURCE')
        if host_source:
            host_spec = importlib.util.spec_from_file_location('codex_host_exec', host_source)
            tool = importlib.util.module_from_spec(host_spec)
            host_spec.loader.exec_module(tool)
        else:
            try:
                import codex_host_exec as tool
            except ImportError:
                tool = ModuleType('codex_host_exec')
                tool.tool_definition = lambda: {'name': 'host_exec', 'description': 'fixture', 'inputSchema': {'type': 'object'}}
        self.host_tool = tool
        patch.dict(sys.modules, {'codex_host_exec': tool}).start()

    def tearDown(self):
        self.runtime.close()
        self.guest.close()
        self.temp.cleanup()

    def lead(self):
        return self.runtime.new_lead({'cwd': str(self.project), 'workspaceMode': 'layr'})

    def worker(self, lead, reviewer=False):
        result = self.runtime.spawn_agents(lead, {'agents': [{
            'name': 'Worker', 'prompt': 'Change the file', 'role': 'reviewer' if reviewer else 'implementer'}]},
            'spawn:' + str(uuid.uuid4()))
        return self.runtime.agent(result['agents'][0]['id'])

    def test_lead_and_worker_native_processes_use_the_guest(self):
        lead = self.runtime.prepare(self.lead())
        self.assertEqual(lead['executionMode'], 'vm')
        self.assertEqual(lead['workspaceMode'], 'layr')
        self.assertEqual(lead['cwd'], str(self.guest.project))
        self.assertEqual(lead['hostProjectPath'], str(self.project))
        worker = self.runtime.prepare(self.worker(lead))
        reviewer = self.runtime.prepare(self.worker(lead, reviewer=True))
        self.assertNotEqual(worker['layrLine'], lead['layrLine'])
        self.assertTrue(self.guest.lines[reviewer['id']]['readOnly'])
        self.assertNotIn('host_exec', {t['name'] for t in self.runtime.tool_definitions(reviewer)})
        self.assertEqual(self.runtime.turn_permissions(reviewer)['sandboxPolicy']['type'], 'readOnly')
        self.assertEqual(len(self.guest.starts), 3)
        for start in self.guest.starts:
            self.assertTrue(start['layr'])
            self.assertEqual(start['transport'], 'native')
            self.assertNotIn('refreshToken', str(start))
        second = self.runtime.prepare(self.lead())
        self.assertEqual(second['layrProjectId'], lead['layrProjectId'])
        self.assertEqual(self.guest.imports, 1)

    def sidebar_readiness(self):
        """The layr state in the project entity that the sidebar receives."""
        with self.runtime.read_db() as db:
            row = db.execute("SELECT payload FROM sync_entities WHERE collection='project' AND id=?",
                             (str(self.project),)).fetchone()
        return json.loads(row[0])['value'].get('layrReadiness')

    def test_sidebar_shows_the_import_while_the_first_layr_chat_prepares(self):
        self.guest.readiness = self.sidebar_readiness
        lead = self.lead()
        self.assertEqual(self.sidebar_readiness(), {'state': 'not_imported'})
        self.guest.fail_import = True
        with self.assertRaises(ConnectionError):
            prepare(self.runtime, lead)
        # The attempt ended: the sidebar no longer shows it in progress.
        self.assertEqual(self.guest.during_import, [{'state': 'importing'}])
        self.assertEqual(self.sidebar_readiness(), {'state': 'not_imported'})
        self.guest.fail_import = False
        prepare(self.runtime, self.runtime.agent(lead['id']))
        self.assertEqual(self.guest.during_import[-1], {'state': 'importing'})
        from codex_vm_agents import project_layr_id
        key = project_layr_id({'id': str(self.project), 'path': str(self.project)})
        self.assertEqual(self.sidebar_readiness(), {'state': 'ready', 'projectId': key})

    def test_the_chip_shows_only_while_the_project_has_a_layr_chat(self):
        from codex_vm_agents import project_layr_id
        lead = prepare(self.runtime, self.lead())
        key = project_layr_id({'id': str(self.project), 'path': str(self.project)})
        self.assertEqual(self.sidebar_readiness(), {'state': 'ready', 'projectId': key})
        # An imported project without a layr chat here shows no LAYR chip.
        self.runtime.delete_conversation(lead['id'])
        self.assertIsNone(self.sidebar_readiness())

    def test_restart_replaces_the_importing_state_of_the_previous_process(self):
        from codex_vm_agents import project_layr_id, refresh_readiness
        self.lead()
        key = project_layr_id({'id': str(self.project), 'path': str(self.project)})
        with share._project_lock(self.guest.state_dir / 'layr-projects' / key):
            refresh_readiness(self.runtime)
            self.assertEqual(self.sidebar_readiness(), {'state': 'importing'})
        # The process stops during the import: nothing refreshes the saved state.
        self.assertEqual(self.sidebar_readiness(), {'state': 'importing'})
        self.runtime.close()
        with patch('codex_linux_workspaces.client', return_value=self.guest):
            self.runtime = fixture.f.ControlledRuntime(self.root / 'state', fixture.f.f.FakeServer)
        self.assertEqual(self.sidebar_readiness(), {'state': 'not_imported'})

    def test_turn_save_loss_keeps_one_snapshot_and_blocks_next_turn(self):
        lead = prepare(self.runtime, self.lead())
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(lead['id'], db)
            current['layrTurnSaves'] = {'turn-1': {'status': 'pending'}}
            self.runtime.put(db, 'agents', current)
        self.guest.lost_save = True
        with self.assertRaises(ConnectionError):
            prepare(self.runtime, self.runtime.agent(lead['id']))
        flush_turns(self.runtime, self.runtime.agent(lead['id']))
        self.assertEqual(self.guest.saves, 1)
        self.assertEqual(self.runtime.agent(lead['id'])['layrTurnSaves']['turn-1']['status'], 'saved')

    def pending_turn(self):
        lead = prepare(self.runtime, self.lead())
        with self.runtime.lock, self.runtime.db() as db:
            current = self.runtime.agent(lead['id'], db)
            current['layrTurnSaves'] = {'turn-1': {'status': 'pending'}}
            self.runtime.put(db, 'agents', current)
        return lead

    def test_an_old_guest_saves_the_turn_as_the_line_owner(self):
        # Guests before 2026-10-12 run `layr layer repair`, which layr no longer has.
        lead = self.pending_turn()
        self.guest.save_failure = 'old'
        flush_turns(self.runtime, self.runtime.agent(lead['id']))
        saved = self.runtime.agent(lead['id'])
        self.assertEqual(saved['layrTurnSaves']['turn-1'], {'status': 'saved', 'stateId': 'state-from-exec'})
        self.assertEqual(saved['layrStateId'], 'state-from-exec')
        self.assertEqual(self.guest.exec_argv, [['layr', 'cache', 'repair'],
                                                ['layr', 'save', '--force', '-m', 'Studio turn turn-1'],
                                                ['layr', 'rev-parse', 'HEAD@{0}']])

    def test_a_definite_save_failure_retries_with_the_next_attempt(self):
        lead = self.pending_turn()
        self.guest.save_failure = 'invalid_params'
        with self.assertRaises(LinuxVMError):
            flush_turns(self.runtime, self.runtime.agent(lead['id']))
        self.assertEqual(self.runtime.agent(lead['id'])['layrTurnSaves']['turn-1'], {'status': 'pending', 'attempt': 1})
        # The guest is fixed. The first ID would replay its saved failure; the next attempt runs.
        self.guest.save_failure = None
        flush_turns(self.runtime, self.runtime.agent(lead['id']))
        self.assertEqual(self.runtime.agent(lead['id'])['layrTurnSaves']['turn-1']['status'], 'saved')
        saves = [options['request_id'] for method, _, options in self.guest.calls if method == 'line.save']
        self.assertEqual(saves, ['layr-turn:' + lead['id'] + ':turn-1', 'layr-turn:' + lead['id'] + ':turn-1:1'])
        self.assertEqual(self.guest.exec_argv, [])

    def test_vm_restart_ends_providers_shows_a_notice_and_reconnects_without_resending(self):
        # docs/linux-vm-workspaces.md, "Guest crash watchdog": the watchdog stopped a dead guest.
        from codex_linux_vm_watchdog import drop_providers, post_notices
        lead = self.runtime.prepare(self.lead())
        old = self.runtime.linux_servers[lead['id']]
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(lead['id'], db)
            agent.update(status='running', inFlight=True, turnId='turn-1')
            self.runtime.put(db, 'agents', agent)
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       ('event-1', lead['id'], 'user', 'Build it', 'dispatching', time.time(),
                        agent['epoch'], None, None))
        record = {'id': 'restart-1', 'at': time.time(), 'reason': 'Kernel panic - not syncing: test'}
        record['chats'] = drop_providers(self.runtime, record)
        self.assertEqual(record['chats'], [lead['id']])
        deadline = time.monotonic() + 10
        while self.runtime.agent(lead['id'])['status'] != 'interrupted' and time.monotonic() < deadline:
            time.sleep(0.05)
        agent = self.runtime.agent(lead['id'])
        self.assertEqual((agent['status'], agent['inFlight']), ('interrupted', False))
        self.assertEqual(agent['error'], 'Linux VM provider disconnected. The turn outcome is unknown. '
                                         'Review the transcript before resuming.')
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='event-1'").fetchone()[0],
                             'uncertain')
        record.update(outcome='restarted', finishedAt=time.time())
        post_notices(self.runtime, record)
        post_notices(self.runtime, record)
        with self.runtime.read_db() as db:
            notices = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_items WHERE agent=? AND id=?",
                (lead['id'], lead['id'] + ':linux-vm-restart:restart-1'))]
        self.assertEqual(len(notices), 1)
        self.assertEqual(notices[0]['nativeNotice'], 'error')
        self.assertIn('Studio restarted it', notices[0]['text'])
        # The new boot: the next message opens a new provider and sends nothing again.
        self.guest.reboot()
        calls = len(self.guest.calls)
        server = self.runtime.connect_agent(self.runtime.agent(lead['id']))
        self.assertIsNot(server, old)
        self.assertEqual(len(self.guest.starts), 2)
        self.assertNotIn('Build it', json.dumps(self.guest.calls[calls:]))
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='event-1'").fetchone()[0],
                             'uncertain')

    def test_submission_accepts_exact_state_and_refuses_changed_head(self):
        lead = prepare(self.runtime, self.lead())
        worker = prepare(self.runtime, self.worker(lead))
        (Path(worker['cwd']) / 'file').write_text('worker')
        task = self.runtime.work_action(lead['id'], {'action': 'create', 'title': 'Task', 'owner': worker['id']},
                                        actor=lead['id'])
        revision = self.guest.lines[worker['id']]['stateId']
        submitted = self.runtime.work_action(worker['id'], {'action': 'submit', 'task_id': task['id'],
            'result': 'Changed file', 'checks': 'Fake guest', 'revision': revision, 'files': ['file']},
            actor=worker['id'])
        self.assertEqual(submitted['results'][-1]['layrStateId'], revision)
        self.guest.lines[worker['id']]['stateId'] = str(uuid.uuid4())
        args = {'action': 'accept', 'task_id': task['id'], 'result': 'Reviewed exact state'}
        with self.assertRaisesRegex(ValueError, 'reviewed state'):
            self.runtime.work_action(lead['id'], args, actor=lead['id'])
        self.assertEqual(self.guest.merges, 0)
        self.guest.lines[worker['id']]['stateId'] = revision
        accepted = self.runtime.work_action(lead['id'], args, actor=lead['id'])
        self.assertEqual(accepted['status'], 'accepted')
        self.assertEqual(self.guest.merges, 1)
        self.assertEqual((self.guest.project / 'file').read_text(), 'worker')
        self.assertEqual((self.project / 'file').read_text(), 'base')

    def test_http_creation_returns_workspace_and_execution_mode(self):
        from fastapi import FastAPI
        from fastapi.testclient import TestClient
        from studio_api.context import ApiContext
        from studio_api.agents.router import create_router
        context = ApiContext.for_schema()
        context.canvas.runtime = self.runtime
        app = FastAPI()
        app.include_router(create_router(context))
        with TestClient(app) as client:
            response = client.post('/api/leads', json={'cwd': str(self.project), 'workspaceMode': 'layr'})
            self.assertEqual(response.status_code, 200, response.text)
            self.assertEqual(response.json()['workspaceMode'], 'layr')
            self.assertEqual(response.json()['executionMode'], 'vm')

    def test_only_http_creation_applies_the_macos_layr_default(self):
        from fastapi import FastAPI
        from fastapi.testclient import TestClient
        from studio_api.context import ApiContext
        from studio_api.agents.router import create_router
        context = ApiContext.for_schema()
        context.canvas.runtime = self.runtime
        app = FastAPI()
        app.include_router(create_router(context))
        with patch('codex_vm_agents.platform.system', return_value='Darwin'):
            with TestClient(app) as client:
                response = client.post('/api/leads', json={'cwd': str(self.project)})
                self.assertEqual(response.status_code, 200, response.text)
                self.assertEqual(response.json()['workspaceMode'], 'layr')
            internal = self.runtime.new_lead({'id': str(uuid.uuid4()), 'cwd': str(self.project), 'reuse_empty': False})
            self.assertEqual((internal['workspaceMode'], internal['executionMode']), ('worktree', 'native'))

    def test_turn_tick_selects_only_vm_agents_with_unsaved_turns(self):
        from codex_vm_agents import tick
        base = self.runtime.new_lead({'cwd': str(self.project), 'workspaceMode': 'worktree'})
        records = {
            'vm-due': {'executionMode': 'vm', 'layrTurnSaves': {'t1': {'status': 'saved'}, 't2': {'status': 'pending'}}},
            'vm-saved': {'executionMode': 'vm', 'layrTurnSaves': {'t1': {'status': 'saved'}}},
            'vm-later': {'executionMode': 'vm', 'layrTurnRetryAt': time.time() + 600,
                         'layrTurnSaves': {'t1': {'status': 'failed'}}},
            'native': {'executionMode': 'native', 'layrTurnSaves': {'t1': {'status': 'pending'}}},
        }
        with self.runtime.lock, self.runtime.db() as db:
            for key, fields in records.items():
                self.runtime.put(db, 'agents', {**copy.deepcopy(base), 'id': key, **fields})
        selected = []
        class Pool:
            def submit(self, function):
                function()
        with patch('codex_vm_agents.flush_turns', lambda runtime, agent: selected.append(agent['id'])), \
             patch.object(self.runtime, 'pool', Pool()):
            tick(self.runtime)
        self.assertEqual(selected, ['vm-due'])

    def test_mode_receipt_and_native_tool_source(self):
        key = str(uuid.uuid4())
        request = {'id': key, 'cwd': str(self.project), 'workspaceMode': 'worktree'}
        native = self.runtime.new_lead(request)
        self.assertEqual(self.runtime.new_lead(request)['id'], native['id'])
        with self.assertRaisesRegex(ValueError, 'different settings'):
            self.runtime.new_lead({**request, 'workspaceMode': 'layr'})
        prepared = self.runtime.prepare(native)
        # A new native thread keeps a small source marker, not a copy of the tool list,
        # so its tools follow the provider and lead filters on every call, as before layr chats.
        stored = self.runtime.agent(native['id'])
        self.assertEqual(stored.get('nativeToolSource'), 'current')
        self.assertNotIn('nativeToolDefinitions', stored)
        original = self.runtime.tool_definitions(prepared)
        self.assertNotIn('host_exec', {t['name'] for t in original})
        self.assertIn('orchestration_review', {t['name'] for t in original})
        claude = self.runtime.tool_definitions({**prepared, 'provider': 'claude'})
        self.assertNotIn('orchestration_review', {t['name'] for t in claude})

    def test_host_exec_actual_runtime_tool_binds_actor_and_replays_receipt(self):
        if not hasattr(self.host_tool, 'run_tool'):
            self.skipTest('Merge host-exec dependency, or set STUDIO_HOST_EXEC_TEST_SOURCE')
        lead = self.runtime.prepare(self.lead())
        self.assertIn('host_exec', {t['name'] for t in self.runtime.tool_definitions(lead)})
        # Codex rejects dynamic tools that mix the canonical and legacy formats.
        self.assertEqual({tuple(sorted(t)) for t in self.runtime.tool_definitions(lead)},
                         {('description', 'inputSchema', 'name', 'type')})
        calls = []
        def stream(method, params, **options):
            calls.append((method, copy.deepcopy(params), options))
            yield {'event': 'output', 'data': {'text': 'host proof'}}
            yield {'result': {'operationId': params['operationId'], 'state': 'exited', 'exitCode': 0}}
        self.guest.stream = stream
        message = {'id': 91, 'method': 'item/tool/call', 'params': {
            'threadId': lead['threadId'], 'callId': 'host-test', 'tool': 'host_exec',
            'arguments': {'action': 'execute', 'command': 'pwd', 'request_id': 'host-proof'}}}
        self.runtime.reply = unittest.mock.Mock()
        connection = self.runtime.agent_connection(lead)
        self.runtime.dynamic(message, 'default', connection)
        result = self.runtime.reply.call_args.args[0]['result']
        self.assertTrue(result['success'], result)
        self.assertEqual(calls[0][0], 'host.exec')
        self.assertEqual(calls[0][1]['agentId'], lead['id'])
        self.assertEqual(calls[0][1]['cwd'], lead['cwd'])
        self.runtime.dynamic(message, 'default', connection)
        self.assertEqual(len(calls), 1)
        reviewer = self.runtime.prepare(self.worker(lead, reviewer=True))
        for action in ('execute', 'status', 'cancel'):
            denied = copy.deepcopy(message)
            denied['params'].update(threadId=reviewer['threadId'], callId='reviewer-' + action)
            denied['params']['arguments']['action'] = action
            self.runtime.dynamic(denied, 'default', self.runtime.agent_connection(reviewer))
            self.assertFalse(self.runtime.reply.call_args.args[0]['result']['success'])
            self.assertEqual(len(calls), 1)

    def test_vm_monitors_use_guest_shell_without_mac_sdk_paths(self):
        from codex_shell import monitor_command
        command = monitor_command(None, 'pwd', '/guest/line', config={'allow_login_shell': False}, guest=True)
        self.assertEqual(command, ['/bin/bash', '-c', 'pwd'])

    def test_vm_rejects_host_spawn_and_linux_creation(self):
        lead = prepare(self.runtime, self.lead())
        with self.assertRaises(ValueError):
            spawn_spec(lead, {'environment': 'host'})
        with self.assertRaises(ValueError):
            spawn_spec(lead, {'workspace': 'worktree'})
        self.runtime.multi_server = unittest.mock.Mock(side_effect=AssertionError('No remote caller'))
        for args in ({'server': 'remote', 'agents': []}, {'agents': [{'server': 'remote'}]}):
            with self.assertRaisesRegex(ValueError, 'chat server'):
                self.runtime.spawn_agents(lead, args, 'vm-remote-rejected')
        with patch('codex_vm_agents.platform.system', return_value='Linux'):
            self.assertEqual(workspace_mode({}, creation=True), 'worktree')
            for mode in ('layr', 'image'):
                with self.assertRaises(ValueError):
                    workspace_mode({'workspaceMode': mode}, creation=True)

    def test_existing_lead_worker_codex_claude_instructions_keep_exact_bytes(self):
        # Execute the original constructor, not a second copy of the new code.
        source = subprocess.check_output(['git', 'show',
            'dd9c89e12835f4ce2ae1f795259d6655f8e33e3c:workspaces/runtime/apps/server/src/codex_runtime.py'], text=True)
        tree = ast.parse(source)
        runtime_class = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == 'Runtime')
        original_methods = [node for node in runtime_class.body if isinstance(node, ast.FunctionDef)
                            and node.name in {'new_thread_params', 'role_guidance', 'tool_definitions'}]
        import codex_runtime
        namespace = dict(vars(codex_runtime))
        namespace['TOOLS'] = json.loads((REPOSITORY_ROOT /
            '.agents/skills/codex-workspace/references/native-tools.json').read_text())
        exec(compile(ast.Module(body=original_methods, type_ignores=[]), '<pre-vm-runtime>', 'exec'), namespace)
        before = namespace['new_thread_params']
        lead = self.runtime.new_lead({'cwd': str(self.project), 'workspaceMode': 'worktree'})
        for provider in ('codex', 'claude'):
            for is_lead in (True, False):
                with self.subTest(provider=provider, is_lead=is_lead):
                    actor = {**lead, 'threadId': 'existing-thread', 'isLead': is_lead,
                             'role': 'orchestrator' if is_lead else 'implementer', 'provider': provider}
                    role_text = self.runtime.role_guidance(actor)
                    from codex_session_tools import session_tool_name, LEGACY_MOVE
                    role_name = 'codex-orchestrator' if is_lead else 'codex-subagent'
                    role_file = '.agents/skills/' + role_name + ('/references/legacy-move-role.md'
                        if is_lead and session_tool_name(actor) == LEGACY_MOVE else '/SKILL.md')
                    original_role = subprocess.check_output(['git', 'show',
                        'dd9c89e12835f4ce2ae1f795259d6655f8e33e3c:' + role_file], text=True).strip()
                    self.assertIn('\n' + original_role + '\n[End Studio role skill]', role_text)
                    read_text = Path.read_text
                    role_path = REPOSITORY_ROOT / role_file
                    def historical_text(path, *args, **kwargs):
                        return original_role if path == role_path else read_text(path, *args, **kwargs)
                    with patch.object(Path, 'read_text', historical_text), \
                         patch.object(self.runtime, 'role_guidance', namespace['role_guidance'].__func__), \
                         patch.object(self.runtime, 'tool_definitions', MethodType(namespace['tool_definitions'], self.runtime)):
                        original = before(self.runtime, copy.deepcopy(actor))
                    actual = self.runtime.new_thread_params(actor)
                    self.assertEqual(actual['developerInstructions'].encode(), original['developerInstructions'].encode())
                    self.assertEqual(actual['dynamicTools'], original['dynamicTools'])


if __name__ == '__main__':
    unittest.main()
