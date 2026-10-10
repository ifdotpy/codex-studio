"""Portable contract checks for generic image workspace copies."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
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

    def _copy(self, source, destination, excludes, base=None, destination_base=None):
        source = pathlib.Path(source)
        destination = pathlib.Path(destination)
        base = source if base is None else pathlib.Path(base)
        destination_base = destination if destination_base is None else pathlib.Path(destination_base)
        excluded = {pathlib.Path(value) for value in excludes}
        destination.mkdir(parents=True, exist_ok=True)
        items = list(source.iterdir())
        present = {item.name for item in items}
        for old in destination.iterdir():
            relative = old.relative_to(destination_base)
            if old.parent == destination and pathlib.Path(old.name) in excluded:
                continue
            if old.name in present or relative in excluded or any(
                    parent in excluded for parent in relative.parents):
                continue
            if any(value.parts[:len(relative.parts)] == relative.parts for value in excluded):
                continue
            if old.is_dir() and not old.is_symlink():
                shutil.rmtree(old)
            else:
                old.unlink()
        for item in items:
            relative = item.relative_to(base)
            if relative in excluded or any(parent in excluded for parent in relative.parents):
                continue
            target = destination / item.name
            if item.is_dir() and not item.is_symlink():
                self._copy(item, target, excludes, base, destination_base)
            elif item.is_symlink():
                target.unlink(missing_ok=True)
                target.symlink_to(os.readlink(item), target_is_directory=item.is_dir())
            else:
                target.unlink(missing_ok=True)
                shutil.copy2(item, target)

    def copy_base_tree(self, root, destination, *, excludes, check=None):
        if self.fail_copy_once:
            self.fail_copy_once = False
            raise RuntimeError('injected base copy failure')
        if check:
            check()
        self._copy(root, destination, excludes)
        if check:
            check()
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
        return {'method': 'test-unmount', 'terminatedPids': 0}

    def remove_layer(self, agent_dir):
        shutil.rmtree(agent_dir, ignore_errors=True)

    def remove_base_version(self, path):
        shutil.rmtree(path, ignore_errors=True)

    def private_bytes(self, path):
        raise AssertionError('Workspace removal must not check its size')

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
        self.old_base_floor = os.environ.get('CODEX_WORKSPACE_MIN_FREE_BYTES')
        self.old_agent_floor = os.environ.get('CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES')
        os.environ['CODEX_WORKSPACE_STORE'] = str(self.store)
        self.old_backend = images._backend_instance
        self.backend = FakeBackend()
        images._backend_instance = self.backend
        self.free_space = mock.patch.object(images, '_free_bytes', return_value=64 * 1024**3)
        self.free_space.start()
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
        self.free_space.stop()
        images._backend_instance = self.old_backend
        if self.old_store is None:
            os.environ.pop('CODEX_WORKSPACE_STORE', None)
        else:
            os.environ['CODEX_WORKSPACE_STORE'] = self.old_store
        for name, value in (('CODEX_WORKSPACE_MIN_FREE_BYTES', self.old_base_floor),
                            ('CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES', self.old_agent_floor)):
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
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
        self.assertEqual(archived['unmount']['method'], 'test-unmount')
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
        removed = images.remove_workspace('archive-test')
        self.assertEqual(removed['state'], 'removed')
        self.assertEqual(removed['unmount']['method'], 'test-unmount')
        self.assertIsNone(removed['freedBytes'])
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


    def test_macos_root_event_does_not_rescan_without_must_scan_flag(self):
        import codex_workspace_macos as macos

        target = self.root / 'root-event-target'
        target.mkdir()
        events = [(str(self.folder), 0, 1)]
        with mock.patch.object(macos, '_read_events', return_value=(events, 1)), \
                mock.patch.object(macos, '_rsync_folder') as rsync:
            macos.Backend().sync_delta(self.folder, target, 0, excludes=())
        rsync.assert_not_called()

    def test_macos_root_must_scan_event_rescans_folder(self):
        import codex_workspace_macos as macos

        target = self.root / 'root-scan-target'
        target.mkdir()
        events = [(str(self.folder), 0x1, 1)]
        with mock.patch.object(macos, '_read_events', return_value=(events, 1)), \
                mock.patch.object(macos, '_rsync_folder') as rsync:
            macos.Backend().sync_delta(self.folder, target, 0, excludes=())
        rsync.assert_called_once()

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

    def test_base_build_is_refused_and_failed_state_is_saved_when_space_is_low(self):
        os.environ['CODEX_WORKSPACE_MIN_FREE_BYTES'] = '20'
        self.free_space.stop()
        self.free_space = mock.patch.object(images, '_free_bytes', return_value=19)
        self.free_space.start()

        result = images.start_base_build(self.folder)

        self.assertEqual(result['state'], 'failed')
        self.assertIn('Not enough free disk space for the image base: 19 free, 20 required (bytes)',
                      result['error'])
        self.assertEqual(images.base_status(self.folder)['state'], 'failed')
        self.assertFalse(list((self.store / 'bases').glob('*/versions/*')))

    def test_low_space_retry_does_not_start_a_build(self):
        os.environ['CODEX_WORKSPACE_MIN_FREE_BYTES'] = '20'
        self.free_space.stop()
        self.free_space = mock.patch.object(images, '_free_bytes', return_value=19)
        self.free_space.start()
        first = images.start_base_build(self.folder)
        self.assertEqual(first['state'], 'failed')
        state_path = images._base_state_path(images._repo_key(self.folder))
        state = images._read_json(state_path, {})
        state['failedAt'] = 0
        images._write_json(state_path, state)

        retried = images.start_base_build(self.folder, retry_failed=True)

        self.assertEqual(retried['state'], 'failed')
        self.assertIn('Not enough free disk space', retried['error'])
        self.assertFalse(list((self.store / 'bases').glob('*/versions/*')))

    def test_copy_abort_saves_failure_and_removes_staging_version(self):
        os.environ['CODEX_WORKSPACE_MIN_FREE_BYTES'] = '20'
        self.free_space.stop()
        self.free_space = mock.patch.object(
            images, '_free_bytes', side_effect=[100, 100, 100, 100, 19])
        self.free_space.start()
        done = threading.Event()
        result = []

        images.start_base_build(self.folder, lambda value: (result.append(value), done.set()))

        self.assertTrue(done.wait(30), 'low-space base build did not finish')
        self.assertEqual(result[-1]['state'], 'failed', result[-1])
        self.assertIn('19 free, 20 required (bytes)', result[-1]['error'])
        self.assertFalse(list((self.store / 'bases').glob('*/versions/*')))

    def test_staging_mount_must_remain_on_a_separate_device(self):
        mount = self.root / 'staging-mount'
        mount.mkdir()

        with self.assertRaisesRegex(RuntimeError, 'staging mount disappeared'):
            images._require_base_space({'mount': mount})


    def test_macos_copy_stops_both_processes_when_the_check_fails(self):
        import codex_workspace_macos as macos

        class FakePipe:
            def close(self):
                pass

        class FakeProcess:
            def __init__(self, *, producer=False):
                self.returncode = None
                self.stopped = False
                if producer:
                    self.stdout = FakePipe()

            def poll(self):
                return self.returncode

            def wait(self, timeout=None):
                if not self.stopped:
                    raise subprocess.TimeoutExpired('tar', timeout)
                self.returncode = -15
                return self.returncode

            def terminate(self):
                self.stopped = True

            def kill(self):
                self.stopped = True

        source = self.root / 'macos-abort-source'
        target = self.root / 'macos-abort-target'
        source.mkdir()
        (source / 'small-file').write_text('small')
        processes = [FakeProcess(producer=True), FakeProcess()]
        checks = 0

        def check():
            nonlocal checks
            checks += 1
            if checks == 3:
                raise RuntimeError('staging mount disappeared')

        with mock.patch.object(macos.subprocess, 'Popen', side_effect=processes):
            with self.assertRaisesRegex(RuntimeError, 'staging mount disappeared'):
                macos._copy_tree_parallel(source, target, check=check)
        self.assertTrue(all(process.stopped for process in processes))

    @unittest.skipUnless(sys.platform == 'darwin', 'requires the macOS tar implementation')
    def test_macos_parallel_copy_checks_and_copies_a_small_tree(self):
        import codex_workspace_macos as macos

        source = self.root / 'macos-copy-source'
        target = self.root / 'macos-copy-target'
        (source / 'nested').mkdir(parents=True)
        target.mkdir()
        (source / 'nested' / 'small-file').write_text('small')
        checks = []

        macos._copy_tree_parallel(source, target, check=lambda: checks.append(True))

        self.assertEqual((target / 'nested' / 'small-file').read_text(), 'small')
        self.assertTrue(checks)

    def test_create_workspace_is_refused_below_agent_floor(self):
        self.build_base()
        os.environ['CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES'] = '5'
        self.free_space.stop()
        self.free_space = mock.patch.object(images, '_free_bytes', return_value=4)
        self.free_space.start()

        with self.assertRaisesRegex(RuntimeError, '4 free, 5 required'):
            images.create_workspace(self.folder, 'low-space-agent')
        self.assertFalse(images._agent_state_path('low-space-agent').exists())
    def test_stale_building_base_is_restarted(self):
        key = images._repo_key(self.folder)
        state_path = images._base_state_path(key)
        state = {
            'schema': 2, 'state': 'building', 'changeDetector': 'git-v1',
            'repoRoot': str(self.folder), 'repoKey': key, 'version': 'v-stale',
            'builderPid': os.getpid() + 1,
        }
        images._write_json(state_path, {**state, 'builderPid': os.getpid()})
        self.assertEqual(images.base_status(self.folder)['state'], 'building')
        images._write_json(state_path, state)
        self.assertEqual(images.base_status(self.folder)['state'], 'missing')

        done = threading.Event()
        results = []
        images.start_base_build(self.folder, lambda value: (results.append(value), done.set()))
        self.assertTrue(done.wait(30), 'stale base build did not restart')
        self.assertEqual(results[-1]['state'], 'ready', results[-1])

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

        images.create_workspace(self.folder, 'reserved-base-test')
        self.assertEqual(reserved_base['repositories'], current['repositories'])
        images.remove_workspace('reserved-base-test')


if __name__ == '__main__':
    unittest.main()
