#!/usr/bin/env python3
"""Native Layr preparation reaches the provider with isolated fixture state."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('native_workspace_fixture', Path(__file__).with_name('workspace-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class NativeWorkspaceContract(unittest.TestCase):
    setUp = fixture.WorkspaceContract.setUp
    tearDown = fixture.WorkspaceContract.tearDown

    def test_native_workspace_move_stops_before_export(self):
        from codex_agent_move import AgentMoves
        service = Mock(runtime=self.runtime)
        moves = AgentMoves(service)
        db = Mock()
        with self.assertRaisesRegex(ValueError, 'binding stay on this server'):
            moves._idle_checks(db, {'workspaceMode': 'layr-native'}, 'move')
        db.execute.assert_not_called()

    def native(self):
        native = Mock()
        native.store, native.socket = self.root / 'store', None
        native.ensure_project.return_value = {'project': 'native-project', 'line': 'main', 'cwd': str(self.project)}
        native.command.return_value = json.dumps({'line_id': 'line-id', 'project_id': 'project-id',
                                                 'path': str(self.project), 'state': 'exact-state'})
        return native

    def test_original_folder_and_environment_reach_provider(self):
        lead = self.runtime.create({'cwd': str(self.project), 'workspaceMode': 'layr-native',
                                    'prompt': ''}, draft=True)
        with patch('codex_native_layr_agents.client', return_value=self.native()):
            prepared = self.runtime.prepare(lead)
        self.assertEqual(prepared['cwd'], str(self.project.resolve()))
        self.assertTrue(prepared['nativeLayrReady'])
        self.assertFalse(prepared['worktree'])
        calls = [params for method, params in self.runtime.server.calls if method == 'thread/start']
        self.assertEqual(len(calls), 1)
        self.assertEqual(calls[0]['cwd'], str(self.project.resolve()))
        self.assertEqual(calls[0]['config']['shell_environment_policy.set.LAYR_PROJECT'], 'native-project')
        definitions = self.runtime.tool_definitions(prepared)
        spawn = next(item for item in definitions if item['name'] == 'orchestration_spawn')
        fields = spawn['inputSchema']['properties']['agents']['items']['properties']
        self.assertEqual(fields['workspace']['enum'], ['layr-native'])
        self.assertEqual(fields['environment']['enum'], ['host'])

    def test_failed_attachment_does_not_start_provider(self):
        lead = self.runtime.create({'cwd': str(self.project), 'workspaceMode': 'layr-native',
                                    'prompt': ''}, draft=True)
        native = self.native()
        native.ensure_project.side_effect = RuntimeError('attachment outcome unknown')
        with patch('codex_native_layr_agents.client', return_value=native):
            with self.assertRaisesRegex(RuntimeError, 'attachment outcome unknown'):
                self.runtime.prepare(lead)
        self.assertIsNone(self.runtime.server)
        self.assertFalse(self.runtime.agent(lead['id']).get('nativeLayrReady'))

    def test_review_uses_the_exact_native_state(self):
        from codex_agent_review import request
        lead = self.runtime.create({'cwd': str(self.project), 'workspaceMode': 'layr-native',
                                    'prompt': ''}, draft=True)
        native = self.native()
        with patch('codex_native_layr_agents.client', return_value=native):
            prepared = self.runtime.prepare(lead)
            state = 'a' * 32
            native.command.return_value = state
            with patch.object(self.runtime, 'spawn_agents', return_value={'agents': [{'id': 'review'}]}) as spawn:
                result = request(self.runtime, prepared, {'target': {'type': 'commit', 'sha': state}}, 'review-key')
            self.assertEqual(result['stateId'], state)
            spec = spawn.call_args.args[1]['agents'][0]
            self.assertEqual(spec['role'], 'reviewer')
            self.assertEqual(spec['base_ref'], state)
            self.assertIn('layr show', spec['prompt'])
            native.command.return_value = 'b' * 32
            with patch.object(self.runtime, 'spawn_agents') as spawn:
                with self.assertRaisesRegex(ValueError, 'exact Layr state'):
                    request(self.runtime, prepared, {'target': {'type': 'commit', 'sha': state}}, 'other-key')
                spawn.assert_not_called()

    def test_worker_uses_separate_native_line(self):
        lead = self.runtime.create({'cwd': str(self.project), 'workspaceMode': 'layr-native',
                                    'prompt': ''}, draft=True)
        native = self.native()
        worker_folder = self.root / 'worker-line'
        worker_folder.mkdir()
        native.ensure_worker.return_value = {'line': 'worker-line', 'cwd': str(worker_folder)}
        with patch('codex_native_layr_agents.client', return_value=native):
            prepared = self.runtime.prepare(lead)
            result = self.runtime.spawn_agents(prepared, {'agents': [
                {'name': 'Worker', 'prompt': 'Inspect', 'base_ref': 'exact-state'}]}, 'native-worker-key')
            worker = self.runtime.agent(result['agents'][0]['id'])
            self.assertEqual(worker['workspaceMode'], 'layr-native')
            self.assertEqual(worker['layrBaseState'], 'exact-state')
            native.command.return_value = json.dumps({'line_id': 'worker-line-id', 'project_id': 'project-id',
                                                      'path': str(worker_folder), 'state': 'exact-state'})
            ready = self.runtime.prepare(worker)
        self.assertEqual(ready['cwd'], str(worker_folder.resolve()))
        self.assertNotEqual(ready['cwd'], prepared['cwd'])
        self.assertEqual(ready['nativeLayrLineId'], 'worker-line-id')
        self.assertEqual(native.ensure_worker.call_args.args[-1], 'exact-state')
        self.assertFalse(ready['worktree'])
        native.command.return_value = json.dumps({'line_id': 'worker-line-id', 'project_id': 'project-id',
                                                  'path': str(worker_folder.resolve()), 'state': 'exact-state'})
        with patch('codex_native_layr_agents.client', return_value=native):
            task = self.runtime.work_action(prepared['id'], {'action': 'create', 'title': 'Native task',
                                                            'owner': ready['id']}, actor=prepared['id'])
            submitted = self.runtime.work_action(ready['id'], {'action': 'submit', 'task_id': task['id'],
                                                               'result': 'Done', 'checks': 'Fixture check passed', 'revision': 'exact-state'}, actor=ready['id'])
            self.assertEqual(submitted['status'], 'review')
            self.assertEqual(submitted['results'][-1]['layrStateId'], 'exact-state')
            native.merge_result.side_effect = RuntimeError('merge outcome unknown')
            decision = {'action': 'accept', 'task_id': task['id'], 'result': 'Reviewed'}
            with self.assertRaisesRegex(RuntimeError, 'merge outcome unknown'):
                self.runtime.work_action(prepared['id'], decision, actor=prepared['id'])
            with self.runtime.db() as db:
                current = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?', (task['id'],)).fetchone()[0])
            self.assertEqual(current['status'], 'review')
            native.merge_result.side_effect = None
            native.merge_result.return_value = {'state': 'merged-state'}
            accepted = self.runtime.work_action(prepared['id'], decision, actor=prepared['id'])
            self.assertEqual(accepted['status'], 'accepted')
            self.assertEqual(native.merge_result.call_args.args[3], 'exact-state')

    def test_native_worker_cannot_route_to_another_server(self):
        lead = self.runtime.create({'cwd': str(self.project), 'workspaceMode': 'layr-native',
                                    'prompt': ''}, draft=True)
        with patch.object(self.runtime, 'multi_server', side_effect=AssertionError('No remote side effect')):
            with self.assertRaisesRegex(ValueError, 'chat server'):
                self.runtime.spawn_agents(lead, {'server': 'remote', 'agents': [
                    {'name': 'Worker', 'prompt': 'Inspect'}]}, 'remote-native-worker')


if __name__ == '__main__':
    unittest.main()
