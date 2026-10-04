"""Exercise agent-view commands through a recording exec_prefix wrapper."""

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / 'scripts'))

import codex_workspace_images as images


class FakeBackend:
    def __init__(self, wrapper):
        self.wrapper = wrapper
        self.changed_paths = []

    def exec_prefix(self):
        return [str(self.wrapper)]

    def clone_workspace(self, base_image, agent_dir):
        layer = agent_dir / 'layer'
        shutil.copytree(base_image, layer, dirs_exist_ok=True)
        return layer

    def mount_workspace(self, layer, mount, *, base_image=None):
        repo = mount / 'repo'
        if not repo.exists():
            repo.parent.mkdir(parents=True, exist_ok=True)
            shutil.copytree(layer, repo)
        return {'mount': str(mount)}

    def sync_delta(self, repo_root, target_repo, token, *, excludes):
        if isinstance(token, dict):
            token = token.get('token')
        for value in self.changed_paths:
            parts = pathlib.Path(value).parts
            if '.git' in parts:
                rel = pathlib.Path(*parts[:parts.index('.git')])
                source, target = pathlib.Path(repo_root) / rel, target_repo / rel
                if source.is_dir():
                    shutil.copytree(source, target, dirs_exist_ok=True)
        return {'token': token, 'changedPaths': list(self.changed_paths), 'historyLost': False}

    def unmount_workspace(self, mount, *, force=False):
        return None

    def remove_layer(self, agent_dir):
        shutil.rmtree(agent_dir, ignore_errors=True)

    def private_bytes(self, path):
        return 0


class PrefixTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='workspace-prefix-')
        self.root = pathlib.Path(self.temp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.store = self.root / 'store'
        os.environ['CODEX_WORKSPACE_STORE'] = str(self.store)
        self.old_backend = images._backend_instance
        self.log = self.root / 'prefix.jsonl'
        wrapper = self.root / 'exec-prefix'
        wrapper.write_text(
            '#!/usr/bin/env python3\n'
            'import json,os,sys\n'
            f'with open({str(self.log)!r}, "a") as stream: '
            'stream.write(json.dumps(sys.argv[1:]) + "\\n")\n'
            'os.execvp(sys.argv[1], sys.argv[1:])\n')
        wrapper.chmod(0o755)
        images._backend_instance = FakeBackend(wrapper)
        self.backend = images._backend_instance
        self.git('init')
        self.git('config', 'user.name', 'Prefix Test')
        self.git('config', 'user.email', 'prefix@example.invalid')
        (self.repo / 'file.txt').write_text('base\n')
        self.git('add', '-A')
        self.git('commit', '-m', 'base')
        key = images._repo_key(self.repo)
        base_image = self.root / 'base-image'
        shutil.copytree(self.repo, base_image)
        base_dir = images._base_dir(key)
        base_dir.mkdir(parents=True)
        images._write_json(images._base_state_path(key), {
            'state': 'ready', 'repoRoot': str(self.repo), 'repoKey': key,
            'version': 'v-test', 'image': str(base_image), 'token': 0,
            'repositories': ['.'], 'dirtyPaths': {'.': []},
        })
        self.agent_id = 'prefix-test'

    def tearDown(self):
        try:
            images.remove_workspace(self.agent_id, force=True)
        except (OSError, RuntimeError, ValueError):
            pass
        images._backend_instance = self.old_backend
        os.environ.pop('CODEX_WORKSPACE_STORE', None)
        self.temp.cleanup()

    def git(self, *args, cwd=None):
        return subprocess.run(['git', '-C', str(cwd or self.repo), *args], check=True,
                              capture_output=True, text=True, timeout=30)

    def test_agent_git_and_file_commands_use_prefix(self):
        from unittest import mock
        with mock.patch.object(images, '_git_repositories',
                               side_effect=AssertionError('create enumerated the tree')):
            workspace = images.create_workspace(self.repo, self.agent_id)
        agent_repo = pathlib.Path(workspace['repoPath'])
        (agent_repo / 'file.txt').write_text('agent change\n')
        self.git('add', '-A', cwd=agent_repo)
        self.git('commit', '-m', 'agent change', cwd=agent_repo)
        with mock.patch.object(images, '_git_repositories',
                               side_effect=AssertionError('collect enumerated the tree')):
            result = images.collect(self.agent_id)
        self.assertEqual(result['state'], 'collected', result)
        self.assertEqual(len(result['repositories']), 1, result)

        calls = [json.loads(line) for line in self.log.read_text().splitlines()]
        agent_git_calls = [call for call in calls if call[0] == 'git' and str(agent_repo) in call]
        self.assertTrue(agent_git_calls, 'no agent Git calls reached exec_prefix')
        self.assertTrue(any('fetch' in call and str(agent_repo) in call for call in calls),
                        'collect fetch did not use exec_prefix')
        self.assertTrue(any(any('alternates' in arg for arg in call) for call in calls),
                        'agent alternates file write did not use exec_prefix')
        self.assertTrue(all(call[0] == 'git' or call[0] == sys.executable for call in calls), calls)

    def test_delta_discovers_a_new_nested_repository_without_tree_walk(self):
        nested = self.repo / 'nested'
        nested.mkdir()
        subprocess.run(['git', '-C', str(nested), 'init'], check=True,
                       capture_output=True, text=True)
        subprocess.run(['git', '-C', str(nested), 'config', 'user.name', 'Nested'], check=True)
        subprocess.run(['git', '-C', str(nested), 'config', 'user.email', 'nested@example.invalid'],
                       check=True)
        (nested / 'nested.txt').write_text('nested repo\n')
        subprocess.run(['git', '-C', str(nested), 'add', '-A'], check=True)
        subprocess.run(['git', '-C', str(nested), 'commit', '-m', 'nested base'], check=True,
                       capture_output=True, text=True)
        self.backend.changed_paths = ['nested/.git/config']
        self.agent_id = 'prefix-nested'
        from unittest import mock
        with mock.patch.object(images, '_git_repositories',
                               side_effect=AssertionError('create enumerated the tree')):
            workspace = images.create_workspace(self.repo, self.agent_id)
        state = images._read_json(images._agent_state_path(self.agent_id), {})
        self.assertIn('nested', [item['path'] for item in state['repositories']])
        self.assertTrue(pathlib.Path(workspace['repoPath'], 'nested', 'nested.txt').exists())


if __name__ == '__main__':
    unittest.main()
