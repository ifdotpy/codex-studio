"""Portable contract checks for generic image workspace copies."""

import os
import pathlib
import shutil
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / 'scripts'))

import codex_workspace_images as images


class FakeBackend:
    def __init__(self):
        self.mounted = set()
        self.mutate_after_copy = None
        self.fail_copy_once = False
        self.fail_mount_once = False
        self.delta_tokens = []

    def supported(self, root):
        return root.is_dir(), ''

    def current_event_id(self, root):
        return 0

    def open_base_staging(self, root, key, version):
        version_path = pathlib.Path(os.environ['CODEX_WORKSPACE_STORE']) / 'bases' / key / 'versions' / version
        (version_path / 'repo').mkdir(parents=True)
        for name in ('home', 'tmp'):
            (version_path / name).mkdir()
        return {'root': version_path / 'repo', 'versionPath': version_path, 'token': 0}

    def _copy(self, source, destination, excludes):
        excluded = {pathlib.Path(value) for value in excludes}
        destination.mkdir(parents=True, exist_ok=True)
        for item in pathlib.Path(source).iterdir():
            if pathlib.Path(item.name) in excluded:
                continue
            target = destination / item.name
            if item.is_dir() and not item.is_symlink():
                shutil.copytree(item, target, symlinks=True, dirs_exist_ok=True)
            elif item.is_symlink():
                target.unlink(missing_ok=True)
                target.symlink_to(os.readlink(item), target_is_directory=item.is_dir())
            else:
                shutil.copy2(item, target)

    def copy_base_tree(self, root, destination, *, excludes):
        if self.fail_copy_once:
            self.fail_copy_once = False
            raise RuntimeError('injected base copy failure')
        self._copy(root, destination, excludes)
        if self.mutate_after_copy:
            self.mutate_after_copy()
            self.mutate_after_copy = None

    def seal_base(self, staging):
        return {'image': staging['versionPath'], 'versionPath': staging['versionPath'], 'token': 0}

    def clone_workspace(self, image, agent_dir):
        layer = pathlib.Path(agent_dir) / 'layer'
        shutil.copytree(image, layer, symlinks=True, dirs_exist_ok=True)
        return layer

    def mount_workspace(self, layer, mount, *, base_image=None):
        if self.fail_mount_once:
            self.fail_mount_once = False
            raise RuntimeError('injected mount failure')
        mount = pathlib.Path(mount)
        if mount not in self.mounted:
            shutil.copytree(layer, mount, symlinks=True, dirs_exist_ok=True)
            self.mounted.add(mount)
        return {'mount': str(mount)}

    def sync_delta(self, root, target, token, *, excludes):
        self.delta_tokens.append(token)
        self._copy(root, pathlib.Path(target), excludes)
        return {'token': 1, 'changedPaths': ['.'], 'scanPaths': ['.']}

    def unmount_workspace(self, mount, *, force=False):
        self.mounted.discard(pathlib.Path(mount))

    def remove_layer(self, agent_dir):
        shutil.rmtree(agent_dir, ignore_errors=True)

    def remove_base_version(self, path):
        shutil.rmtree(path, ignore_errors=True)

    def private_bytes(self, path):
        return images._allocated_bytes(path)

    def exec_prefix(self):
        return []


class WorkspaceCopyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='workspace-copy-')
        self.root = pathlib.Path(self.temp.name)
        self.folder = self.root / 'folder'
        self.folder.mkdir()
        self.store = self.folder / 'studio-store'
        self.old_store = os.environ.get('CODEX_WORKSPACE_STORE')
        os.environ['CODEX_WORKSPACE_STORE'] = str(self.store)
        self.old_backend = images._backend_instance
        self.backend = FakeBackend()
        images._backend_instance = self.backend
        (self.folder / '.git' / 'objects' / 'aa').mkdir(parents=True)
        (self.folder / '.git' / 'config').write_bytes(b'config bytes\x00\xff')
        (self.folder / '.git' / 'index').write_bytes(b'index bytes')
        (self.folder / '.git' / 'objects' / 'aa' / 'object').write_bytes(b'object bytes')
        (self.folder / 'node_modules' / '.cache').mkdir(parents=True)
        (self.folder / 'node_modules' / '.cache' / 'cache.bin').write_bytes(b'cache')
        (self.folder / '.worktrees' / 'legacy').mkdir(parents=True)
        (self.folder / '.worktrees' / 'legacy' / 'ignored').write_bytes(b'excluded')
        (self.folder / 'tracked.txt').write_bytes(b'initial')

    def tearDown(self):
        images._backend_instance = self.old_backend
        if self.old_store is None:
            os.environ.pop('CODEX_WORKSPACE_STORE', None)
        else:
            os.environ['CODEX_WORKSPACE_STORE'] = self.old_store
        self.temp.cleanup()

    def build_base(self):
        done = threading.Event()
        result = []
        images.start_base_build(self.folder, lambda value: (result.append(value), done.set()))
        self.assertTrue(done.wait(30), 'base build did not finish')
        self.assertEqual(result[-1]['state'], 'ready', result[-1])

    def test_copy_includes_metadata_cache_and_excludes_only_studio_folders(self):
        self.backend.mutate_after_copy = lambda: (self.folder / 'tracked.txt').write_bytes(b'changed during copy')
        self.build_base()
        workspace = images.create_workspace(self.folder, 'copy-test')
        target = pathlib.Path(workspace['path'])
        for relative in ('.git/config', '.git/index', '.git/objects/aa/object',
                         'node_modules/.cache/cache.bin'):
            self.assertEqual((target / relative).read_bytes(), (self.folder / relative).read_bytes())
        self.assertEqual((target / 'tracked.txt').read_bytes(), b'changed during copy')
        self.assertFalse((target / '.worktrees').exists())
        self.assertFalse((target / 'studio-store').exists())
        self.assertEqual(set(workspace), {'mount', 'path'})

    def test_archive_restore_and_remove_keep_the_image(self):
        self.build_base()
        workspace = images.create_workspace(self.folder, 'archive-test')
        (pathlib.Path(workspace['path']) / 'agent-only.txt').write_text('keep this file')
        state_path = images._agent_state_path('archive-test')
        state = images._read_json(state_path, {})
        image = pathlib.Path(state['image'])
        archived = images.archive_workspace('archive-test')
        self.assertEqual(archived['state'], 'archived')
        self.assertTrue(image.exists())
        self.assertEqual(images._read_json(state_path, {})['state'], 'archived')
        restored = images.create_workspace(self.folder, 'archive-test')
        self.assertEqual(restored['path'], workspace['path'])
        self.assertEqual((pathlib.Path(restored['path']) / 'agent-only.txt').read_text(),
                         'keep this file')
        self.assertEqual(images._read_json(state_path, {})['state'], 'ready')
        state['state'] = 'creating'
        images._write_json(state_path, state)
        images.ensure_mounted('archive-test')
        self.assertEqual(images._read_json(state_path, {})['state'], 'creating')
        self.assertEqual(images.remove_workspace('archive-test')['state'], 'removed')
        self.assertFalse(image.exists())

    def test_copy_delta_helper_replaces_a_destination_symlink(self):
        import codex_workspace_macos as macos

        source = self.root / 'source-file'
        target = self.root / 'target-file'
        victim = self.root / 'user-file'
        source.write_bytes(b'new copy')
        victim.write_bytes(b'keep')
        target.symlink_to(victim)
        macos._copy_delta_entry(source, target)
        self.assertFalse(target.is_symlink())
        self.assertEqual(target.read_bytes(), b'new copy')
        self.assertEqual(victim.read_bytes(), b'keep')

        root = self.root / 'target-root'
        root.mkdir()
        parent_victim = self.root / 'user-folder'
        parent_victim.mkdir()
        (parent_victim / 'nested').write_bytes(b'keep nested')
        (root / 'parent').symlink_to(parent_victim, target_is_directory=True)
        nested_source = self.root / 'nested-source'
        nested_source.write_bytes(b'new nested')
        macos._copy_delta_entry(nested_source, root / 'parent' / 'nested', root)
        self.assertFalse((root / 'parent').is_symlink())
        self.assertEqual((root / 'parent' / 'nested').read_bytes(), b'new nested')
        self.assertEqual((parent_victim / 'nested').read_bytes(), b'keep nested')
        scan_victim = self.root / 'scan-victim'
        scan_victim.mkdir()
        (root / 'scanned').symlink_to(scan_victim, target_is_directory=True)
        macos._prepare_delta_directory(root / 'scanned', root)
        self.assertFalse((root / 'scanned').is_symlink())
        self.assertEqual(list(scan_victim.iterdir()), [])

    def test_linux_folder_copy_keeps_destination_symlinks_private(self):
        if shutil.which('rsync') is None:
            self.skipTest('requires rsync')
        import codex_workspace_linux as linux

        source = self.root / 'linux-source'
        target = self.root / 'linux-target'
        source.mkdir()
        target.mkdir()
        victim = self.root / 'linux-victim'
        victim.write_bytes(b'keep')
        (source / 'file').write_bytes(b'new copy')
        (target / 'file').symlink_to(victim)
        linux._copy_folder(source, target)
        self.assertFalse((target / 'file').is_symlink())
        self.assertEqual((target / 'file').read_bytes(), b'new copy')
        self.assertEqual(victim.read_bytes(), b'keep')

    def test_failed_base_retries_when_requested(self):
        self.backend.fail_copy_once = True
        done = threading.Event()
        result = []
        images.start_base_build(self.folder, lambda value: (result.append(value), done.set()))
        self.assertTrue(done.wait(30), 'failed base build did not finish')
        self.assertEqual(result[-1]['state'], 'failed', result[-1])

        retried = threading.Event()
        retry_result = []
        images.start_base_build(self.folder, lambda value: (retry_result.append(value), retried.set()),
                                retry_failed=True)
        self.assertTrue(retried.wait(30), 'requested base retry did not finish')
        self.assertEqual(retry_result[-1]['state'], 'ready', retry_result[-1])

    def test_interrupted_create_uses_reserved_base_after_refresh(self):
        self.build_base()
        self.backend.fail_mount_once = True
        with self.assertRaisesRegex(RuntimeError, 'injected mount failure'):
            images.create_workspace(self.folder, 'reserved-base-test')
        reserved = images._read_json(images._agent_state_path('reserved-base-test'), {})
        self.assertEqual(reserved['state'], 'creating')
        reserved_base = images._read_json(images._base_dir(reserved['repoKey']) / 'versions' /
                                          reserved['baseVersion'] / 'version.json', {})
        self.assertTrue(reserved_base)

        current_path = images._base_state_path(reserved['repoKey'])
        current = images._read_json(current_path, {})
        newer_version = 'v-newer'
        newer_image = images._base_dir(reserved['repoKey']) / 'versions' / newer_version
        (newer_image / 'repo').mkdir(parents=True)
        newer = {**current, 'version': newer_version, 'image': str(newer_image), 'token': 'new-token'}
        images._write_json(newer_image / 'version.json', newer)
        images._write_json(current_path, newer)

        self.backend.delta_tokens.clear()
        images.create_workspace(self.folder, 'reserved-base-test')
        self.assertEqual(self.backend.delta_tokens[0], reserved_base['token'])
        self.assertNotEqual(self.backend.delta_tokens[0], 'new-token')
        images.remove_workspace('reserved-base-test')


if __name__ == '__main__':
    unittest.main()
