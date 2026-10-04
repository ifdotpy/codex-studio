#!/usr/bin/env python3
"""Image workspace caller state, permissions, recovery, and failure contracts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import subprocess
import sys
import tempfile
import types
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location(
    'workspace_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ImageWorkspaceRuntime(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='image-workspace-runtime-')
        self.root = Path(self.temp.name)
        self.repo = self.root / 'repo'
        (self.repo / 'project').mkdir(parents=True)
        self.repo = self.repo.resolve()
        subprocess.run(['git', 'init', '-q', str(self.repo)], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'config', 'user.name', 'Fixture'], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'config', 'user.email', 'fixture@example.test'], check=True)
        (self.repo / 'project' / 'tracked.txt').write_text('base\n')
        subprocess.run(['git', '-C', str(self.repo), 'add', '.'], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'commit', '-qm', 'base'], check=True)
        self.rt = fixture.ControlledRuntime(self.root / 'state', fixture.f.FakeServer)
        self.rt.catalog = lambda account='default': copy.deepcopy(fixture.CATALOG)
        self.lead = self.rt.new_lead({'cwd': str(self.repo)})

    def tearDown(self):
        self.rt.close()
        self.temp.cleanup()

    def spawn(self, name='Worker'):
        return self.rt.spawn_agents(self.rt.agent(self.lead['id']), {'agents': [{
            'name': name, 'prompt': 'Edit one file', 'cwd': str(self.repo / 'project')} ]},
            'image-spawn-' + name)['agents'][0]

    def test_multi_switch_starts_the_supported_repository_base(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        started = Mock(return_value={'state': 'building'})
        self.rt.start_image_base = started
        self.rt.conversation_settings(self.lead['id'], {
            'agent_mode': 'single', 'expected_mode_revision': 0, 'request_id': 'mode-single'})
        self.rt.conversation_settings(self.lead['id'], {
            'agent_mode': 'multi', 'expected_mode_revision': 1, 'request_id': 'mode-multi'})
        started.assert_called_once_with(str(self.repo))

    def test_spawn_starts_read_only_even_in_yolo_until_ready_notice(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        record = self.rt.agent(worker)
        self.assertTrue(record['imageWorkspace'])
        self.assertEqual(record['imageWorkspacePhase'], 'read_only')
        self.assertEqual(record['cwd'], str(self.repo / 'project'))
        self.assertEqual(self.rt.turn_permissions(record)['sandboxPolicy'], {'type': 'readOnly'})
        with self.rt.db() as db:
            text = db.execute('SELECT text FROM runtime_events WHERE id=?',
                              (worker + ':initial',)).fetchone()[0]
        self.assertIn('read-only until Studio sends a workspace-ready notice', text)

    def test_claude_initial_thread_stays_read_only_when_yolo_is_enabled(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.rt.agent(self.spawn()['id'])
        worker.update(provider='claude', yoloMode=True)
        params = self.rt.new_thread_params(worker)
        self.assertEqual(params['approvalPolicy'], 'on-request')
        self.assertEqual(params['sandbox'], 'read-only')

    def test_codex_protocol_switches_from_read_only_to_the_image_path(self):
        eventually = fixture.f.eventually
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker_id = self.spawn()['id']
        with self.rt.lock, self.rt.db() as db:
            worker = self.rt.agent(worker_id, db)
            worker['yoloMode'] = False
            self.rt.put(db, 'agents', worker)
        self.rt.prepare(self.rt.agent(worker_id))
        self.rt.dispatch()
        eventually(lambda: any(method == 'turn/start' and params['threadId'] ==
                               self.rt.agent(worker_id)['threadId']
                               for method, params in self.rt.server.calls))
        first = [params for method, params in self.rt.server.calls if method == 'turn/start'
                 and params['threadId'] == self.rt.agent(worker_id)['threadId']][-1]
        self.assertEqual(first['sandboxPolicy'], {'type': 'readOnly'})
        eventually(lambda: self.rt.agent(worker_id)['turnId'] is not None)
        self.rt.server.complete(self.rt.agent(worker_id)['threadId'],
                                self.rt.agent(worker_id)['turnId'])
        eventually(lambda: not self.rt.agent(worker_id)['inFlight'])
        mount = self.root / 'protocol-mount'
        project = mount / 'repo' / 'project'
        project.mkdir(parents=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'repoPath': str(mount / 'repo'),
            'branch': 'codex-agent/' + worker_id, 'startCommit': 'abc',
            'snapshotCommit': 'snapshot'})
        engine.ensure_mounted = Mock(return_value={
            'mount': str(mount), 'repoPath': str(mount / 'repo')})
        engine.exec_prefix = Mock(return_value=[])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            self.rt.image_base_completed(worker_id, {'state': 'ready'})
            self.rt.dispatch()
            eventually(lambda: len([params for method, params in self.rt.server.calls
                                    if method == 'turn/start' and params['threadId'] ==
                                    self.rt.agent(worker_id)['threadId']]) >= 2)
            notice_turn = [params for method, params in self.rt.server.calls if method == 'turn/start'
                           and params['threadId'] == self.rt.agent(worker_id)['threadId']][-1]
            resume = [params for method, params in self.rt.server.calls if method == 'thread/resume'
                      and params['threadId'] == self.rt.agent(worker_id)['threadId']][-1]
            self.assertEqual(resume['cwd'], str(project))
            self.assertEqual(notice_turn['cwd'], str(project))
            self.assertEqual(notice_turn['sandboxPolicy']['type'], 'workspaceWrite')
            self.assertIn(str(project), notice_turn['input'][0]['text'])

    def test_ready_inherited_permissions_allow_writes_only_in_the_mount(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.rt.agent(self.spawn()['id'])
        worker.update(cwd=str(self.root / 'mount'), imageWorkspaceReady=True,
                      imageWorkspacePhase='ready', yoloMode=None)
        policy = self.rt.turn_permissions(worker)
        self.assertEqual(policy['approvalPolicy'], 'on-request')
        self.assertEqual(policy['sandboxPolicy']['type'], 'workspaceWrite')
        self.assertEqual(policy['sandboxPolicy']['writableRoots'], [worker['cwd']])

    def test_linux_provider_process_falls_back_when_namespaces_are_unavailable(self):
        from codex_runtime import provider_process_command
        with patch('sys.platform', 'linux'), \
                patch('codex_workspace_images.exec_prefix', side_effect=RuntimeError('unshare denied')):
            self.assertEqual(provider_process_command(['provider']), ['provider'])
        with patch('sys.platform', 'linux'), \
                patch('codex_workspace_images.exec_prefix', return_value=['missing-nsenter', '--']), \
                patch('shutil.which', return_value=None):
            self.assertEqual(provider_process_command(['provider']), ['provider'])

    def test_base_failure_switches_to_the_existing_worktree_fallback(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        with patch.object(self.rt.pool, 'submit') as submit:
            self.rt.image_base_completed(worker, {'state': 'failed', 'error': 'disk full'})
        record = self.rt.agent(worker)
        self.assertFalse(record['imageWorkspace'])
        self.assertTrue(record['worktree'])
        self.assertEqual(record['imageWorkspacePhase'], 'fallback')
        self.assertIn('disk full', record['imageWorkspaceError'])
        self.assertIsNone(record['error'])
        submit.assert_called_once()

    def test_ready_callback_creates_writable_mount_and_one_notice(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        mount = self.root / 'mount'
        project = mount / 'repo' / 'project'
        project.mkdir(parents=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'repoPath': str(mount / 'repo'),
            'branch': 'codex-agent/' + worker, 'startCommit': 'abc',
            'snapshotCommit': 'snap'})
        engine.exec_prefix = Mock(return_value=['nsenter', '--'])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch.object(self.rt, '_send_image_workspace_notice') as notice, \
                patch('codex_runtime.subprocess.run', return_value=types.SimpleNamespace(
                    returncode=0)) as path_check:
            self.rt.image_base_completed(worker, {'state': 'ready'})
        path_check.assert_called_once_with(
            ['nsenter', '--', 'test', '-d', str(project)], capture_output=True, timeout=10)
        record = self.rt.agent(worker)
        self.assertEqual(record['cwd'], str(project))
        self.assertTrue(record['imageWorkspaceReady'])
        self.assertEqual(record['branch'], 'codex-agent/' + worker)
        self.assertEqual(record['imageWorkspaceSnapshotCommit'], 'snap')
        self.assertNotEqual(self.rt.turn_permissions(record)['sandboxPolicy'], {'type': 'readOnly'})
        notice.assert_called_once()
        self.assertIn(str(project), notice.call_args.args[1])
        self.assertIn('snap', notice.call_args.args[1])

    def test_callback_replay_after_switch_does_not_duplicate_workspace_or_notice(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        mount = self.root / 'mount-replay'
        image_repo = mount / 'repo'
        subprocess.run(['git', 'clone', '-q', '--shared', str(self.repo), str(image_repo)], check=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'repoPath': str(image_repo),
            'branch': 'codex-agent/' + worker, 'startCommit': 'abc',
            'snapshotCommit': None})
        engine.exec_prefix = Mock(return_value=[])
        engine.remove_workspace = Mock()
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch.object(self.rt, '_send_image_workspace_notice') as notice:
            self.rt.image_base_completed(worker, {'state': 'ready'})
            self.rt.image_base_completed(worker, {'state': 'ready'})
        self.assertTrue(self.rt.agent(worker)['imageWorkspaceReady'])
        engine.create_workspace.assert_called_once()
        engine.remove_workspace.assert_not_called()
        notice.assert_called_once()

    def test_restart_prepare_remounts_a_ready_workspace(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        mount = self.root / 'mount'
        image_repo = mount / 'repo'
        subprocess.run(['git', 'clone', '-q', '--shared', str(self.repo), str(image_repo)], check=True)
        project = image_repo / 'project'
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(cwd=str(project), imageWorkspaceReady=True,
                          imageWorkspacePhase='ready', imageWorkspaceMount=str(mount),
                          imageWorkspaceNoticeSent='image-workspace-ready:' + worker)
            self.rt.put(db, 'agents', record)
        engine = types.ModuleType('codex_workspace_images')
        engine.ensure_mounted = Mock(return_value={'mount': str(mount), 'repoPath': str(mount / 'repo')})
        engine.exec_prefix = Mock(return_value=[])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            self.rt.prepare(self.rt.agent(worker))
        engine.ensure_mounted.assert_called_once_with(worker)

    def test_restart_prepare_rejoins_base_build_during_read_only_phase(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        self.rt.start_image_base.reset_mock()
        self.rt.prepare(self.rt.agent(worker))
        self.rt.start_image_base.assert_called_once_with(str(self.repo), worker)

    def test_child_spawn_maps_parent_image_path_to_the_user_repository(self):
        worker = self.spawn()['id']
        actor = self.rt.agent(worker)
        mount = self.root / 'parent-image'
        image_repo = mount / 'repo'
        child_folder = image_repo / 'project' / 'child'
        child_folder.mkdir(parents=True)
        actor.update(imageWorkspaceReady=True, imageWorkspaceRepo=str(self.repo))
        engine = types.ModuleType('codex_workspace_images')
        engine.ensure_mounted = Mock(return_value={'repoPath': str(image_repo)})
        engine.exec_prefix = Mock(return_value=['nsenter', '--'])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch('codex_runtime.git_toplevel', return_value=str(image_repo)) as detect:
            repo, directory, prefix = self.rt.worker_spawn_repository(actor, str(child_folder))
        self.assertEqual(repo, str(self.repo))
        self.assertEqual(directory, str(self.repo / 'project' / 'child'))
        self.assertEqual(prefix, ['nsenter', '--'])
        detect.assert_called_once_with(str(child_folder), prefix=['nsenter', '--'])

    def test_collect_conflict_is_saved_in_the_worker_result(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(imageWorkspaceReady=True, imageWorkspacePhase='ready')
            self.rt.put(db, 'agents', record)
        engine = types.ModuleType('codex_workspace_images')
        engine.collect = Mock(return_value={'conflict': 'tracked.txt',
                                            'rawRef': 'refs/studio/agents/' + worker + '/raw'})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch.object(self.rt, '_capture_reserved_checkpoint'):
            self.rt.checkpoint_after_turn(worker, 'turn-1', 'checkpoint-1')
        record = self.rt.agent(worker)
        self.assertEqual(record['imageWorkspaceCollect']['conflict'], 'tracked.txt')
        self.assertIn('tracked.txt', record['imageWorkspaceError'])
        with self.rt.db() as db:
            notice = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='child_result'",
                                (self.lead['id'],)).fetchone()[0]
        self.assertIn('tracked.txt', notice)
        self.assertIn('refs/studio/agents/' + worker + '/raw', notice)

    def test_unsupported_platform_uses_git_worktree(self):
        self.rt.image_workspace_support = lambda _repo: (False, 'unsupported platform')
        worker = self.spawn()
        self.assertEqual(worker['workspace'], 'worktree')
        record = self.rt.agent(worker['id'])
        self.assertFalse(record['imageWorkspace'])
        self.assertTrue(record['worktree'])

    def test_archive_collect_remove_and_restore_saved_image_branch(self):
        from codex_agent_management import _cleanup_image_workspace, manage_agent
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        image_mount = self.root / 'image-to-archive'
        image_repo = image_mount / 'repo'
        image_repo.parent.mkdir(parents=True)
        subprocess.run(['git', 'clone', '-q', '--shared', str(self.repo), str(image_repo)], check=True)
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(cwd=str(image_repo), imageWorkspaceReady=True,
                          imageWorkspacePhase='ready', imageWorkspaceMount=str(image_mount))
            self.rt.put(db, 'agents', record)
        subprocess.run(['git', '-C', str(self.repo), 'branch',
                        'codex-agent/' + worker], check=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.collect = Mock(return_value={'branch': 'codex-agent/' + worker})
        engine.remove_workspace = Mock(return_value={'freedBytes': 123, 'state': 'removed'})
        engine.workspace_bytes = Mock(return_value=123)
        engine.ensure_mounted = Mock(return_value={'mount': str(image_mount), 'repoPath': str(image_repo)})
        engine.exec_prefix = Mock(return_value=[])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            cleanup = _cleanup_image_workspace(self.rt, worker)
        self.assertEqual(cleanup['state'], 'removed')
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(deletedAt=11, epoch=1, status='paused', autoWake=False,
                          agentArchive={'at': 11, 'epoch': 1, 'cleanupPending': False})
            self.rt.put(db, 'agents', record)
        restored_mount = self.root / 'restored'
        restored_repo = restored_mount / 'repo'
        subprocess.run(['git', 'clone', '-q', '--shared', str(self.repo), str(restored_repo)], check=True)
        engine.create_workspace = Mock(return_value={
            'mount': str(restored_mount), 'repoPath': str(restored_repo),
            'branch': 'codex-agent/' + worker, 'snapshotCommit': None})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            result = manage_agent(self.rt, self.lead['id'],
                                  {'action': 'restore', 'agent_id': worker}, 0)
        self.assertEqual(result['status'], 'restored')
        record = self.rt.agent(worker)
        self.assertTrue(record['imageWorkspaceReady'])
        self.assertEqual(record['branch'], 'codex-agent/' + worker)

    def test_maintenance_and_disk_reports_include_workspace_and_base_bytes(self):
        from codex_agent_management import worktree_maintenance_report
        from codex_worktree_disk import WorktreeDiskScanner
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(imageWorkspaceReady=True, imageWorkspacePhase='ready')
            self.rt.put(db, 'agents', record)
            lead = self.rt.agent(self.lead['id'], db)
            lead['imageWorkspaceBaseRepo'] = str(self.repo)
            self.rt.put(db, 'agents', lead)
        engine = types.ModuleType('codex_workspace_images')
        engine.workspace_bytes = Mock(return_value=40)
        engine.base_bytes = Mock(return_value=20)
        engine.list_workspaces = Mock(return_value=[])
        engine.base_status = Mock(return_value={'state': 'ready', 'version': 'v1', 'error': None})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            disk = WorktreeDiskScanner(self.rt.root).snapshot()
            maintenance = worktree_maintenance_report(self.rt, self.lead['id'], self.lead['epoch'])
        self.assertEqual(disk['workers'][worker]['bytes'], 40)
        self.assertEqual(disk['baseBytes'], 20)
        self.assertEqual(disk['storageBytes'], 60)
        self.assertEqual(maintenance['bases'][0]['state'], 'ready')
        self.assertEqual(maintenance['bases'][0]['bytes'], 20)


if __name__ == '__main__':
    unittest.main(verbosity=2)
