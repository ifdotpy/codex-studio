#!/usr/bin/env python3
"""Real macOS image engine through the runtime fixture provider."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'workspace_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


def wait_for(predicate, timeout=90):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if predicate():
            return
        time.sleep(.02)
    raise AssertionError('Timed out waiting for the real image workspace flow')


@unittest.skipUnless(sys.platform == 'darwin' and shutil.which('diskutil'),
                     'requires the macOS ASIF image backend')
class ImageWorkspaceRuntimeMacE2E(unittest.TestCase):
    def test_read_only_to_real_mount_commit_and_collect(self):
        with tempfile.TemporaryDirectory(prefix='image-runtime-e2e-') as temp:
            root = Path(temp)
            repo = root / 'repo'
            project = repo / 'project'
            project.mkdir(parents=True)
            subprocess.run(['git', 'init', '-q', str(repo)], check=True)
            subprocess.run(['git', '-C', str(repo), 'config', 'user.name', 'Fixture'], check=True)
            subprocess.run(['git', '-C', str(repo), 'config', 'user.email', 'fixture@example.test'], check=True)
            (project / 'tracked.txt').write_text('base tree\n')
            subprocess.run(['git', '-C', str(repo), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(repo), 'commit', '-qm', 'base tree'], check=True)

            old_store = os.environ.get('CODEX_WORKSPACE_STORE')
            os.environ['CODEX_WORKSPACE_STORE'] = str(root / 'store')
            import codex_workspace_images as engine
            engine._backend_instance = None
            create_real = engine.create_workspace
            allow_create = threading.Event()
            user_edit_written = threading.Event()
            rt = fixture.ControlledRuntime(root / 'state', fixture.f.FakeServer)
            rt.catalog = lambda account='default': copy.deepcopy(fixture.CATALOG)
            rt.image_workspace_support = lambda path: engine.supported(path)
            lead = rt.new_lead({'cwd': str(repo)})
            worker_id = None
            try:
                def create_after_first_read_only_turn(path, agent_id, *, start_commit=None):
                    if agent_id == worker_id:
                        if not allow_create.wait(90):
                            raise RuntimeError('test did not release image workspace creation')
                        (project / 'tracked.txt').write_text('parent uncommitted edit\n')
                        user_edit_written.set()
                    return create_real(path, agent_id, start_commit=start_commit)

                with patch.object(engine, 'create_workspace', side_effect=create_after_first_read_only_turn):
                    worker_id = rt.spawn_agents(rt.agent(lead['id']), {'agents': [{
                        'name': 'Image E2E', 'prompt': 'Add worker.txt', 'cwd': str(project)}]},
                        'image-runtime-e2e')['agents'][0]['id']
                    rt.prepare(rt.agent(worker_id))
                    rt.dispatch()
                    wait_for(lambda: any(method == 'turn/start' and params.get('threadId') ==
                                         rt.agent(worker_id).get('threadId') for method, params in rt.server.calls))
                    first = [params for method, params in rt.server.calls if method == 'turn/start' and
                             params.get('threadId') == rt.agent(worker_id).get('threadId')][-1]
                    self.assertEqual(first['approvalPolicy'], 'never')
                    self.assertEqual(first['sandboxPolicy'], {'type': 'readOnly'})
                    self.assertIn('read-only until Studio sends a workspace-ready notice',
                                  first['input'][0]['text'])
                    wait_for(lambda: rt.agent(worker_id).get('turnId') is not None)
                    agent = rt.agent(worker_id)
                    rt.server.complete(agent['threadId'], agent['turnId'], 'Read-only turn complete')
                    wait_for(lambda: not rt.agent(worker_id).get('inFlight'))
                    allow_create.set()
                    wait_for(lambda: rt.agent(worker_id).get('imageWorkspaceReady'))
                    self.assertTrue(user_edit_written.is_set())
                    ready = rt.agent(worker_id)
                    image_repo = Path(ready['imageWorkspaceMount']) / 'repo'
                    self.assertEqual((image_repo / 'project' / 'tracked.txt').read_text(),
                                     'parent uncommitted edit\n')

                    rt.dispatch()
                    wait_for(lambda: len([1 for method, params in rt.server.calls
                                          if method == 'turn/start' and params.get('threadId') ==
                                          rt.agent(worker_id).get('threadId')]) >= 2)
                    turns = [params for method, params in rt.server.calls if method == 'turn/start' and
                             params.get('threadId') == rt.agent(worker_id).get('threadId')]
                    self.assertIn('Write access is enabled', turns[-1]['input'][0]['text'])
                    self.assertEqual(turns[-1]['cwd'], str(image_repo / 'project'))
                    worker_file = image_repo / 'project' / 'worker.txt'
                    worker_file.write_text('worker commit\n')
                    git = lambda *args: subprocess.run([*engine.exec_prefix(), 'git', '-C', str(image_repo),
                                                         *args], check=True, capture_output=True)
                    git('config', 'user.name', 'Fixture Worker')
                    git('config', 'user.email', 'worker@example.test')
                    git('add', 'project/worker.txt')
                    git('commit', '-m', 'worker change')
                    agent = rt.agent(worker_id)
                    rt.server.complete(agent['threadId'], agent['turnId'], 'Committed worker change')
                    wait_for(lambda: rt.agent(worker_id).get('imageWorkspaceCollect') is not None)

                branch_head = subprocess.check_output(
                    ['git', '-C', str(repo), 'rev-parse', f'refs/heads/codex-agent/{worker_id}'], text=True).strip()
                collected_text = subprocess.check_output(
                    ['git', '-C', str(repo), 'show', branch_head + ':project/tracked.txt'], text=True)
                collected_worker = subprocess.check_output(
                    ['git', '-C', str(repo), 'show', branch_head + ':project/worker.txt'], text=True)
                self.assertEqual(collected_text, 'base tree\n')
                self.assertEqual(collected_worker, 'worker commit\n')
                self.assertEqual((project / 'tracked.txt').read_text(), 'parent uncommitted edit\n')
                self.assertEqual(rt.agent(worker_id)['imageWorkspaceCollect']['state'], 'collected')
            finally:
                allow_create.set()
                if worker_id:
                    try:
                        engine.remove_workspace(worker_id, force=True)
                    except Exception:
                        pass
                rt.close()
                engine._backend_instance = None
                if old_store is None:
                    os.environ.pop('CODEX_WORKSPACE_STORE', None)
                else:
                    os.environ['CODEX_WORKSPACE_STORE'] = old_store


if __name__ == '__main__':
    unittest.main()
