"""Real macOS lifecycle checks for generic folder workspace copies."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

import codex_workspace_images as images
from codex_workspace_macos import Backend


def tree_bytes(root, *, exclude_git_indexes=False):
    root = pathlib.Path(root)
    result = {}
    for current, dirs, files in os.walk(root, topdown=True, followlinks=False):
        current = pathlib.Path(current)
        relative = current.relative_to(root)
        dirs[:] = [name for name in dirs if not (relative == pathlib.Path('.') and name == '.worktrees')]
        for name in dirs + files:
            path = current / name
            rel = path.relative_to(root).as_posix()
            if exclude_git_indexes and path.name == 'index' and path.parent.name == '.git':
                continue
            if path.is_symlink():
                result[rel] = ('link', os.readlink(path))
            elif path.is_dir():
                result[rel] = ('dir',)
            else:
                result[rel] = ('file', path.read_bytes())
    return result


class MacWorkspaceCopyTests(unittest.TestCase):
    def setUp(self):
        if sys.platform != 'darwin' or not shutil.which('diskutil'):
            self.skipTest('requires macOS diskutil')
        self.temp = tempfile.TemporaryDirectory(prefix='studio-image-copy-')
        self.root = pathlib.Path(self.temp.name)
        self.folder = self.root / 'repo'
        self.folder.mkdir()
        self.store = self.root / 'store'
        self.old_store = os.environ.get('CODEX_WORKSPACE_STORE')
        os.environ['CODEX_WORKSPACE_STORE'] = str(self.store)
        self.agent_ids = []
        self.victim = self.root / 'victim'
        self.victim.write_text('user file must stay safe\n')
        self.git('init', '-q')
        self.git('config', 'user.name', 'Workspace Test')
        self.git('config', 'user.email', 'workspace-test@example.invalid')
        (self.folder / 'tracked.txt').write_text('base\n')
        (self.folder / '.gitignore').write_text('ignored.txt\n.worktrees/\n')
        (self.folder / 'swap').symlink_to('../victim')
        self.git('add', '-A')
        self.git('commit', '-m', 'base')
        (self.folder / 'node_modules' / '.cache').mkdir(parents=True)
        (self.folder / 'node_modules' / '.cache' / 'cache.bin').write_bytes(b'cache')
        (self.folder / '.worktrees' / 'legacy').mkdir(parents=True)
        (self.folder / '.worktrees' / 'legacy' / 'ignored').write_bytes(b'excluded')

    def tearDown(self):
        for agent_id in self.agent_ids:
            try:
                images.remove_workspace(agent_id)
            except (OSError, RuntimeError, ValueError):
                pass
        if self.store.exists():
            for state_file in self.store.glob('bases/*/base.json'):
                versions = state_file.parent / 'versions'
                for version in versions.iterdir() if versions.exists() else ():
                    try:
                        Backend().remove_base_version(version)
                    except (OSError, RuntimeError):
                        pass
        if self.old_store is None:
            os.environ.pop('CODEX_WORKSPACE_STORE', None)
        else:
            os.environ['CODEX_WORKSPACE_STORE'] = self.old_store
        self.temp.cleanup()

    def git(self, *args, cwd=None):
        return subprocess.run(['git', '-C', str(cwd or self.folder), *args], check=True,
                              capture_output=True, text=True, timeout=180).stdout.strip()

    def new_agent(self, label):
        agent_id = 'mac-copy-' + label
        self.agent_ids.append(agent_id)
        return agent_id

    def build_base(self, root=None):
        root = pathlib.Path(root or self.folder)
        done = threading.Event()
        result = []
        images.start_base_build(root, lambda value: (result.append(value), done.set()))
        self.assertTrue(done.wait(300), f'base build timed out: {root}')
        self.assertEqual(result[-1]['state'], 'ready', result[-1])
        return result[-1]

    def test_full_copy_two_agents_archive_restore_and_remove(self):
        self.build_base()
        state = images._read_json(images._base_state_path(images._repo_key(self.folder)), {})
        self.assertTrue(pathlib.Path(state['image']).exists())
        (self.folder / 'tracked.txt').write_text('user edit after base\n')
        (self.folder / 'swap').unlink()
        (self.folder / 'swap').write_text('regular replacement\n')
        (self.folder / 'untracked.txt').write_text('untracked\n')
        (self.folder / 'ignored.txt').write_text('ignored\n')
        self.git('add', 'tracked.txt', 'swap')

        agent_a = self.new_agent('a')
        agent_b = self.new_agent('b')
        workspace_a = images.create_workspace(self.folder, agent_a)
        workspace_b = images.create_workspace(self.folder, agent_b)
        target_a, target_b = pathlib.Path(workspace_a['path']), pathlib.Path(workspace_b['path'])
        expected_tree = tree_bytes(self.folder, exclude_git_indexes=True)
        expected_tree.pop('ignored.txt', None)
        self.assertEqual(tree_bytes(target_a, exclude_git_indexes=True), expected_tree)
        self.assertEqual(tree_bytes(target_b, exclude_git_indexes=True), expected_tree)
        self.assertEqual(self.git('status', '--porcelain=v2', cwd=target_a),
                         self.git('status', '--porcelain=v2'))
        self.assertEqual(self.git('status', '--porcelain=v2', cwd=target_b),
                         self.git('status', '--porcelain=v2'))
        self.assertTrue((target_a / '.git' / 'objects').is_dir())
        self.assertTrue(any((target_a / '.git' / 'objects').rglob('*')))
        self.assertEqual((target_a / 'node_modules' / '.cache' / 'cache.bin').read_bytes(), b'cache')
        self.assertFalse((target_a / '.worktrees').exists())
        self.assertFalse((target_a / 'ignored.txt').exists())
        self.assertFalse((target_a / 'swap').is_symlink())
        self.assertEqual(self.victim.read_text(), 'user file must stay safe\n')

        (target_a / 'agent-only.txt').write_text('only in agent A')
        self.assertFalse((target_b / 'agent-only.txt').exists())
        self.assertFalse((self.folder / 'agent-only.txt').exists())
        image_a = pathlib.Path(images._read_json(images._agent_state_path(agent_a), {})['image'])
        archived = images.archive_workspace(agent_a)
        self.assertEqual(archived['state'], 'archived')
        self.assertTrue(image_a.exists())
        self.assertIn(agent_a, {row['agentId'] for row in images.list_workspaces()})
        restored = images.ensure_mounted(agent_a)
        self.assertEqual(restored['path'], workspace_a['path'])
        self.assertEqual((pathlib.Path(restored['path']) / 'agent-only.txt').read_text(),
                         'only in agent A')
        self.assertEqual(images.remove_workspace(agent_a)['state'], 'removed')
        self.assertFalse(image_a.exists())

    def test_folder_without_git_uses_same_copy_lifecycle(self):
        plain = self.root / 'plain-folder'
        (plain / 'nested').mkdir(parents=True)
        (plain / 'nested' / 'data.bin').write_bytes(b'plain bytes\x00\xff')
        (plain / 'ignored-output').write_text('no Git needed')
        self.build_base(plain)
        agent = self.new_agent('plain')
        workspace = images.create_workspace(plain, agent)
        target = pathlib.Path(workspace['path'])
        self.assertFalse((target / '.git').exists())
        self.assertEqual(tree_bytes(target), tree_bytes(plain))

    def test_immediate_writes_reach_git_and_plain_workspaces(self):
        self.build_base()
        plain = self.root / 'immediate-plain'
        plain.mkdir()
        (plain / 'seed.txt').write_text('seed\n')
        self.build_base(plain)
        backend = images._get_backend()
        original_sync = backend.sync_delta

        def no_fsevents(*_args, **_kwargs):
            raise AssertionError('workspace change detection used FSEvents')

        backend.sync_delta = no_fsevents
        try:
            with mock.patch('codex_workspace_macos._read_events', side_effect=no_fsevents):
                for attempt in range(10):
                    path = self.folder / f'immediate-{attempt}.txt'
                    path.write_text(f'git write {attempt}\n')
                    agent = self.new_agent(f'immediate-git-{attempt}')
                    workspace = images.create_workspace(self.folder, agent)
                    self.assertEqual((pathlib.Path(workspace['path']) / path.name).read_text(),
                                     f'git write {attempt}\n')
                    images.remove_workspace(agent)
                for attempt in range(10):
                    path = plain / f'immediate-{attempt}.txt'
                    path.write_text(f'plain write {attempt}\n')
                    agent = self.new_agent(f'immediate-plain-{attempt}')
                    workspace = images.create_workspace(plain, agent)
                    self.assertEqual((pathlib.Path(workspace['path']) / path.name).read_text(),
                                     f'plain write {attempt}\n')
                    images.remove_workspace(agent)
        finally:
            backend.sync_delta = original_sync

    def test_interrupted_create_retries_reserved_version_after_refresh(self):
        old_base = self.build_base()
        agent = self.new_agent('refresh-retry')
        backend = images._get_backend()
        original_mount = backend.mount_workspace
        failed = {'done': False}

        def fail_first_mount(layer, mount, *, base_image=None):
            if not failed['done'] and pathlib.Path(mount) == images._mount_path(agent):
                failed['done'] = True
                raise RuntimeError('injected create interruption')
            return original_mount(layer, mount, base_image=base_image)

        backend.mount_workspace = fail_first_mount
        try:
            with self.assertRaisesRegex(RuntimeError, 'injected create interruption'):
                images.create_workspace(self.folder, agent)
        finally:
            backend.mount_workspace = original_mount
        reserved = images._read_json(images._agent_state_path(agent), {})
        reserved_base = images._base_metadata(reserved['repoKey'], reserved['baseVersion'])
        self.assertEqual(reserved['state'], 'creating')
        self.assertTrue(reserved_base)

        (self.folder / 'tracked.txt').write_text('changed between base versions\n')
        self.git('add', 'tracked.txt')
        images._build_base(self.folder, reserved['repoKey'], reserved['baseVersion'])
        current = images._read_json(images._base_state_path(reserved['repoKey']), {})
        self.assertNotEqual(current['version'], reserved['baseVersion'])
        workspace = images.create_workspace(self.folder, agent)
        self.assertEqual(images._read_json(images._agent_state_path(agent), {})['baseVersion'],
                         reserved_base['version'])
        self.assertEqual((pathlib.Path(workspace['path']) / 'tracked.txt').read_text(),
                         'changed between base versions\n')

    def test_failed_base_retries_immediately_when_requested(self):
        backend = images._get_backend()
        original_copy = backend.copy_base_tree
        failed = {'done': False}

        def fail_once(root, destination, *, excludes):
            if not failed['done']:
                failed['done'] = True
                raise RuntimeError('injected base failure')
            return original_copy(root, destination, excludes=excludes)

        backend.copy_base_tree = fail_once
        try:
            first_done = threading.Event()
            first = []
            images.start_base_build(self.folder, lambda value: (first.append(value), first_done.set()))
            self.assertTrue(first_done.wait(300), 'failed base build timed out')
            self.assertEqual(first[-1]['state'], 'failed', first[-1])
        finally:
            backend.copy_base_tree = original_copy

        retry_done = threading.Event()
        retry = []
        images.start_base_build(self.folder,
                                lambda value: (retry.append(value), retry_done.set()),
                                retry_failed=True)
        self.assertTrue(retry_done.wait(300), 'requested retry timed out')
        self.assertEqual(retry[-1]['state'], 'ready', retry[-1])


if __name__ == '__main__':
    unittest.main(verbosity=2)
