#!/usr/bin/env python3
"""Index refresh and sparse staged-entry delta contracts for image workspaces."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))

spec = importlib.util.spec_from_file_location(
    'workspace_images_prefix_fixture', Path(__file__).with_name('workspace-images-prefix.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_workspace_images as images


def git(root, *args):
    return subprocess.run(['git', '-C', str(root), *args], check=True,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout


def git_snapshot(root):
    git_dir = Path(git(root, 'rev-parse', '--absolute-git-dir').decode().strip())
    return {path.relative_to(git_dir).as_posix(): path.read_bytes()
            for path in git_dir.rglob('*') if path.is_file() and not path.is_symlink()}


def init_repo(root, name):
    root.mkdir(parents=True)
    git(root, 'init', '-q')
    git(root, 'config', 'user.name', name)
    git(root, 'config', 'user.email', name + '@example.test')
    (root / 'tracked.txt').write_text(name + ' original\n')
    git(root, 'add', 'tracked.txt')
    git(root, 'commit', '-qm', 'initial')


class ImageIndexContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='workspace-index-contract-')
        self.root = Path(self.temp.name)
        self.folder = self.root / 'repo'
        init_repo(self.folder, 'root')
        self.nested = self.folder / 'nested'
        init_repo(self.nested, 'nested')
        self.store = self.root / 'store'
        self.old_store = os.environ.get('CODEX_WORKSPACE_STORE')
        os.environ['CODEX_WORKSPACE_STORE'] = str(self.store)
        self.old_backend = images._backend_instance
        self.backend = fixture.FakeBackend()
        images._backend_instance = self.backend
        self.agent_id = 'index-contract-agent'

    def tearDown(self):
        try:
            images.remove_workspace(self.agent_id)
        except (OSError, RuntimeError, ValueError):
            pass
        images._backend_instance = self.old_backend
        if self.old_store is None:
            os.environ.pop('CODEX_WORKSPACE_STORE', None)
        else:
            os.environ['CODEX_WORKSPACE_STORE'] = self.old_store
        self.temp.cleanup()

    def build_base(self):
        done = threading.Event()
        results = []
        images.start_base_build(self.folder, lambda value: (results.append(value), done.set()))
        self.assertTrue(done.wait(60))
        self.assertEqual(results[-1]['state'], 'ready', results[-1])
        state = images._read_json(images._base_state_path(images._repo_key(self.folder)), {})
        return state

    def test_base_refresh_and_index_delta_preserve_user_git_and_staging(self):
        (self.folder / 'tracked.txt').write_text('root staged before base\n')
        git(self.folder, 'add', 'tracked.txt')
        (self.nested / 'tracked.txt').write_text('nested unstaged before base\n')
        root_before_base = git_snapshot(self.folder)
        nested_before_base = git_snapshot(self.nested)

        base = self.build_base()
        self.assertEqual(git_snapshot(self.folder), root_before_base)
        self.assertEqual(git_snapshot(self.nested), nested_before_base)
        self.assertEqual({row['path'] for row in base['repositories']}, {'.', 'nested'})
        base_root = Path(base['image']) / 'repo'
        self.assertNotEqual((base_root / '.git' / 'index').read_bytes(),
                            (self.folder / '.git' / 'index').read_bytes())

        (self.folder / 'added.txt').write_text('new staged root file\n')
        git(self.folder, 'add', 'added.txt')
        (self.folder / 'tracked.txt').write_text('root staged after base\n')
        git(self.folder, 'add', 'tracked.txt')
        (self.nested / 'added.txt').write_text('new staged nested file\n')
        git(self.nested, 'add', 'added.txt')
        (self.folder / ':(exclude)*').write_text('literal pathspec filename\n')
        git(self.folder, 'add', '--', ':(exclude)*')
        (self.nested / 'tracked.txt').write_text('nested original\n')
        git(self.nested, 'rm', '-q', 'tracked.txt')
        root_before_delta = git_snapshot(self.folder)
        nested_before_delta = git_snapshot(self.nested)
        expected_root = git(self.folder, 'status', '--porcelain=v2')
        expected_nested = git(self.nested, 'status', '--porcelain=v2')
        root_before_delta = git_snapshot(self.folder)
        nested_before_delta = git_snapshot(self.nested)

        original_sync = self.backend.sync_delta

        def omit_staged_file(root, target, token, *, excludes):
            result = original_sync(root, target, token, excludes=excludes)
            (Path(target) / 'tracked.txt').write_text('root staged before base\n')
            return result

        self.backend.sync_delta = omit_staged_file
        try:
            workspace = images.create_workspace(self.folder, self.agent_id)
        finally:
            self.backend.sync_delta = original_sync
        agent_root = Path(workspace['path'])
        self.assertEqual(git_snapshot(self.folder), root_before_delta)
        self.assertEqual(git_snapshot(self.nested), nested_before_delta)
        self.assertEqual(git(agent_root, 'status', '--porcelain=v2'), expected_root)
        self.assertEqual(git(agent_root / 'nested', 'status', '--porcelain=v2'), expected_nested)
        self.assertEqual({row['path'] for row in images._read_json(
            images._agent_state_path(self.agent_id), {})['repositories']}, {'.', 'nested'})

    def test_git_directory_delta_copies_commits_fetch_refs_and_gc(self):
        base = self.build_base()
        remote = self.root / 'remote'
        init_repo(remote, 'remote')
        (remote / 'remote.txt').write_text('fetched branch\n')
        git(remote, 'add', 'remote.txt')
        git(remote, 'commit', '-qm', 'remote branch')
        git(self.folder, 'remote', 'add', 'origin', str(remote))
        git(self.folder, 'fetch', 'origin')
        (self.folder / 'tracked.txt').write_text('new committed root\n')
        git(self.folder, 'add', 'tracked.txt')
        git(self.folder, 'mv', 'tracked.txt', 'moved.txt')
        git(self.folder, 'commit', '-qm', 'new source commit')
        git(self.folder, 'gc', '--prune=now')

        workspace = images.create_workspace(self.folder, self.agent_id)
        target = Path(workspace['path'])
        self.assertEqual(git(target, 'log', '-1', '--format=%H'),
                         git(self.folder, 'log', '-1', '--format=%H'))
        self.assertEqual(git(target, 'branch', '-a'), git(self.folder, 'branch', '-a'))
        self.assertEqual(git(target, 'status', '--porcelain=v2'),
                         git(self.folder, 'status', '--porcelain=v2'))

    def test_stash_and_git_config_changes_trigger_git_metadata_sync(self):
        self.build_base()
        (self.folder / 'tracked.txt').write_text('stashed after base\n')
        git(self.folder, 'stash', 'push', '-m', 'image delta stash')
        git(self.folder, 'config', 'image.delta-test', 'present')
        workspace = images.create_workspace(self.folder, self.agent_id)
        target = Path(workspace['path'])
        self.assertEqual(git(target, 'stash', 'list'), git(self.folder, 'stash', 'list'))
        self.assertEqual(git(target, 'config', 'image.delta-test'), b'present\n')
        self.assertEqual(git(target, 'status', '--porcelain=v2'),
                         git(self.folder, 'status', '--porcelain=v2'))

    def test_clean_create_reuses_snapshot_and_skips_index_parse_and_gitdir_mirror(self):
        self.build_base()
        with (mock.patch.object(images, '_repo_snapshots', wraps=images._repo_snapshots) as snapshots,
              mock.patch.object(images, '_git_repositories', wraps=images._git_repositories) as discovery,
              mock.patch.object(images, '_index_entries', wraps=images._index_entries) as entries,
              mock.patch.object(images, '_sync_git_directories',
                                wraps=images._sync_git_directories) as git_sync):
            images.create_workspace(self.folder, self.agent_id)
        self.assertEqual(snapshots.call_count, 1)
        self.assertEqual(discovery.call_count, 0)
        self.assertEqual(entries.call_count, 0)
        self.assertEqual(git_sync.call_count, 0)

    def test_changed_index_reads_only_candidate_paths(self):
        self.build_base()
        (self.folder / 'tracked.txt').write_text('staged after base\n')
        git(self.folder, 'add', 'tracked.txt')
        with (mock.patch.object(images, '_index_entries', wraps=images._index_entries) as entries,
              mock.patch.object(images, '_sync_git_directories',
                                wraps=images._sync_git_directories) as git_sync):
            workspace = images.create_workspace(self.folder, self.agent_id)
        self.assertGreater(entries.call_count, 0)
        self.assertEqual(git_sync.call_count, 0)
        self.assertTrue(all(call.kwargs['paths'] == {b'tracked.txt'}
                            for call in entries.call_args_list))
        target = Path(workspace['path'])
        self.assertEqual(git(target, 'status', '--porcelain=v2'),
                         git(self.folder, 'status', '--porcelain=v2'))

    def test_repo_scan_skips_excluded_worktrees(self):
        excluded = self.folder / '.worktrees' / 'ignored-repo'
        init_repo(excluded, 'ignored')
        self.assertEqual({relative for relative, _repo in images._git_repositories(self.folder)},
                         {'.', 'nested'})

    def test_nested_repo_created_after_base_gets_git_metadata(self):
        self.build_base()
        added = self.folder / 'added-repo'
        init_repo(added, 'added')
        workspace = images.create_workspace(self.folder, self.agent_id)
        target = Path(workspace['path']) / 'added-repo'
        self.assertEqual(git(target, 'rev-parse', 'HEAD'), git(added, 'rev-parse', 'HEAD'))
        self.assertEqual(git(target, 'status', '--porcelain=v2'),
                         git(added, 'status', '--porcelain=v2'))

    def test_nested_worktree_points_into_image_git_directory(self):
        linked = self.folder / 'linked-worktree'
        git(self.folder, 'worktree', 'add', '-q', '-b', 'image-linked-test', str(linked))
        self.build_base()
        workspace = images.create_workspace(self.folder, self.agent_id)
        target_root = Path(workspace['path'])
        target = target_root / 'linked-worktree'
        git_dir = Path(git(target, 'rev-parse', '--absolute-git-dir').decode().strip()).resolve()
        self.assertTrue(git_dir.is_relative_to((target_root / '.git').resolve()), git_dir)
        self.assertEqual(git(target, 'status', '--porcelain=v2'),
                         git(linked, 'status', '--porcelain=v2'))

    def test_plain_parent_rsync_excludes_nested_git_index(self):
        plain = self.root / 'plain'
        nested = plain / 'nested'
        init_repo(nested, 'nested-plain')
        target = self.root / 'plain-copy'
        shutil.copytree(plain, target)
        baseline = {'repositories': images._repo_snapshots(plain)}
        base_repositories, _ = images._repo_index_metadata(plain, target, refresh_all=True,
                                                            backend=self.backend)

        (nested / 'tracked.txt').write_text('staged after base\n')
        git(nested, 'add', 'tracked.txt')
        source_git_before = git_snapshot(nested)
        excludes = images._delta_excludes(plain, images._workspace_excludes(plain))
        original_run = subprocess.run
        with mock.patch('codex_workspace_images.subprocess.run', wraps=original_run) as run:
            delta = images._sync_detected(plain, target, baseline, excludes, self.backend)
        rsync_calls = [call.args[0] for call in run.call_args_list
                       if call.args and 'rsync' in call.args[0]]
        self.assertTrue(any('--exclude=/nested/.git/index' in args for args in rsync_calls),
                        rsync_calls)
        previous = {row['path']: row for row in base_repositories}
        _records, index_changes = images._repo_index_metadata(
            plain, target, previous, refresh_all=False, backend=self.backend)
        changed_index_paths = set(index_changes.get('nested', set()))
        images._refresh_changed_paths(plain, target, delta['changedPaths'], changed_index_paths,
                                      self.backend)

        self.assertEqual(git_snapshot(nested), source_git_before)
        self.assertEqual(git(target / 'nested', 'status', '--porcelain=v2'),
                         git(nested, 'status', '--porcelain=v2'))


if __name__ == '__main__':
    unittest.main()
