"""Real diskutil and Git checks for the macOS image workspace engine."""

import json
import os
import pathlib
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / 'scripts'))

import codex_workspace_images as images
from codex_workspace_macos import Backend


class WorkspaceImagesMacTests(unittest.TestCase):
    def setUp(self):
        if sys.platform != 'darwin' or not shutil.which('diskutil'):
            self.skipTest('requires macOS diskutil')
        self.temp = tempfile.TemporaryDirectory(prefix='studio-image-workspace-')
        self.root = pathlib.Path(self.temp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.store = self.root / 'store'
        self.previous_store = os.environ.get('CODEX_WORKSPACE_STORE')
        os.environ['CODEX_WORKSPACE_STORE'] = str(self.store)
        self.agent_ids = []
        self.timings = {}
        self._make_repo()

    def tearDown(self):
        for agent_id in self.agent_ids:
            try:
                images.remove_workspace(agent_id, force=True)
            except (OSError, RuntimeError, ValueError):
                pass
        for base_file in self.store.glob('bases/*/base.json') if self.store.exists() else ():
            state = json.loads(base_file.read_text())
            for item in state.get('protectedRefs', []):
                repo = pathlib.Path(state['repoRoot']) / item['path']
                self.git('update-ref', '-d', item['ref'], cwd=repo, check=False)
            for version in (base_file.parent / 'versions').glob('*'):
                try:
                    Backend().remove_base_version(version)
                except (OSError, RuntimeError):
                    pass
        if self.previous_store is None:
            os.environ.pop('CODEX_WORKSPACE_STORE', None)
        else:
            os.environ['CODEX_WORKSPACE_STORE'] = self.previous_store
        self.temp.cleanup()

    def git(self, *args, cwd=None, check=True):
        return subprocess.run(['git', '-C', str(cwd or self.repo), *args],
                              check=check, capture_output=True, text=True, timeout=60)

    def _make_repo(self):
        self.subsource = self.root / 'subsource'
        self.subsource.mkdir()
        subprocess.run(['git', '-C', str(self.subsource), 'init'], check=True,
                       capture_output=True, text=True)
        self.git('config', 'user.name', 'Workspace Test', cwd=self.subsource)
        self.git('config', 'user.email', 'workspace-test@example.invalid', cwd=self.subsource)
        (self.subsource / 'inner.txt').write_text('inner base\n')
        self.git('add', '-A', cwd=self.subsource)
        self.git('commit', '-m', 'submodule base', cwd=self.subsource)

        subprocess.run(['git', '-C', str(self.repo), 'init'], check=True,
                       capture_output=True, text=True)
        self.git('config', 'user.name', 'Workspace Test')
        self.git('config', 'user.email', 'workspace-test@example.invalid')
        self.git('config', 'protocol.file.allow', 'always')
        (self.repo / 'tracked.txt').write_text('base\n')
        (self.repo / 'delete-me.txt').write_text('base\n')
        self.git('add', 'tracked.txt', 'delete-me.txt')
        self.git('commit', '-m', 'base')
        self.git('-c', 'protocol.file.allow=always', 'submodule', 'add',
                 str(self.subsource), 'sub')
        self.git('commit', '-m', 'add submodule')

    def _build_base(self):
        done = threading.Event()
        result = []
        started = time.monotonic()
        state = images.start_base_build(self.repo, lambda value: (result.append(value), done.set()))
        self.assertIn(state['state'], ('building', 'ready'))
        self.assertTrue(done.wait(90), 'base build did not finish')
        self.timings['base'] = time.monotonic() - started
        self.assertEqual(result[-1]['state'], 'ready', result[-1])
        self.base = json.loads(next((self.store / 'bases').glob('*/base.json')).read_text())

    def _fresh_user_changes(self):
        (self.repo / 'tracked.txt').write_text('new user commit\n')
        self.git('add', 'tracked.txt')
        self.git('commit', '-m', 'user commit after base')
        (self.repo / 'sub' / 'inner.txt').write_text('submodule user commit\n')
        self.git('add', 'inner.txt', cwd=self.repo / 'sub')
        self.git('commit', '-m', 'submodule user commit', cwd=self.repo / 'sub')
        self.git('add', 'sub')
        self.git('commit', '-m', 'update submodule after base')
        (self.repo / 'tracked.txt').write_text('uncommitted user edit\n')
        (self.repo / 'untracked.txt').write_text('untracked user file\n')
        (self.repo / 'delete-me.txt').unlink()
        self.user_head = self.git('rev-parse', 'HEAD').stdout.strip()
        self.user_sub_head = self.git('rev-parse', 'HEAD', cwd=self.repo / 'sub').stdout.strip()
        self.user_status = self.git('status', '--porcelain').stdout

    def _create(self, agent_id):
        self.agent_ids.append(agent_id)
        started = time.monotonic()
        result = images.create_workspace(self.repo, agent_id)
        self.timings.setdefault('create', []).append(time.monotonic() - started)
        return result

    def test_real_image_workspace_lifecycle(self):
        self._build_base()
        old_version = images.base_status(self.repo)['version']
        large_file = self.repo / 'refresh-large.bin'
        large_file.write_bytes(b'R' * (20 * 1024 * 1024))
        refresh_started = time.monotonic()
        callback_called = threading.Event()
        self.assertEqual(images.start_base_build(self.repo,
                                                lambda _value: callback_called.set())['state'], 'ready')
        self.assertTrue(callback_called.wait(1), 'ready base callback was not immediate')
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline and images.base_status(self.repo)['version'] == old_version:
            time.sleep(.1)
        self.assertNotEqual(images.base_status(self.repo)['version'], old_version,
                            'large delta did not refresh the base')
        self.timings['refresh'] = time.monotonic() - refresh_started
        self.git('add', 'refresh-large.bin')
        self.git('commit', '-m', 'large file after base refresh')
        excluded = subprocess.run(['tmutil', 'isexcluded', str(self.store)],
                                  capture_output=True, text=True, timeout=30)
        self.assertEqual(excluded.returncode, 0, excluded.stderr)
        self.assertIn('Excluded', excluded.stdout)
        self.assertIsNone(self._create('agent-clean')['snapshotCommit'])
        images.remove_workspace('agent-clean', force=True)
        self._fresh_user_changes()
        self.git('config', 'core.checkStat', 'default')
        self.git('config', 'core.trustctime', 'true')
        legacy = self.repo / '.worktrees' / 'legacy'
        legacy.mkdir(parents=True)
        (legacy / 'must-not-copy').write_text('legacy data')
        self.user_status = self.git('status', '--porcelain').stdout

        start_a = self._create('agent-a')
        start_b = self._create('agent-b')
        repo_a = pathlib.Path(start_a['repoPath'])
        repo_b = pathlib.Path(start_b['repoPath'])
        self.assertFalse((repo_a / '.worktrees').exists())
        self.assertEqual((repo_a / 'tracked.txt').read_text(), 'uncommitted user edit\n')
        self.assertEqual((repo_a / 'untracked.txt').read_text(), 'untracked user file\n')
        self.assertFalse((repo_a / 'delete-me.txt').exists())
        self.assertEqual((repo_a / 'sub' / 'inner.txt').read_text(), 'submodule user commit\n')

        (repo_a / 'only-agent-a.txt').write_text('isolated\n')
        self.assertFalse((repo_b / 'only-agent-a.txt').exists())
        (repo_a / 'agent-change.txt').write_text('agent change\n')
        self.git('add', '-A', cwd=repo_a)
        self.git('-c', 'user.name=Agent A', '-c', 'user.email=agent-a@example.invalid',
                 'commit', '-m', 'agent root change', cwd=repo_a)
        (repo_a / 'sub' / 'inner.txt').write_text('agent submodule change\n')
        self.git('add', 'inner.txt', cwd=repo_a / 'sub')
        self.git('-c', 'user.name=Agent A', '-c', 'user.email=agent-a@example.invalid',
                 'commit', '-m', 'agent submodule change', cwd=repo_a / 'sub')
        self.git('add', 'sub', cwd=repo_a)
        self.git('-c', 'user.name=Agent A', '-c', 'user.email=agent-a@example.invalid',
                 'commit', '-m', 'agent submodule pointer', cwd=repo_a)

        # Base protection refs must keep alternated objects alive through prune-now.
        self.git('gc', '--prune=now')
        self.git('gc', '--prune=now', cwd=self.repo / 'sub')
        self.git('cat-file', '-e', start_a['startCommit'] + '^{commit}', cwd=repo_a)
        collected = images.collect('agent-a')
        self.assertEqual(collected['state'], 'collected', collected)
        result_branch = 'codex-agent/agent-a'
        self.assertEqual(self.git('rev-parse', result_branch, cwd=self.repo).stdout.strip(),
                         collected['repositories'][-1]['commit'])
        root_commits = self.git('rev-list', '--reverse', start_a['startCommit'] + '..' + result_branch,
                                cwd=self.repo).stdout.splitlines()
        self.assertEqual(len(root_commits), 2)
        self.assertEqual(self.git('rev-parse', root_commits[0] + '^', cwd=self.repo).stdout.strip(),
                         start_a['startCommit'])
        agent_state = json.loads((self.store / 'agents' / 'agent-a' / 'agent.json').read_text())
        sub_state = next(row for row in agent_state['repositories'] if row['path'] == 'sub')
        self.assertEqual(self.git('rev-list', '--count', sub_state['startCommit'] + '..codex-agent/agent-a',
                                  cwd=self.repo / 'sub').stdout.strip(), '1')
        self.assertEqual(self.git('show', result_branch + ':tracked.txt', cwd=self.repo).stdout,
                         'new user commit\n')
        self.assertNotEqual(self.git('show', result_branch + ':untracked.txt', cwd=self.repo,
                                     check=False).returncode, 0)
        self.assertEqual(self.git('show', result_branch + ':delete-me.txt', cwd=self.repo).stdout,
                         'base\n')
        self.assertEqual(self.git('rev-parse', 'HEAD').stdout.strip(), self.user_head)
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=self.repo / 'sub').stdout.strip(),
                         self.user_sub_head)
        self.assertEqual(self.git('status', '--porcelain').stdout, self.user_status)

        # A user edit to .git/config must not disable the image's fast Git settings.
        remount = pathlib.Path(start_b['mount'])
        Backend().unmount_workspace(remount)
        ensured = images.ensure_mounted('agent-b')
        self.assertEqual(ensured['repoPath'], str(repo_b))
        self.assertEqual(self.git('config', 'core.checkStat', cwd=repo_b).stdout.strip(), 'minimal')
        self.assertEqual(self.git('config', 'core.trustctime', cwd=repo_b).stdout.strip(), 'false')
        self.git('status', '--porcelain', cwd=repo_b)

        # A same-file user commit after the agent starts must conflict without moving the branch.
        (self.repo / 'conflict.txt').write_text('conflict base\n')
        self.git('add', 'conflict.txt')
        self.git('commit', '-m', 'add conflict file')
        conflict = self._create('agent-conflict')
        conflict_repo = pathlib.Path(conflict['repoPath'])
        (conflict_repo / 'conflict.txt').write_text('agent version\n')
        self.git('add', 'conflict.txt', cwd=conflict_repo)
        self.git('-c', 'user.name=Conflict Agent', '-c', 'user.email=agent@example.invalid',
                 'commit', '-m', 'agent conflict', cwd=conflict_repo)
        (self.repo / 'conflict.txt').write_text('user version\n')
        self.git('add', 'conflict.txt')
        self.git('commit', '-m', 'user conflict')
        conflict_result = images.collect('agent-conflict')
        self.assertEqual(conflict_result['state'], 'conflict', conflict_result)
        self.assertTrue(self.git('show-ref', '--verify', '--quiet',
                                 'refs/studio/agents/agent-conflict/raw', check=False).returncode == 0)
        self.assertNotEqual(self.git('show-ref', '--verify', '--quiet',
                                     'refs/heads/codex-agent/agent-conflict', check=False).returncode, 0)

        # A process with its cwd in the mount is stopped before image removal.
        holder = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'],
                                  cwd=pathlib.Path(start_b['repoPath']))
        time.sleep(.2)
        images.remove_workspace('agent-b', force=True)
        self.assertIsNotNone(holder.poll(), 'force removal left a process in the image')

        # Kill a creator after clonefile and state commit, then retry against the same image.
        self._crash_retry()
        print('workspace timings seconds:', json.dumps({
            'base': round(self.timings['base'], 3),
            'refresh': round(self.timings['refresh'], 3),
            'create': [round(value, 3) for value in self.timings['create']],
        }))

    def _crash_retry(self):
        agent_id = 'agent-crash'
        self.agent_ids.append(agent_id)
        marker = self.root / 'creator-paused'
        source = r'''
import os, pathlib, time
import codex_workspace_images as images
backend = images._get_backend()
mount = backend.mount_workspace
def pause(layer, path, *, base_image=None):
    pathlib.Path(os.environ['PAUSE_MARKER']).write_text('paused')
    time.sleep(60)
    return mount(layer, path, base_image=base_image)
backend.mount_workspace = pause
images.create_workspace(os.environ['TEST_REPO'], os.environ['TEST_AGENT'])
'''
        env = os.environ | {'PYTHONPATH': str(pathlib.Path(__file__).resolve().parents[1] / 'scripts'),
                            'PAUSE_MARKER': str(marker), 'TEST_REPO': str(self.repo),
                            'TEST_AGENT': agent_id}
        child = subprocess.Popen([sys.executable, '-c', source], env=env,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline and not marker.exists() and child.poll() is None:
            time.sleep(.01)
        self.assertTrue(marker.exists(), 'creator did not reach the mount step')
        child.send_signal(signal.SIGKILL)
        child.wait(timeout=10)
        marker.unlink(missing_ok=True)
        created = self._create(agent_id)
        self.assertTrue(pathlib.Path(created['repoPath'], 'tracked.txt').exists())


if __name__ == '__main__':
    unittest.main(verbosity=2)
