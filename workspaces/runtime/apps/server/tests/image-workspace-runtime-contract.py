#!/usr/bin/env python3
"""Image workspace caller state, permissions, recovery, and failure contracts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import concurrent.futures
import importlib.util
import shutil
import subprocess
import shutil
import sys
import tempfile
import threading
import types
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location(
    'workspace_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ImageWorkspaceRuntime(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.repo_template_temp = tempfile.TemporaryDirectory(prefix='image-workspace-repo-template-')
        cls.addClassCleanup(cls.repo_template_temp.cleanup)
        cls.repo_template = Path(cls.repo_template_temp.name) / 'repo'
        (cls.repo_template / 'project').mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', str(cls.repo_template)], check=True)
        subprocess.run(['git', '-C', str(cls.repo_template), 'config', 'user.name', 'Fixture'], check=True)
        subprocess.run(['git', '-C', str(cls.repo_template), 'config', 'user.email', 'fixture@example.test'], check=True)
        # The repo is copied by parallel tests. Background Git maintenance can
        # create/remove .git/objects/maintenance.lock during copytree.
        subprocess.run(['git', '-C', str(cls.repo_template), 'config', 'gc.auto', '0'], check=True)
        subprocess.run(['git', '-C', str(cls.repo_template), 'config', 'maintenance.auto', 'false'], check=True)
        (cls.repo_template / 'project' / 'tracked.txt').write_text('base\n')
        subprocess.run(['git', '-C', str(cls.repo_template), 'add', '.'], check=True)
        subprocess.run(['git', '-C', str(cls.repo_template), 'commit', '-qm', 'base'], check=True)

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='image-workspace-runtime-')
        self.root = Path(self.temp.name)
        self.repo = self.root / 'repo'
        shutil.copytree(self.repo_template, self.repo)
        self.repo = self.repo.resolve()
        self.rt = fixture.ControlledRuntime(self.root / 'state', fixture.f.FakeServer)
        self.rt.catalog = lambda account='default': copy.deepcopy(fixture.CATALOG)
        self.addCleanup(patch.stopall)
        patch('codex_worker_workspace.platform.system', return_value='Darwin').start()
        self.lead = self.rt.new_lead({'cwd': str(self.repo), 'workspaceMode': 'image'})

    def tearDown(self):
        self.rt.close()
        self.temp.cleanup()

    def spawn(self, name='Worker', **options):
        return self.rt.spawn_agents(self.rt.agent(self.lead['id']), {'agents': [{
            'name': name, 'prompt': 'Edit one file', 'cwd': str(self.repo / 'project'),
            **options} ]},
            'image-spawn-' + name)['agents'][0]




    def test_multi_switch_defers_repository_base_until_first_worker(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        engine = types.ModuleType('codex_workspace_images')
        def start(repo, on_done=None, retry_failed=False):
            self.assertFalse(self.rt.lock._is_owned())
            return {'state': 'building'}
        engine.start_base_build = Mock(side_effect=start)
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            self.rt.conversation_settings(self.lead['id'], {
                'agent_mode': 'single', 'expected_mode_revision': 0, 'request_id': 'mode-single'})
            self.rt.conversation_settings(self.lead['id'], {
                'agent_mode': 'multi', 'expected_mode_revision': 1, 'request_id': 'mode-multi'})
            engine.start_base_build.assert_not_called()
            worker = self.spawn()['id']
        self.assertEqual(self.rt.agent(worker)['imageWorkspacePhase'], 'read_only')
        self.assertEqual(self.rt.agent(worker)['imageWorkspaceRepo'], str(self.repo))
        self.assertEqual(engine.start_base_build.call_count, 2)
        first, callback = engine.start_base_build.call_args_list
        self.assertEqual((first.args, first.kwargs),
                         ((str(self.repo),), {'on_done': None, 'retry_failed': False}))
        self.assertEqual(callback.args, (str(self.repo),))
        self.assertTrue(callable(callback.kwargs['on_done']))
        self.assertFalse(callback.kwargs['retry_failed'])

    def test_start_image_base_passes_retry_failed_to_engine(self):
        engine = types.ModuleType('codex_workspace_images')
        engine.start_base_build = Mock(return_value={'state': 'building'})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            result = self.rt.start_image_base(str(self.repo), retry_failed=True)
        self.assertEqual(result, {'state': 'building'})
        engine.start_base_build.assert_called_once_with(str(self.repo), on_done=None, retry_failed=True)

    def test_start_image_base_does_not_retry_failed_build_by_default(self):
        engine = types.ModuleType('codex_workspace_images')
        engine.start_base_build = Mock(return_value={'state': 'failed'})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            result = self.rt.start_image_base(str(self.repo))
        self.assertEqual(result, {'state': 'failed'})
        engine.start_base_build.assert_called_once_with(str(self.repo), on_done=None,
                                                        retry_failed=False)

    def test_spawn_starts_image_base_after_transaction_and_lock(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        calls = []

        def start(repo, agent_id=None, **options):
            calls.append((agent_id, self.rt.lock._is_owned(), options))
            if agent_id:
                self.assertIsNotNone(self.rt.agent(agent_id))
            return {'state': 'building'}

        self.rt.start_image_base = start
        self.spawn()
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0], (None, False, {}))
        self.assertIsNotNone(calls[1][0])
        self.assertEqual(calls[1][1:], (False, {}))

    def test_default_image_workspace_input_describes_copy_with_uncommitted_changes(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        spawned = self.spawn()
        worker = spawned['id']
        self.assertEqual(spawned['workspaceState'], 'building')
        with self.rt.db() as db:
            text = db.execute('SELECT text FROM runtime_events WHERE id=?',
                              (worker + ':initial',)).fetchone()[0]
        self.assertIn('copy of ' + str(self.repo), text)
        self.assertIn('uncommitted changes', text)
        self.assertIn('End this turn if no more read-only work remains.', text)
        self.assertIn('When the workspace-ready notice arrives, follow its instruction', text)
        self.assertNotIn('[Studio worker base]', text)

    def test_spawn_starts_read_only_even_in_yolo_until_ready_notice(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        record = self.rt.agent(worker)
        self.assertTrue(record['imageWorkspace'])
        self.assertEqual(record['imageWorkspacePhase'], 'read_only')
        self.assertEqual(record['cwd'], str(self.repo / 'project'))
        self.assertEqual(self.rt.turn_permissions(record), {
            'approvalPolicy': 'never', 'sandboxPolicy': {
                'type': 'workspaceWrite',
                'writableRoots': [self.rt.image_workspace_temp(record)],
                'networkAccess': False}})
        with self.rt.db() as db:
            text = db.execute('SELECT text FROM runtime_events WHERE id=?',
                              (worker + ':initial',)).fetchone()[0]
        self.assertIn('This turn is read-only.', text)
        self.assertIn('Studio sends a workspace-ready notice when the copy is ready.', text)
        self.assertIn('Read the source folder at ' + record['cwd'] + ' by its absolute path', text)

    def test_folder_outside_git_gets_an_image_when_supported(self):
        folder = self.root / 'plain-folder'
        folder.mkdir()
        self.rt.image_workspace_support = lambda root: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        result = self.rt.spawn_agents(self.rt.agent(self.lead['id']), {'agents': [{
            'name': 'Plain folder', 'prompt': 'Edit files', 'cwd': str(folder)}]},
            'plain-folder-spawn')['agents'][0]
        record = self.rt.agent(result['id'])
        self.assertEqual(result['workspace'], 'image')
        self.assertTrue(record['imageWorkspace'])
        self.assertFalse(record['worktree'])
        self.assertEqual(record['imageWorkspaceRepo'], str(folder.resolve()))

    def test_folder_outside_git_uses_original_folder_when_images_are_unsupported(self):
        folder = self.root / 'plain-folder-unsupported'
        folder.mkdir()
        self.rt.image_workspace_support = lambda _root: (False, 'unsupported')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        result = self.rt.spawn_agents(self.rt.agent(self.lead['id']), {'agents': [{
            'name': 'Plain folder fallback', 'prompt': 'Edit files', 'cwd': str(folder)}]},
            'plain-folder-fallback')['agents'][0]
        record = self.rt.agent(result['id'])
        self.assertEqual(result['workspace'], 'shared')
        self.assertFalse(record['imageWorkspace'])
        self.assertFalse(record['worktree'])
        self.assertEqual(record['cwd'], str(folder.resolve()))

    def test_worker_result_includes_copy_path_source_time_and_uncommitted_state(self):
        worker_id = self.spawn()['id']
        with self.rt.lock, self.rt.db() as db:
            worker = self.rt.agent(worker_id, db)
            worker.update(imageWorkspace=True, imageWorkspaceReady=True,
                          workspaceMode='image', imageWorkspaceError=None, imageWorkspaceBaseState='ready',
                          imageWorkspaceRepo=str(self.repo),
                          imageWorkspaceCreatedAt=123.0, cwd='/image/repo')
            self.rt.put(db, 'agents', worker)
            self.rt.parent_event(db, worker, 'turn-1', 'Done')
            event = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='child_result'",
                               (self.lead['id'],)).fetchone()[0]
        result = __import__('json').loads(event)
        self.assertEqual(result['workspace'], {
            'mode': 'image', 'path': '/image/repo', 'source': str(self.repo),
            'state': 'ready',
            'takenAt': '1970-01-01T00:02:03Z', 'includesUncommittedChanges': True})

    def test_child_results_include_mode_and_path_for_shared_and_worktree(self):
        for mode in ('shared', 'worktree'):
            worker_id = self.spawn('Result ' + mode, workspace=mode)['id']
            path = str(self.root / (mode + '-path'))
            with self.rt.lock, self.rt.db() as db:
                worker = self.rt.agent(worker_id, db)
                worker.update(cwd=path, worktree=(mode == 'worktree'),
                              workspaceMode=mode, worktreeReady=(mode == 'worktree'))
                self.rt.put(db, 'agents', worker)
                self.rt.parent_event(db, worker, 'result-' + mode, 'Done')
                event = db.execute(
                    "SELECT text FROM runtime_events WHERE agent=? AND kind='child_result' ORDER BY created DESC LIMIT 1",
                    (self.lead['id'],),
                ).fetchone()[0]
            result = __import__('json').loads(event)
            self.assertEqual(result['workspace'], {'mode': mode, 'path': path})

    def test_startup_restarts_all_read_only_image_builds_after_releasing_lock(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        workers = [self.spawn(name)['id'] for name in ('Restart one', 'Restart two')]
        self.rt.close()

        def start(runtime, repo, agent_id, *, retry_failed=False):
            self.assertFalse(runtime.lock._is_owned())
            self.assertEqual(repo, str(self.repo))
            self.assertIn(agent_id, workers)

        with patch.object(fixture.f.Runtime, 'start_image_base', autospec=True,
                          side_effect=start) as resumed:
            self.rt = fixture.ControlledRuntime(self.root / 'state', fixture.f.FakeServer)
        self.assertCountEqual([call.args[1:] for call in resumed.call_args_list],
                              [(str(self.repo), worker) for worker in workers])

    def test_child_result_shows_read_only_wait_and_fallback_error(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker_id = self.spawn()['id']
        waiting = self.rt.image_workspace_summary(self.rt.agent(worker_id))
        self.assertEqual(waiting['state'], 'building')

        with self.rt.lock, self.rt.db() as db:
            worker = self.rt.agent(worker_id, db)
            worker.update(imageWorkspace=False, imageWorkspacePhase='fallback',
                          imageWorkspaceBaseState='failed', imageWorkspaceError='disk full',
                          workspaceMode='worktree')
            self.rt.parent_event(db, worker, 'turn-1', 'Stopped')
            event = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='child_result'",
                               (self.lead['id'],)).fetchone()[0]
        result = __import__('json').loads(event)
        self.assertEqual(result['workspace']['state'], 'failed')
        self.assertEqual(result['workspace']['error'], 'disk full')
        self.assertEqual(result['workspace']['mode'], 'worktree')
        self.assertEqual(result['workspace']['path'], str(self.repo / 'project'))

    def test_claude_initial_thread_stays_read_only_when_yolo_is_enabled(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.rt.agent(self.spawn()['id'])
        worker.update(provider='claude', yoloMode=True)
        params = self.rt.new_thread_params(worker)
        self.assertEqual(params['approvalPolicy'], 'never')
        self.assertEqual(params['sandbox'], 'read-only')
        self.assertEqual(params['claude']['permissionMode'], 'plan')
        self.assertEqual(params['studioImageWorkspaceTempDir'],
                         self.rt.image_workspace_temp(worker))
        worker.update(imageWorkspaceReady=True, cwd=str(self.root / 'image-copy'))
        ready = self.rt.new_thread_params(worker)
        self.assertIsNone(ready['studioImageWorkspaceTempDir'])

    def test_codex_initial_thread_uses_read_only_without_approval_wait(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.rt.agent(self.spawn()['id'])
        worker.update(yoloMode=True)
        params = self.rt.new_thread_params(worker)
        self.assertEqual(params['approvalPolicy'], 'never')
        self.assertEqual(params['sandbox'], 'workspace-write')
        self.assertEqual(params['cwd'], self.rt.image_workspace_temp(worker))
        self.assertNotEqual(params['cwd'], worker['cwd'])

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
        temp_dir = self.rt.image_workspace_temp(self.rt.agent(worker_id))
        self.assertEqual(first['cwd'], temp_dir)
        self.assertEqual(first['sandboxPolicy'], {
            'type': 'workspaceWrite', 'writableRoots': [temp_dir],
            'networkAccess': False})
        eventually(lambda: self.rt.agent(worker_id)['turnId'] is not None)
        initial_turn = self.rt.agent(worker_id)['turnId']
        thread_id = self.rt.agent(worker_id)['threadId']
        server = self.rt.server
        second_turn_started = threading.Event()
        original_notify = server._notify

        def notify_after_second_turn(message):
            original_notify(message)
            params = message.get('params', {})
            turn = params.get('turn', {})
            if (message.get('method') == 'turn/started' and
                    params.get('threadId') == thread_id and
                    turn.get('id') != initial_turn):
                second_turn_started.set()

        server._notify = notify_after_second_turn
        self.rt.server.complete(thread_id, initial_turn)
        eventually(lambda: not self.rt.agent(worker_id)['inFlight'])
        mount = self.root / 'protocol-mount'
        project = mount / 'repo' / 'project'
        project.mkdir(parents=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
        engine.ensure_mounted = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
        engine.exec_prefix = Mock(return_value=[])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            self.rt.image_base_completed(worker_id, {'state': 'ready'})
            self.rt.dispatch()
            self.assertTrue(second_turn_started.wait(30),
                            'ready image base did not start the resumed turn')
            notice_turn = [params for method, params in self.rt.server.calls if method == 'turn/start'
                           and params['threadId'] == self.rt.agent(worker_id)['threadId']][-1]
            resume = [params for method, params in self.rt.server.calls if method == 'thread/resume'
                      and params['threadId'] == self.rt.agent(worker_id)['threadId']][-1]
            self.assertEqual(resume['cwd'], str(project))
            self.assertEqual(notice_turn['cwd'], str(project))
            self.assertEqual(notice_turn['sandboxPolicy']['type'], 'workspaceWrite')
            self.assertIn(str(project), notice_turn['input'][0]['text'])

    def active_workspace_handoff(self, provider, lose_reply=False):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker_id = self.spawn(provider + ' active')['id']
        with self.rt.lock, self.rt.db() as db:
            worker = self.rt.agent(worker_id, db)
            worker.update(provider=provider, yoloMode=False)
            self.rt.put(db, 'agents', worker)
        with patch('codex_claude_auth_wait.require_auth'):
            self.rt.prepare(self.rt.agent(worker_id))
            self.rt.dispatch(worker_id)
            fixture.f.eventually(lambda: self.rt.delivery_receipt(
                worker_id + ':initial')['status'] == 'delivered')
        initial = self.rt.agent(worker_id)
        turn_id = initial['turnId']
        initial_params = [p for m, p in self.rt.server.calls if m == 'turn/start'][-1]
        initial_policy = copy.deepcopy(initial_params['sandboxPolicy'])
        self.assertNotIn(str(self.repo / 'project'), initial_policy.get('writableRoots', []))
        if provider == 'claude':
            self.assertEqual(initial_policy['type'], 'readOnly')
        mount = self.root / (provider + '-active-mount')
        project = mount / 'repo' / 'project'
        project.mkdir(parents=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
        engine.ensure_mounted = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
        engine.exec_prefix = Mock(return_value=[])
        handoff_id = 'image-workspace-handoff:' + worker_id
        ready_id = 'image-workspace-ready:' + worker_id
        original_call = self.rt.server.call
        effective = {'cwd': initial_params['cwd'], 'policy': initial_policy}

        def call(method, params, timeout=60):
            if method == 'turn/start':
                # Both providers keep the active turn's execution context.
                if params['threadId'] not in self.rt.server.active_turns:
                    effective.update(cwd=params['cwd'], policy=params['sandboxPolicy'])
                if lose_reply and params.get('clientUserMessageId') == handoff_id:
                    original_call(method, params, timeout)
                    raise RuntimeError('response timed out; outcome unknown')
            return original_call(method, params, timeout)

        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch.object(self.rt.server, 'call', side_effect=call):
            self.rt.image_base_completed(worker_id, {'state': 'ready'})
            self.rt.dispatch(worker_id)
            fixture.f.eventually(lambda: self.rt.delivery_receipt(handoff_id) is not None)
            fixture.f.eventually(lambda: self.rt.delivery_receipt(handoff_id)['status'] ==
                                 ('uncertain' if lose_reply else 'delivered'))
            self.assertEqual(self.rt.agent(worker_id)['turnId'], turn_id)
            self.assertEqual(effective, {'cwd': initial_params['cwd'], 'policy': initial_policy})
            self.assertEqual(self.rt.delivery_receipt(ready_id)['status'], 'pending')
            with self.rt.db() as db:
                text = db.execute('SELECT text FROM runtime_events WHERE id=?', (handoff_id,)).fetchone()[0]
            self.assertIn('Your copy is ready at ' + str(project), text)
            self.assertIn('End this turn now', text)
            self.assertNotIn('Write access is enabled', text)
            self.rt.image_base_completed(worker_id, {'state': 'ready'})
            self.rt._send_image_workspace_notice(worker_id, 'changed callback text', ready_id)
            self.rt.dispatch(worker_id)
            self.assertEqual(sum(m == 'turn/start' and p.get('clientUserMessageId') == handoff_id
                                 for m, p in self.rt.server.calls), 1)
            if lose_reply:
                # Simulate restart after both event commits, before the final marker.
                with self.rt.lock, self.rt.db() as db:
                    record = self.rt.agent(worker_id, db)
                    record.pop('imageWorkspaceNoticeSent', None)
                    self.rt.put(db, 'agents', record)
                self.rt.close()
                self.rt = fixture.ControlledRuntime(self.root / 'state', fixture.f.FakeServer)
                self.rt.catalog = lambda account='default': copy.deepcopy(fixture.CATALOG)
                self.rt._send_image_workspace_notice(worker_id, 'changed callback text', ready_id)
                self.rt.dispatch(worker_id)
                self.assertEqual(self.rt.delivery_receipt(handoff_id)['status'], 'uncertain')
                self.assertIsNone(self.rt.server)
                with self.rt.db() as db:
                    self.assertEqual(db.execute('SELECT count(*) FROM runtime_events WHERE id=?',
                                                (handoff_id,)).fetchone()[0], 1)
                    self.assertEqual(db.execute('SELECT text FROM runtime_events WHERE id=?',
                                                (handoff_id,)).fetchone()[0], text)
                return
            self.rt.server.complete(initial['threadId'], turn_id)
            fixture.f.eventually(lambda: not self.rt.agent(worker_id)['inFlight'])
            with patch('codex_claude_auth_wait.require_auth'):
                self.rt.dispatch(worker_id)
                fixture.f.eventually(lambda: self.rt.delivery_receipt(ready_id)['status'] == 'delivered')
            self.assertEqual(effective['cwd'], str(project))
            self.assertEqual(effective['policy']['type'], 'workspaceWrite')
            self.assertEqual(effective['policy']['writableRoots'], [str(project)])
            self.assertNotEqual(self.rt.agent(worker_id)['turnId'], turn_id)
            engine.create_workspace.assert_called_once()

    def test_codex_ready_notice_reaches_active_turn_once_then_switches_copy(self):
        self.active_workspace_handoff('codex')

    def test_claude_ready_notice_reaches_active_turn_once_then_switches_copy(self):
        self.active_workspace_handoff('claude')

    def test_restart_during_ready_notice_delivery_keeps_unknown_without_replay(self):
        self.active_workspace_handoff('codex', lose_reply=True)

    def test_ready_base_during_initial_preparation_preserves_thread_and_input(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.schedule_fast_dispatch = lambda *args, **kwargs: None
        mount = self.root / 'race-mount'
        project = mount / 'repo' / 'project'
        project.mkdir(parents=True)
        engine = types.ModuleType('codex_workspace_images')
        callbacks = []
        def start(repo, on_done=None, retry_failed=False):
            if on_done:
                callbacks.append(on_done)
            return {'state': 'ready'}
        engine.start_base_build = Mock(side_effect=start)
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
        engine.ensure_mounted = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
        engine.exec_prefix = Mock(return_value=[])
        native = concurrent.futures.Future()
        requested = threading.Event()
        self.rt.connect()
        original_submit = self.rt.server.submit
        def submit(method, params):
            if method == 'thread/start':
                self.rt.server.calls.append((method, copy.deepcopy(params)))
                requested.set()
                return native
            return original_submit(method, params)
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch.object(self.rt.server, 'submit', side_effect=submit):
            worker = self.spawn('Ready preparation race')['id']
            self.rt.dispatch(worker)
            self.assertTrue(requested.wait(3))
            operation = self.rt.preparations[worker]
            # The base engine calls this immediately when its base is ready.
            callbacks[0]({'state': 'ready'}).result(3)
            callbacks[0]({'state': 'ready'}).result(3)
            self.assertFalse(self.rt.agent(worker)['imageWorkspaceReady'])
            engine.create_workspace.assert_not_called()
            native.set_result({'thread': {'id': 'race-thread'}})
            self.assertEqual(operation['future'].result(3)['threadId'], 'race-thread')
            fixture.f.eventually(lambda: self.rt.agent(worker)['imageWorkspaceReady'])
            fixture.f.eventually(lambda: self.rt.delivery_receipt(worker + ':initial')['status'] == 'delivered')
            record = self.rt.agent(worker)
            self.assertIsNone(record['error'])
            self.assertEqual(record['threadId'], 'race-thread')
            self.rt.server.complete('race-thread', record['turnId'])
            fixture.f.eventually(lambda: not self.rt.agent(worker)['inFlight'])
            self.rt.dispatch(worker)
            fixture.f.eventually(lambda: self.rt.delivery_receipt(
                'image-workspace-ready:' + worker)['status'] == 'delivered')
            starts = [params for method, params in self.rt.server.calls if method == 'thread/start']
            resumes = [params for method, params in self.rt.server.calls if method == 'thread/resume']
            turns = [params for method, params in self.rt.server.calls if method == 'turn/start']
            self.assertEqual(len(starts), 1)
            self.assertEqual(resumes[-1]['threadId'], 'race-thread')
            self.assertEqual(resumes[-1]['cwd'], str(project))
            self.assertEqual(turns[-1]['cwd'], str(project))
            self.assertEqual(sum(p['clientUserMessageId'] == worker + ':initial' for p in turns), 1)
            engine.create_workspace.assert_called_once()

    def test_base_exception_during_preparation_waits_without_native_replay(self):
        from codex_runtime import PreparationPending
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.schedule_fast_dispatch = lambda *args, **kwargs: None
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn('Preparation fallback')['id']
        self.rt.start_image_base.side_effect = RuntimeError('base unavailable')
        self.rt.preparation_wait_seconds = .01
        native = concurrent.futures.Future()
        self.rt.connect()
        original_submit = self.rt.server.submit
        requests = []
        def submit(method, params):
            if method == 'thread/start':
                requests.append(params)
                return native
            return original_submit(method, params)
        with patch.object(self.rt.server, 'submit', side_effect=submit):
            waits = []
            for _ in range(2):
                with self.assertRaises(PreparationPending) as pending:
                    self.rt.prepare(self.rt.agent(worker))
                waits.append(pending.exception.future)
            self.assertIs(waits[0], waits[1])
            self.assertEqual(len(requests), 1)
            native.set_result({'thread': {'id': 'fallback-thread'}})
            self.assertEqual(waits[0].result(3)['threadId'], 'fallback-thread')
            fixture.f.eventually(lambda: self.rt.agent(worker)['imageWorkspacePhase'] == 'fallback')
            record = self.rt.agent(worker)
            self.assertTrue(record['worktree'])
            self.assertEqual(record['threadId'], 'fallback-thread')
            self.assertIsNone(record['error'])
            self.assertNotIn(worker, self.rt.loaded)

    def test_ready_callback_waits_for_preparation_before_operation_registration(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.schedule_fast_dispatch = lambda *args, **kwargs: None
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn('Before operation registration')['id']
        mount = self.root / 'registration-mount'
        (mount / 'repo' / 'project').mkdir(parents=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
        engine.exec_prefix = Mock(return_value=[])
        entered, release = threading.Event(), threading.Event()
        original_params = self.rt.new_thread_params
        def params(agent):
            result = original_params(agent)
            entered.set()
            if not release.wait(3):
                raise RuntimeError('Preparation parameter gate timed out')
            return result
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch.object(self.rt, 'new_thread_params', side_effect=params), \
                patch.object(self.rt, '_send_image_workspace_notice'):
            prepared = self.rt.pool.submit(self.rt.prepare, self.rt.agent(worker))
            self.assertTrue(entered.wait(3))
            callback = self.rt.pool.submit(self.rt.image_base_completed, worker, {'state': 'ready'})
            try:
                self.assertFalse(callback.done())
                self.assertFalse(self.rt.agent(worker)['imageWorkspaceReady'])
            finally:
                release.set()
            self.assertIsNotNone(prepared.result(3)['threadId'])
            callback.result(3)
            fixture.f.eventually(lambda: self.rt.agent(worker)['imageWorkspaceReady'])
            self.assertIsNone(self.rt.agent(worker)['error'])

    def test_ready_image_workspace_keeps_existing_worktree_permissions(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.rt.agent(self.spawn()['id'])
        worker.update(cwd=str(self.root / 'mount'), imageWorkspaceReady=True,
                      imageWorkspacePhase='ready', yoloMode=None)
        expected = {**worker, 'imageWorkspace': False, 'imageWorkspaceReady': False}
        self.assertEqual(self.rt.turn_permissions(worker), self.rt.turn_permissions(expected))
        self.assertEqual(self.rt.turn_permissions(worker), {})
        params = self.rt.new_thread_params(worker)
        self.assertNotIn('approvalPolicy', params)
        self.assertNotIn('sandbox', params)

    def test_image_base_ref_is_given_to_worker_without_snapshot_metadata(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        user_head = subprocess.check_output(
            ['git', '-C', str(self.repo), 'rev-parse', 'HEAD'], text=True).strip()
        default = self.rt.agent(self.spawn('implicit-head')['id'])
        explicit = self.rt.agent(self.spawn('explicit-head', base_ref='HEAD')['id'])
        self.rt.projects({'action': 'set_worker_base', 'path': str(self.repo),
                          'base_ref': 'HEAD', 'expected_revision': 0})
        project_default = self.rt.agent(self.spawn('project-default')['id'])
        self.assertIsNone(default.get('imageWorkspaceBaseRef'))
        self.assertEqual(explicit['imageWorkspaceBaseRef'], 'HEAD')
        self.assertEqual(project_default['imageWorkspaceBaseRef'], 'HEAD')
        self.assertEqual(explicit['workerBaseRef'], 'HEAD')
        self.assertEqual(project_default['workerBaseRef'], 'HEAD')


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
        self.assertIn('disk full', submit.call_args.args[2])
        with self.rt.db() as db:
            event = db.execute("SELECT text FROM runtime_events WHERE agent=? AND kind='child_result'",
                               (self.lead['id'],)).fetchone()[0]
        result = __import__('json').loads(event)
        self.assertEqual(result['workspace']['state'], 'failed')
        self.assertEqual(result['workspace']['error'], 'disk full')
        self.assertEqual(record['workspaceMode'], 'worktree')
        self.assertFalse(record['workspaceModeExplicit'])
        self.assertEqual(result['workspace']['mode'], 'worktree')
        self.assertEqual(result['workspace']['path'], record['cwd'])

    def test_explicit_image_failure_stays_read_only_and_reports_no_fallback(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker_id = self.spawn('Explicit image', workspace='image')['id']
        source = str(self.repo / 'project')
        with patch.object(self.rt.pool, 'submit'):
            self.rt.image_base_completed(worker_id, {'state': 'failed', 'error': 'disk full'})
        record = self.rt.agent(worker_id)
        self.assertEqual(record['workspaceMode'], 'image')
        self.assertTrue(record['workspaceModeExplicit'])
        self.assertTrue(record['imageWorkspace'])
        self.assertFalse(record['imageWorkspaceReady'])
        self.assertEqual(record['imageWorkspacePhase'], 'failed')
        self.assertFalse(record['worktree'])
        self.assertEqual(record['cwd'], source)
        self.assertEqual(record['status'], 'paused')
        self.assertFalse(record['autoWake'])
        self.assertEqual(
            self.rt.turn_permissions(record)['sandboxPolicy']['writableRoots'],
            [self.rt.image_workspace_temp(record)],
        )
        with self.rt.db() as db:
            event = db.execute(
                "SELECT text FROM runtime_events WHERE agent=? AND kind='child_result' ORDER BY created DESC LIMIT 1",
                (self.lead['id'],),
            ).fetchone()[0]
        result = __import__('json').loads(event)
        self.assertEqual(result['workspace']['mode'], 'image')
        self.assertEqual(result['workspace']['path'], source)
        self.assertEqual(result['workspace']['error'], 'disk full')

    def test_ready_callback_creates_writable_mount_and_one_notice(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        mount = self.root / 'mount'
        project = mount / 'repo' / 'project'
        project.mkdir(parents=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'path': str(mount / 'repo')})
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
        self.assertIsNone(record['branch'])
        self.assertNotEqual(self.rt.turn_permissions(record)['sandboxPolicy'], {'type': 'readOnly'})
        notice.assert_called_once()
        self.assertIn(str(project), notice.call_args.args[1])
        self.assertIn('uncommitted changes', notice.call_args.args[1])
        engine.create_workspace.assert_called_once_with(
            str(self.repo), worker)

    def test_failed_mount_path_check_removes_created_workspace(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(self.root / 'mount'), 'path': str(self.root / 'mount/repo')})
        engine.exec_prefix = Mock(return_value=[])
        engine.remove_workspace = Mock()
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch('codex_runtime.subprocess.run', return_value=types.SimpleNamespace(returncode=1)):
            self.rt.image_base_completed(worker, {'state': 'ready'})
        engine.remove_workspace.assert_called_once_with(worker)
        record = self.rt.agent(worker)
        self.assertEqual(record['imageWorkspacePhase'], 'fallback')
        self.assertIn('workspace', record['imageWorkspaceError'].lower())

    def test_partial_create_failure_removes_workspace_by_agent_id(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(side_effect=RuntimeError('mount failed after reservation'))
        engine.exec_prefix = Mock(return_value=[])
        engine.remove_workspace = Mock(return_value={'state': 'removed'})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            self.rt.image_base_completed(worker, {'state': 'ready'})
        engine.remove_workspace.assert_called_once_with(worker)
        record = self.rt.agent(worker)
        self.assertTrue(record['worktree'])
        self.assertIn('mount failed after reservation', record['imageWorkspaceError'])

    def test_callback_replay_after_switch_does_not_duplicate_workspace_or_notice(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        mount = self.root / 'mount-replay'
        image_repo = mount / 'repo'
        subprocess.run(['git', 'clone', '-q', '--shared', str(self.repo), str(image_repo)], check=True)
        engine = types.ModuleType('codex_workspace_images')
        engine.create_workspace = Mock(return_value={
            'mount': str(mount), 'path': str(image_repo)})
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
        engine.ensure_mounted = Mock(return_value={'mount': str(mount), 'path': str(mount / 'repo')})
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
        engine.ensure_mounted = Mock(return_value={'path': str(image_repo)})
        engine.exec_prefix = Mock(return_value=['nsenter', '--'])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}), \
                patch('codex_runtime.git_toplevel', return_value=str(image_repo)) as detect:
            repo, directory, prefix = self.rt.worker_spawn_repository(actor, str(child_folder))
        self.assertEqual(repo, str(image_repo))
        self.assertEqual(directory, str(child_folder))
        self.assertEqual(prefix, ['nsenter', '--'])
        detect.assert_called_once_with(str(child_folder), prefix=['nsenter', '--'])

    def test_image_turn_skips_git_checkpoint_and_git_tools(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(imageWorkspaceReady=True, imageWorkspacePhase='ready',
                          status='completed', autoWake=True, inFlight=False,
                          turnId=None, lastCompletedTurn='turn-1')
            self.rt.put(db, 'agents', record)
        with patch.object(self.rt, '_capture_reserved_checkpoint') as capture, \
                patch('codex_workspace.subprocess.run') as run:
            with self.rt.lock, self.rt.db() as db:
                record = self.rt.agent(worker, db)
                self.rt.queue_checkpoint_after_turn(db, record, 'turn-1')
            self.rt.checkpoint_after_turn(worker, 'turn-1', 'checkpoint-1')
            capture.assert_not_called()
            run.assert_not_called()
        with self.assertRaisesRegex(ValueError, 'unavailable for image workspaces'):
            self.rt.git(self.rt.agent(worker), ['status'])
        with self.assertRaisesRegex(ValueError, 'Git checkpoints are unavailable for image workspaces'):
            self.rt.checkpoint_capture(worker)

    def test_unsupported_platform_uses_git_worktree(self):
        self.rt.image_workspace_support = lambda _repo: (False, 'unsupported platform')
        worker = self.spawn()
        self.assertEqual(worker['workspace'], 'worktree')
        record = self.rt.agent(worker['id'])
        self.assertFalse(record['imageWorkspace'])
        self.assertTrue(record['worktree'])

    def test_archive_retains_image_and_restore_reattaches_it(self):
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
        engine = types.ModuleType('codex_workspace_images')
        engine.archive_workspace = Mock(return_value={'freedBytes': 0, 'state': 'archived'})
        engine.workspace_bytes = Mock(return_value=123)
        engine.ensure_mounted = Mock(return_value={'mount': str(image_mount), 'path': str(image_repo)})
        engine.exec_prefix = Mock(return_value=[])
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            cleanup = _cleanup_image_workspace(self.rt, worker)
        self.assertEqual(cleanup['state'], 'archived')
        engine.archive_workspace.assert_called_once_with(worker)
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(deletedAt=11, epoch=1, status='paused', autoWake=False,
                          agentArchive={'at': 11, 'epoch': 1, 'cleanupPending': False})
            self.rt.put(db, 'agents', record)
        engine.ensure_mounted = Mock(return_value={
            'mount': str(image_mount), 'path': str(image_repo)})
        with patch('codex_agent_management.subprocess.run', return_value=types.SimpleNamespace(returncode=0)):
            with patch.dict(sys.modules, {'codex_workspace_images': engine}):
                result = manage_agent(self.rt, self.lead['id'],
                                      {'action': 'restore', 'agent_id': worker}, 0)
        self.assertEqual(result['status'], 'restored')
        record = self.rt.agent(worker)
        self.assertTrue(record['imageWorkspaceReady'])
        self.assertEqual(record['cwd'], str(image_repo / 'project'))
        engine.ensure_mounted.assert_called_once_with(worker)

    def test_delete_agent_removes_retained_image_outside_lock(self):
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn('Delete root')['id']
        child = self.spawn('Delete child')['id']
        with self.rt.lock, self.rt.db() as db:
            child_record = self.rt.agent(child, db)
            child_record['parentId'] = worker
            self.rt.put(db, 'agents', child_record)
        self.assertTrue(self.rt.agent(worker)['imageWorkspace'])
        self.assertTrue(self.rt.agent(child)['imageWorkspace'])
        engine = types.ModuleType('codex_workspace_images')
        attempted = []
        def remove(agent_id):
            self.assertFalse(self.rt.lock._is_owned())
            attempted.append(agent_id)
            if agent_id == worker:
                raise RuntimeError('image mount busy')
            return {'state': 'removed', 'freedBytes': 12}
        engine.remove_workspace = Mock(side_effect=remove)
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            result = self.rt.delete_conversation(worker)
        self.assertEqual(set(result['deleted']), {worker, child})
        self.assertEqual(result['imageCleanupFailed'], [worker])
        self.assertCountEqual(attempted, [worker, child])
        self.assertTrue(self.rt.agent(worker)['imageWorkspace'])
        self.assertIn('image mount busy', self.rt.agent(worker)['imageWorkspaceError'])
        engine.list_workspaces = Mock(return_value=[{'agentId': worker, 'state': 'ready',
                                                      'mount': '/retained/image'}])
        engine.base_bytes = Mock(return_value=0)
        engine.base_status = Mock(return_value={'state': 'ready'})
        from codex_agent_management import worktree_maintenance_report
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            report = worktree_maintenance_report(self.rt, self.lead['id'])
        self.assertIn(worker, [row['id'] for row in report['workspaces']])

    def test_maintenance_report_does_not_measure_workspace_or_base_sizes(self):
        from codex_agent_management import worktree_maintenance_report
        self.rt.image_workspace_support = lambda _repo: (True, '')
        self.rt.start_image_base = Mock(return_value={'state': 'building'})
        worker = self.spawn()['id']
        with self.rt.lock, self.rt.db() as db:
            record = self.rt.agent(worker, db)
            record.update(imageWorkspaceReady=False, imageWorkspacePhase='archived',
                          cleanedImageWorkspace={'phase': 'archived'})
            self.rt.put(db, 'agents', record)
            lead = self.rt.agent(self.lead['id'], db)
            lead['imageWorkspaceBaseRepo'] = str(self.repo)
            self.rt.put(db, 'agents', lead)
        engine = types.ModuleType('codex_workspace_images')
        engine.workspace_bytes = Mock(side_effect=AssertionError('Unexpected workspace size check'))
        engine.base_bytes = Mock(side_effect=AssertionError('Unexpected base size check'))
        engine.list_workspaces = Mock(return_value=[{'agentId': worker, 'state': 'archived',
                                                       'mount': '/retained/image'}])
        engine.base_status = Mock(return_value={'state': 'ready', 'version': 'v1', 'error': None})
        with patch.dict(sys.modules, {'codex_workspace_images': engine}):
            maintenance = worktree_maintenance_report(self.rt, self.lead['id'], self.lead['epoch'])
        self.assertEqual(maintenance['bases'][0]['state'], 'ready')
        self.assertNotIn('bytes', maintenance['bases'][0])
        self.assertEqual(maintenance['workspaces'][0]['state'], 'archived')
        engine.workspace_bytes.assert_not_called()
        engine.base_bytes.assert_not_called()


if __name__ == '__main__':
    unittest.main(verbosity=2)
