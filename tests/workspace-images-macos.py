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


def _rounded(value):
    if isinstance(value, dict):
        return {key: _rounded(item) for key, item in value.items()}
    if isinstance(value, float):
        return round(value, 3)
    return value


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
        result = subprocess.run(['git', '-C', str(cwd or self.repo), *args],
                                check=False, capture_output=True, text=True, timeout=60)
        if check and result.returncode:
            raise AssertionError(f'git {args} failed in {cwd or self.repo}: {result.stderr}')
        return result

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
        backend = images._get_backend()
        done = threading.Event()
        result = []
        steps = {'repositoryDiscovery': 0.0, 'open': 0.0, 'copy': 0.0,
                 'seal': 0.0, 'repoSetup': 0.0, 'protectionRefs': 0.0}
        git_calls = {}
        direct_commands = {}
        saved_methods = {}
        for method, key in (('open_base_staging', 'open'), ('copy_base_tree', 'copy'),
                            ('seal_base', 'seal')):
            original = getattr(backend, method)
            saved_methods[method] = original

            def timed_backend(*args, _original=original, _key=key, **kwargs):
                tick = time.monotonic()
                try:
                    return _original(*args, **kwargs)
                finally:
                    steps[_key] += time.monotonic() - tick

            setattr(backend, method, timed_backend)
        original_prepare = images._prepare_repo

        def timed_prepare(*args, **kwargs):
            tick = time.monotonic()
            try:
                return original_prepare(*args, **kwargs)
            finally:
                steps['repoSetup'] += time.monotonic() - tick

        images._prepare_repo = timed_prepare
        original_discovery = images._git_repositories

        def timed_discovery(*args, **kwargs):
            tick = time.monotonic()
            try:
                return original_discovery(*args, **kwargs)
            finally:
                steps['repositoryDiscovery'] += time.monotonic() - tick

        images._git_repositories = timed_discovery
        original_git = images._git

        def timed_git(repo, *args, **kwargs):
            tick = time.monotonic()
            try:
                return original_git(repo, *args, **kwargs)
            finally:
                command = str(args[0]) if args else '(empty)'
                row = git_calls.setdefault(command, {'count': 0, 'seconds': 0.0})
                row['count'] += 1
                row['seconds'] += time.monotonic() - tick
                if len(args) > 1 and args[0] == 'update-ref' and str(args[1]).startswith('refs/studio/base/'):
                    steps['protectionRefs'] += time.monotonic() - tick

        images._git = timed_git
        original_command = images._command

        def timed_command(args, **kwargs):
            caller = sys._getframe(1).f_code.co_name
            tick = time.monotonic()
            try:
                return original_command(args, **kwargs)
            finally:
                if caller != '_git':
                    key = caller + ':' + str(args[0])
                    row = direct_commands.setdefault(key, {'count': 0, 'seconds': 0.0})
                    row['count'] += 1
                    row['seconds'] += time.monotonic() - tick

        images._command = timed_command
        started = time.monotonic()
        try:
            state = images.start_base_build(self.repo, lambda value: (result.append(value), done.set()))
            self.assertIn(state['state'], ('building', 'ready'))
            self.assertTrue(done.wait(180), 'base build did not finish')
            self.timings['base'] = time.monotonic() - started
        finally:
            images._git = original_git
            images._command = original_command
            images._prepare_repo = original_prepare
            images._git_repositories = original_discovery
            for method, original in saved_methods.items():
                setattr(backend, method, original)
        self.assertEqual(result[-1]['state'], 'ready', result[-1])
        steps['other'] = max(0.0, self.timings['base'] - sum(steps.values()))
        self.timings['baseSteps'] = {**steps, 'gitCalls': git_calls,
                                     'directCommands': direct_commands}
        self.base = json.loads(next((self.store / 'bases').glob('*/base.json')).read_text())
        self.assertIn('.git/modules/sub/objects', self.base['excludes'])

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

    def _create(self, agent_id, *, start_commit=None):
        self.agent_ids.append(agent_id)
        started = time.monotonic()
        backend = images._get_backend()
        steps = {'clone': 0.0, 'attach': 0.0, 'delta': 0.0, 'snapshotCommit': 0.0}
        saved_backend_methods = {}
        for method, key in (('clone_workspace', 'clone'), ('mount_workspace', 'attach'),
                            ('sync_delta', 'delta')):
            original = getattr(backend, method)
            saved_backend_methods[method] = original

            def timed_backend(*args, _original=original, _key=key, **kwargs):
                tick = time.monotonic()
                try:
                    return _original(*args, **kwargs)
                finally:
                    steps[_key] += time.monotonic() - tick

            setattr(backend, method, timed_backend)
        original_git = images._git

        def timed_git(repo, *args, **kwargs):
            tick = time.monotonic()
            try:
                return original_git(repo, *args, **kwargs)
            finally:
                if 'write-tree' in args or 'commit-tree' in args:
                    steps['snapshotCommit'] += time.monotonic() - tick

        images._git = timed_git
        try:
            result = images.create_workspace(self.repo, agent_id, start_commit=start_commit)
        finally:
            images._git = original_git
            for method, original in saved_backend_methods.items():
                setattr(backend, method, original)
        elapsed = time.monotonic() - started
        self.timings.setdefault('create', []).append(elapsed)
        steps['repoSetup'] = max(0.0, elapsed - sum(steps.values()))
        self.timings.setdefault('createSteps', []).append(steps)
        if not (pathlib.Path(result['repoPath']) / '.git').exists():
            state = images._read_json(images._agent_state_path(agent_id), {})
            raise AssertionError(f'workspace Git metadata missing after create: {state}')
        return result

    def test_real_image_workspace_lifecycle(self):
        self._build_base()
        self.git('branch', 'user-after-base')
        clean = self._create('agent-clean')
        self.assertIsNone(clean['snapshotCommit'])
        clean_repo = pathlib.Path(clean['repoPath'])
        self.assertEqual(self.git('show-ref', '--verify', '--quiet', 'refs/heads/user-after-base',
                                  cwd=clean_repo).returncode, 0)
        images.remove_workspace('agent-clean', force=True)
        old_version = images.base_status(self.repo)['version']
        large_file = self.repo / 'refresh-large.bin'
        large_file.write_bytes(b'R' * (20 * 1024 * 1024))
        refresh_started = time.monotonic()
        callback_called = threading.Event()
        self.assertEqual(images.start_base_build(self.repo,
                                                lambda _value: callback_called.set())['state'], 'ready')
        self.assertTrue(callback_called.wait(1), 'ready base callback was not immediate')
        deadline = time.monotonic() + 180
        while time.monotonic() < deadline and images.base_status(self.repo)['version'] == old_version:
            time.sleep(.1)
        self.assertNotEqual(images.base_status(self.repo)['version'], old_version,
                            'large delta did not refresh the base')
        self.timings['refresh'] = time.monotonic() - refresh_started
        self.git('add', 'refresh-large.bin')
        self.git('commit', '-m', 'large file after base refresh')
        refreshed_head = self.git('rev-parse', 'HEAD').stdout.strip()
        clean_after_refresh = self._create('agent-clean-after-refresh')
        self.assertIsNone(clean_after_refresh['snapshotCommit'])
        clean_after_repo = pathlib.Path(clean_after_refresh['repoPath'])
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=clean_after_repo).stdout.strip(), refreshed_head)
        self.assertEqual(self.git('status', '--porcelain', cwd=clean_after_repo).stdout, '')
        images.remove_workspace('agent-clean-after-refresh', force=True)
        custom_untracked = self.repo / 'custom-start-untracked.txt'
        custom_untracked.write_text('parent untracked edit\n')
        custom_status = self.git('status', '--porcelain').stdout
        custom_start = self._create('agent-custom-start', start_commit=self.base['head'])
        self.assertEqual(custom_start['startCommit'], self.base['head'])
        self.assertFalse((pathlib.Path(custom_start['repoPath']) / custom_untracked.name).exists())
        self.assertEqual(self.git('status', '--porcelain').stdout, custom_status)
        images.remove_workspace('agent-custom-start', force=True)
        custom_untracked.unlink()
        excluded = subprocess.run(['tmutil', 'isexcluded', str(self.store)],
                                  capture_output=True, text=True, timeout=30)
        self.assertEqual(excluded.returncode, 0, excluded.stderr)
        self.assertIn('Excluded', excluded.stdout)
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

        (self.repo / 'user-after-agent-start.txt').write_text('later user commit\n')
        self.git('add', 'user-after-agent-start.txt')
        self.git('commit', '-m', 'user commit after agent start')
        user_head_after_agent_start = self.git('rev-parse', 'HEAD').stdout.strip()

        # Base protection refs must keep alternated objects alive through prune-now.
        self.git('gc', '--prune=now')
        self.git('gc', '--prune=now', cwd=self.repo / 'sub')
        self.git('cat-file', '-e', start_a['startCommit'] + '^{commit}', cwd=repo_a)
        collected = images.collect('agent-a')
        self.assertEqual(collected['state'], 'collected', collected)
        saved_heads = {row['path']: row['head'] for row in collected['repositories']}
        self.assertEqual(set(saved_heads), {'.', 'sub'})
        self.assertTrue(all(row.get('branch') == 'codex-agent/agent-a'
                            for row in collected['repositories']))
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
        self.assertEqual(self.git('rev-parse', 'HEAD').stdout.strip(), user_head_after_agent_start)
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=self.repo / 'sub').stdout.strip(),
                         self.user_sub_head)
        self.assertEqual(self.git('status', '--porcelain').stdout, self.user_status)
        self.assertEqual(self.git('rev-parse', result_branch + '^').stdout.strip(), root_commits[-2])
        self.assertNotEqual(self.git('show', result_branch + ':user-after-agent-start.txt',
                                     check=False).returncode, 0)

        images.remove_workspace('agent-a', force=True)
        restored_a = images.create_workspace(self.repo, 'agent-a', restore_heads=saved_heads)
        restored_root = pathlib.Path(restored_a['repoPath'])
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=restored_root).stdout.strip(),
                         saved_heads['.'])
        self.assertEqual((restored_root / 'agent-change.txt').read_text(), 'agent change\n')
        self.assertEqual(self.git('rev-parse', 'HEAD', cwd=restored_root / 'sub').stdout.strip(),
                         saved_heads['sub'])
        self.assertEqual((restored_root / 'sub' / 'inner.txt').read_text(),
                         'agent submodule change\n')
        restored_state = json.loads((self.store / 'agents' / 'agent-a' / 'agent.json').read_text())
        self.assertTrue(all(row['snapshotCommit'] is None for row in restored_state['repositories']))

        # A user edit to .git/config must not disable the image's fast Git settings.
        remount = pathlib.Path(start_b['mount'])
        Backend().unmount_workspace(remount)
        ensured = images.ensure_mounted('agent-b')
        self.assertEqual(ensured['repoPath'], str(repo_b))
        self.assertEqual(self.git('config', 'core.checkStat', cwd=repo_b).stdout.strip(), 'minimal')
        self.assertEqual(self.git('config', 'core.trustctime', cwd=repo_b).stdout.strip(), 'false')

        status_started = time.monotonic()
        self.git('status', '--porcelain', cwd=repo_b)
        self.assertLess(time.monotonic() - status_started, 5.0,
                        'agent git status is slow after source config changes')

        # A file in the user snapshot must conflict when the agent changes it.
        (self.repo / 'conflict.txt').write_text('conflict base\n')
        conflict = self._create('agent-conflict')
        conflict_repo = pathlib.Path(conflict['repoPath'])
        (conflict_repo / 'conflict.txt').write_text('agent version\n')
        self.git('add', 'conflict.txt', cwd=conflict_repo)
        self.git('-c', 'user.name=Conflict Agent', '-c', 'user.email=agent@example.invalid',
                 'commit', '-m', 'agent conflict', cwd=conflict_repo)
        (self.repo / 'conflict.txt').write_text('user version\n')
        self.git('add', 'conflict.txt')
        self.git('commit', '-m', 'user conflict')
        conflict_user_head = self.git('rev-parse', 'HEAD').stdout.strip()
        conflict_result = images.collect('agent-conflict')
        self.assertEqual(conflict_result['state'], 'conflict', conflict_result)
        self.assertEqual(conflict_result['path'], '.')
        root_conflict = next(row for row in conflict_result['repositories'] if row['path'] == '.')
        self.assertEqual(root_conflict['state'], 'conflict')
        self.assertTrue(self.git('show-ref', '--verify', '--quiet',
                                 'refs/studio/agents/agent-conflict/raw', check=False).returncode == 0)
        self.assertNotEqual(self.git('show-ref', '--verify', '--quiet',
                                     'refs/heads/codex-agent/agent-conflict', check=False).returncode, 0)
        self.assertEqual(self.git('rev-parse', 'HEAD').stdout.strip(), conflict_user_head)
        self.assertEqual((self.repo / 'conflict.txt').read_text(), 'user version\n')

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
            'baseSteps': _rounded(self.timings['baseSteps']),
            'refresh': round(self.timings['refresh'], 3),
            'create': [round(value, 3) for value in self.timings['create']],
            'createSteps': [_rounded(row)
                            for row in self.timings['createSteps']],
        }))

    def test_linked_worktree_git_metadata_is_private(self):
        seed = self.root / 'linked-seed'
        linked = self.root / 'linked-repo'
        seed.mkdir()
        subprocess.run(['git', '-C', str(seed), 'init'], check=True, capture_output=True)
        subprocess.run(['git', '-C', str(seed), 'config', 'user.name', 'Workspace Test'], check=True)
        subprocess.run(['git', '-C', str(seed), 'config', 'user.email', 'workspace-test@example.invalid'], check=True)
        (seed / 'base.txt').write_text('base\n')
        subprocess.run(['git', '-C', str(seed), 'add', '-A'], check=True)
        subprocess.run(['git', '-C', str(seed), 'commit', '-m', 'linked base'], check=True,
                       capture_output=True)
        subprocess.run(['git', '-C', str(seed), 'worktree', 'add', '-b', 'linked-branch',
                        str(linked)], check=True, capture_output=True)
        self.assertTrue((linked / '.git').is_file())
        old_head = subprocess.check_output(['git', '-C', str(linked), 'rev-parse', 'HEAD'], text=True).strip()
        index_path = pathlib.Path(subprocess.check_output(
            ['git', '-C', str(linked), 'rev-parse', '--path-format=absolute', '--git-path', 'index'],
            text=True).strip())
        old_index = index_path.read_bytes()
        common_ref_before = subprocess.check_output(
            ['git', '-C', str(linked), 'for-each-ref', '--format=%(refname) %(objectname)'], text=True)
        agent_id = 'linked-worktree-agent'
        self.agent_ids.append(agent_id)
        completed = threading.Event()
        outcome = []
        images.start_base_build(linked, lambda result: (outcome.append(result), completed.set()))
        self.assertTrue(completed.wait(120), 'linked-worktree base build timed out')
        self.assertEqual(outcome[-1]['state'], 'ready', outcome[-1])
        workspace = images.create_workspace(linked, agent_id)
        agent_repo = pathlib.Path(workspace['repoPath'])
        self.assertTrue((agent_repo / '.git').is_dir())
        self.assertFalse((agent_repo / '.git' / 'commondir').exists())
        self.assertFalse((agent_repo / '.git' / 'gitdir').exists())
        (agent_repo / 'agent.txt').write_text('private\n')
        self.git('add', '-A', cwd=agent_repo)
        self.git('-c', 'user.name=Agent', '-c', 'user.email=agent@example.invalid',
                 'commit', '-m', 'agent commit', cwd=agent_repo)
        self.git('checkout', '-b', 'agent-local-branch', cwd=agent_repo)
        self.assertEqual(subprocess.check_output(['git', '-C', str(linked), 'rev-parse', 'HEAD'],
                                                text=True).strip(), old_head)
        index_after = pathlib.Path(subprocess.check_output(
            ['git', '-C', str(linked), 'rev-parse', '--path-format=absolute', '--git-path', 'index'],
            text=True).strip()).read_bytes()
        self.assertEqual(index_after, old_index)
        common_ref_after = subprocess.check_output(
            ['git', '-C', str(linked), 'for-each-ref', '--format=%(refname) %(objectname)'], text=True)
        self.assertEqual([line for line in common_ref_after.splitlines() if not line.startswith('refs/studio/')],
                         [line for line in common_ref_before.splitlines() if not line.startswith('refs/studio/')])

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
