#!/usr/bin/env python3
"""Exercise layr chat callers with an isolated guest and native transport."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

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
from types import ModuleType, MethodType
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('vm_fixture', Path(__file__).with_name('vm-native-fixture.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_vm_agents import flush_turns, prepare, spawn_spec, workspace_mode


class Guest(fixture.NativeGuest):
    def __init__(self, root, project):
        super().__init__(root)
        self.project = project
        self.lines = {}
        self.receipts = {}
        self.imports = 0
        self.saves = 0
        self.lost_save = False
        self.merges = 0
        self.script.write_text(fixture.FAKE_NATIVE.replace("import json,sys", "import json,sys,os")
            .replace("'linux-thread'", "'linux-'+str(os.getpid())"))

    def ensure_layr_project(self, project_id, source, owner, request_id):
        if not self.project.exists():
            shutil.copytree(source, self.project)
            self.imports += 1
        return {'projectId': project_id, 'cwd': str(self.project), 'owner': 'fixture-lead'}

    def request(self, method, params=None, **options):
        if method.startswith('provider.') or method == 'health':
            return super().request(method, params, **options)
        self.calls.append((method, copy.deepcopy(params), options))
        identity = options.get('request_id')
        if method not in {'agent.context', 'line.evidence'} and identity in self.receipts:
            return copy.deepcopy(self.receipts[identity])
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

    def test_mode_receipt_and_native_session_definition_freeze(self):
        key = str(uuid.uuid4())
        request = {'id': key, 'cwd': str(self.project), 'workspaceMode': 'worktree'}
        native = self.runtime.new_lead(request)
        self.assertEqual(self.runtime.new_lead(request)['id'], native['id'])
        with self.assertRaisesRegex(ValueError, 'different settings'):
            self.runtime.new_lead({**request, 'workspaceMode': 'layr'})
        prepared = self.runtime.prepare(native)
        original = self.runtime.tool_definitions(prepared)
        with patch('codex_runtime.TOOLS', []):
            self.assertEqual(self.runtime.tool_definitions(prepared), original)
        self.assertNotIn('host_exec', {t['name'] for t in original})

    def test_host_exec_actual_runtime_tool_binds_actor_and_replays_receipt(self):
        if not hasattr(self.host_tool, 'run_tool'):
            self.skipTest('Merge host-exec dependency, or set STUDIO_HOST_EXEC_TEST_SOURCE')
        lead = self.runtime.prepare(self.lead())
        self.assertIn('host_exec', {t['name'] for t in self.runtime.tool_definitions(lead)})
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
            'dea97695539198295d481b5f9c3b35515b1b5610:scripts/codex_runtime.py'], text=True)
        tree = ast.parse(source)
        runtime_class = next(node for node in tree.body if isinstance(node, ast.ClassDef) and node.name == 'Runtime')
        original_methods = [node for node in runtime_class.body if isinstance(node, ast.FunctionDef)
                            and node.name in {'new_thread_params', 'role_guidance', 'tool_definitions'}]
        import codex_runtime
        namespace = dict(vars(codex_runtime))
        namespace['TOOLS'] = json.loads((Path(codex_runtime.__file__).resolve().parents[1] /
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
                        'dea97695539198295d481b5f9c3b35515b1b5610:' + role_file], text=True).strip()
                    self.assertIn('\n' + original_role + '\n[End Studio role skill]', role_text)
                    read_text = Path.read_text
                    role_path = Path(codex_runtime.__file__).resolve().parents[1] / role_file
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
