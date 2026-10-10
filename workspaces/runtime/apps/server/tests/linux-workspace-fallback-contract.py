#!/usr/bin/env python3
"""Linux image requests fall back through the actual worker creation caller."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('fallback_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class LinuxFallback(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='linux-fallback-')
        self.root = Path(self.tmp.name).resolve()
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        for args in (['init', '-q'], ['config', 'user.name', 'Fixture'],
                     ['config', 'user.email', 'fixture@example.test']):
            subprocess.run(['git', '-C', str(self.repo), *args], check=True)
        (self.repo / 'file').write_text('committed')
        subprocess.run(['git', '-C', str(self.repo), 'add', '.'], check=True)
        subprocess.run(['git', '-C', str(self.repo), 'commit', '-qm', 'Base'], check=True)
        (self.repo / 'file').write_text('uncommitted')
        self.rt = f.ControlledRuntime(self.root / 'state', f.f.FakeServer)
        self.rt.catalog = lambda account='default': copy.deepcopy(f.CATALOG)
        self.addCleanup(patch.stopall)
        patch('codex_worker_workspace.platform.system', return_value='Linux').start()
        self.lead = self.rt.new_lead({'cwd': str(self.repo)})
        self.rt.start_image_base = lambda *args, **kwargs: self.fail('An image base must not start')

    def tearDown(self):
        self.rt.close()
        self.tmp.cleanup()

    def test_explicit_image_uses_worktree_and_reports_platform_reason(self):
        result = self.rt.spawn_agents(self.lead, {'agents': [{
            'name': 'Worker', 'prompt': 'Work', 'workspace': 'image'}]}, 'linux-image-fallback')['agents'][0]
        self.assertEqual(result['workspace'], 'worktree')
        self.assertIn('macOS ASIF', result['workspaceError'])
        worker = self.rt.agent(result['id'])
        self.assertFalse(worker['imageWorkspace'])
        self.assertEqual(worker['environment'], 'host')
        from codex_runtime import PreparationPending
        try:
            self.rt.prepare(worker)
        except PreparationPending as pending:
            pending.future.result(timeout=10)
            self.rt.prepare(self.rt.agent(worker['id']))
        worker = self.rt.agent(worker['id'])
        self.assertTrue(worker['worktreeReady'])
        self.assertEqual((Path(worker['cwd']) / 'file').read_text(), 'committed')
        self.assertEqual((self.repo / 'file').read_text(), 'uncommitted')

    def test_linux_image_backend_is_unavailable_and_vm_worker_override_is_rejected(self):
        from codex_runtime import Runtime
        import codex_workspace_images
        with patch.object(codex_workspace_images, '_backend_instance', None), patch('codex_workspace_images.sys.platform', 'linux'):
            with self.assertRaisesRegex(RuntimeError, 'macOS ASIF'):
                codex_workspace_images.supported(self.repo)
        with self.assertRaisesRegex(ValueError, 'Choose a layr chat'):
            self.rt.spawn_agents(self.lead, {'agents': [{
                'name': 'Old VM', 'prompt': 'Work', 'environment': 'linux'}]}, 'old-vm-worker')

    def test_new_linux_chat_defaults_to_worktree(self):
        from codex_runtime import Runtime
        created = Runtime.new_lead(self.rt, {'cwd': str(self.repo)})
        self.assertEqual(created['workspaceMode'], 'worktree')
        self.assertEqual(created['executionMode'], 'native')
        with self.assertRaisesRegex(ValueError, 'only worktree'):
            Runtime.new_lead(self.rt, {'cwd': str(self.repo), 'workspaceMode': 'image'})


if __name__ == '__main__':
    unittest.main()
