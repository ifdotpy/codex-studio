#!/usr/bin/env python3
"""Image workspace mount identity: a mount point is used only for the agent's own volume.

A moved or restored state folder can leave a plain folder, another volume, a second
attachment, or a volume of a replaced image file. These checks use a fake volume model in
place of diskutil and hdiutil; they never attach a real image.
"""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import json
import os
import plistlib
import shutil
import tempfile
import typing
import unittest
from contextlib import redirect_stderr
from io import StringIO
from unittest import mock

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

import codex_workspace_images as images  # noqa: E402
import codex_workspace_macos as macos  # noqa: E402
from codex_shared_values import (IMAGE_MOUNT_PROBLEM_STATUSES,  # noqa: E402
                                 IMAGE_WORKSPACE_MARKER_NAME)
from studio_api.system.models import ImageMountProblemStatus  # noqa: E402


class VolumeBackend:
    """Fake attach and detach: a detach moves the volume into "<image>.volume"."""

    def __init__(self, store: Path):
        self.store = store
        self.mounted: dict[Path, Path] = {}
        self.attach_calls = 0

    def supported(self, root):
        return True, ''

    def current_event_id(self, root):
        return 0

    def open_base_staging(self, root, key, version):
        version_path = self.store / 'bases' / key / 'versions' / version
        (version_path / 'repo').mkdir(parents=True)
        image = version_path / 'base.asif'
        image.write_text('base image')
        return {'root': version_path / 'repo', 'image': image, 'versionPath': version_path,
                'token': 0}

    def copy_base_tree(self, root, destination, *, excludes=(), check=None):
        shutil.copytree(root, destination, dirs_exist_ok=True)

    def seal_base(self, staging):
        return {'image': staging['image'], 'versionPath': staging['versionPath'], 'token': 0}

    def clone_workspace(self, image, agent_dir):
        agent_dir.mkdir(parents=True, exist_ok=True)
        layer = agent_dir / 'workspace.asif'
        shutil.copy2(image, layer)
        return layer

    def mount_workspace(self, layer, mount, *, base_image=None):
        self.attach_calls += 1
        volume = Path(layer).with_suffix('.volume')
        if volume.exists():
            mount.rmdir()
            volume.rename(mount)
        (mount / 'repo').mkdir(parents=True, exist_ok=True)
        self.mounted[Path(mount)] = Path(layer)
        return {'mount': str(mount)}

    def sync_delta(self, root, target, token, *, excludes=()):
        return {'token': token, 'changedPaths': [], 'historyLost': False}

    def unmount_workspace(self, mount, *, force=False):
        layer = self.mounted.pop(Path(mount), None)
        if layer is not None:
            Path(mount).rename(layer.with_suffix('.volume'))
            Path(mount).mkdir()
        return {'method': 'fake', 'terminatedPids': 0}

    def is_mounted(self, path):
        return Path(path) in self.mounted

    def attachments(self):
        return [{'image': str(layer), 'mounts': [str(mount)], 'device': None}
                for mount, layer in self.mounted.items()]

    def remove_layer(self, agent_dir):
        shutil.rmtree(agent_dir, ignore_errors=True)

    def remove_base_version(self, path):
        shutil.rmtree(path, ignore_errors=True)

    def private_bytes(self, path):
        return 0

    def exec_prefix(self):
        return []


class MountIdentityContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='mount-identity-')
        self.root = Path(os.path.realpath(self.temp.name))
        self.project = self.root / 'project'
        self.project.mkdir()
        (self.project / 'file.txt').write_text('base')
        self.store = self.root / 'store'
        self.environment = mock.patch.dict(os.environ, {
            'CODEX_WORKSPACE_STORE': str(self.store),
            'CODEX_WORKSPACE_MIN_FREE_BYTES': '0',
            'CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES': '0'})
        self.environment.start()
        self.backend = VolumeBackend(self.store)
        self.old_backend = images._backend_instance
        images._backend_instance = self.backend
        self.old_report = images._mount_report

    def tearDown(self):
        images._backend_instance = self.old_backend
        images._mount_report = self.old_report
        self.environment.stop()
        self.temp.cleanup()

    def ready(self, *agents):
        key = images._repo_key(self.project)
        images._build_base(self.project, key)
        self.assertEqual(images.base_status(self.project)['state'], 'ready')
        for agent in agents:
            images.create_workspace(self.project, agent)

    def image(self, agent):
        return self.store / 'agents' / agent / 'workspace.asif'

    def mount(self, agent):
        return self.store / 'mnt' / agent

    def attach_marked(self, mount, image, owner):
        mount.mkdir(parents=True, exist_ok=True)
        (mount / IMAGE_WORKSPACE_MARKER_NAME).write_text(json.dumps(
            {'schema': 1, 'agentId': owner, 'image': str(image)}))
        self.backend.mounted[mount] = image

    def assert_refused(self, status, action):
        with self.assertRaises(images.ImageMountError) as raised:
            action()
        self.assertEqual(raised.exception.status, status, str(raised.exception))
        return raised.exception

    def test_attach_records_marker_and_identity(self):
        self.ready('agent-1')
        state = images._read_json(images._agent_state_path('agent-1'))
        marker = json.loads((self.mount('agent-1') / IMAGE_WORKSPACE_MARKER_NAME).read_text())
        inode = self.image('agent-1').stat().st_ino
        self.assertEqual(marker['agentId'], 'agent-1')
        self.assertEqual(marker['imageInode'], inode)
        self.assertEqual(state['mountIdentity']['imageInode'], inode)
        self.assertEqual(state['mountIdentity']['mount'], str(self.mount('agent-1')))
        images.ensure_mounted('agent-1')
        self.assertEqual(self.backend.attach_calls, 1)
        self.assertTrue(images.check_mounts()['ok'])

    def test_plain_folder_is_never_attached_over_or_deleted(self):
        self.ready('agent-1')
        images.archive_workspace('agent-1')
        # A restore from a copy of a mounted volume leaves its files in a plain folder.
        (self.mount('agent-1') / 'repo').mkdir()
        (self.mount('agent-1') / 'repo' / 'work.txt').write_text('only copy')
        for action in (lambda: images.ensure_mounted('agent-1'),
                       lambda: images.archive_workspace('agent-1'),
                       lambda: images.remove_workspace('agent-1')):
            self.assert_refused('plain-folder', action)
        self.assertEqual(self.backend.attach_calls, 1)
        self.assertTrue(self.image('agent-1').exists())
        self.assertEqual((self.mount('agent-1') / 'repo' / 'work.txt').read_text(), 'only copy')
        report = images.check_mounts()
        self.assertFalse(report['ok'])
        self.assertEqual(report['agents'][0]['status'], 'plain-folder')

    def test_foreign_and_unmarked_volumes_are_refused(self):
        self.ready('agent-1')
        images.archive_workspace('agent-1')
        self.attach_marked(self.mount('agent-1'), self.root / 'other.asif', 'agent-2')
        error = self.assert_refused('foreign-volume', lambda: images.ensure_mounted('agent-1'))
        self.assertIn('agent agent-2', str(error))
        self.assert_refused('foreign-volume', lambda: images.remove_workspace('agent-1'))
        self.assertTrue(self.image('agent-1').exists())
        (self.mount('agent-1') / IMAGE_WORKSPACE_MARKER_NAME).unlink()
        (self.root / 'other.asif').write_text('other image')
        self.assert_refused('foreign-volume', lambda: images.ensure_mounted('agent-1'))

    def test_unmarked_volume_of_this_image_is_adopted(self):
        self.ready('agent-1')
        # A volume attached before markers existed: hdiutil names this image.
        (self.mount('agent-1') / IMAGE_WORKSPACE_MARKER_NAME).unlink()
        state_path = images._agent_state_path('agent-1')
        state = images._read_json(state_path)
        state.pop('mountIdentity')
        images._write_json(state_path, state)
        images.ensure_mounted('agent-1')
        self.assertEqual(self.backend.attach_calls, 1)
        marker = json.loads((self.mount('agent-1') / IMAGE_WORKSPACE_MARKER_NAME).read_text())
        self.assertEqual(marker['imageInode'], self.image('agent-1').stat().st_ino)

    def test_second_attachment_is_refused_and_reported_at_startup(self):
        self.ready('agent-1')
        images.archive_workspace('agent-1')
        # The old state folder was moved away with its volume still attached.
        moved = self.root / 'moved' / 'workspaces'
        self.attach_marked(moved / 'mnt' / 'agent-1', moved / 'agents' / 'agent-1' / 'workspace.asif',
                           'agent-1')
        error = self.assert_refused('attached-elsewhere', lambda: images.ensure_mounted('agent-1'))
        self.assertIn(str(moved / 'mnt' / 'agent-1'), str(error))
        for action in (lambda: images.archive_workspace('agent-1'),
                       lambda: images.remove_workspace('agent-1')):
            self.assert_refused('attached-elsewhere', action)
        self.assertTrue(self.image('agent-1').exists())
        report = images.check_mounts()
        self.assertEqual(report['agents'][0]['status'], 'attached-elsewhere')
        self.assertEqual(report['strayAttachments'][0]['agentId'], 'agent-1')
        stderr = StringIO()
        with redirect_stderr(stderr):
            images._run_startup_mount_check()
        self.assertIn('agent-1: attached-elsewhere', stderr.getvalue())
        summary = images.mount_diagnostics()
        self.assertEqual(summary['state'], 'checked')
        self.assertEqual(summary['problems'], [{'agentId': 'agent-1', 'status': 'attached-elsewhere'}])
        self.assertEqual(summary['strayAttachments'], 1)
        self.assertNotIn('moved', json.dumps(summary))

    def test_moved_unmarked_volume_is_matched_by_its_image_path(self):
        self.ready('agent-1')
        images.archive_workspace('agent-1')
        moved = self.root / 'moved' / 'workspaces'
        (moved / 'mnt' / 'agent-1').mkdir(parents=True)
        self.backend.mounted[moved / 'mnt' / 'agent-1'] = moved / 'agents' / 'agent-1' / 'workspace.asif'
        self.assert_refused('attached-elsewhere', lambda: images.ensure_mounted('agent-1'))

    def test_replaced_image_file_is_refused_while_its_old_volume_is_attached(self):
        self.ready('agent-1')
        restored = self.image('agent-1').with_suffix('.restored')
        shutil.copy2(self.image('agent-1'), restored)
        restored.replace(self.image('agent-1'))
        for action in (lambda: images.ensure_mounted('agent-1'),
                       lambda: images.archive_workspace('agent-1')):
            self.assert_refused('image-replaced', action)
        self.assertTrue(self.backend.is_mounted(self.mount('agent-1')))

    def test_missing_image_with_nothing_attached_can_be_removed(self):
        self.ready('agent-1')
        images.archive_workspace('agent-1')
        self.image('agent-1').unlink()
        self.assertEqual(images.check_mounts()['agents'][0]['status'], 'image-missing')
        self.assertEqual(images.remove_workspace('agent-1')['state'], 'removed')
        self.assertFalse(self.mount('agent-1').exists())


class ContractOwnerTests(unittest.TestCase):
    def test_diagnostics_problem_codes_match_the_owner(self):
        self.assertEqual(typing.get_args(ImageMountProblemStatus), IMAGE_MOUNT_PROBLEM_STATUSES)


class MacBackendProbeTests(unittest.TestCase):
    def test_hdiutil_info_lists_image_mount_points_and_whole_disk(self):
        info = plistlib.loads(plistlib.dumps({'images': [
            {'image-path': '/store/agents/agent-1/workspace.asif', 'system-entities': [
                {'dev-entry': '/dev/disk9', 'content-hint': 'GUID_partition_scheme'},
                {'dev-entry': '/dev/disk9s1', 'content-hint': 'Apple_APFS'},
                {'dev-entry': '/dev/disk10s1', 'mount-point': '/store/mnt/agent-1'}]},
            {'image-path': '/other/base.asif', 'system-entities': []}]}))
        self.assertEqual(macos._attachments_from_info(info), [
            {'image': '/store/agents/agent-1/workspace.asif', 'mounts': ['/store/mnt/agent-1'],
             'device': '/dev/disk9'},
            {'image': '/other/base.asif', 'mounts': [], 'device': None}])

    def test_attach_refuses_a_mount_point_that_holds_files(self):
        with tempfile.TemporaryDirectory(prefix='mount-identity-backend-') as temp:
            mount = Path(temp) / 'mnt'
            mount.mkdir()
            (mount / 'left-behind.txt').write_text('only copy')
            with mock.patch.object(macos, '_mounted', return_value=False), \
                    mock.patch.object(macos, '_run') as run:
                with self.assertRaisesRegex(RuntimeError, 'holds files'):
                    macos.Backend().mount_workspace(Path(temp) / 'image.asif', mount)
            run.assert_not_called()
            self.assertEqual((mount / 'left-behind.txt').read_text(), 'only copy')

    def test_attach_refuses_a_mount_point_with_a_volume(self):
        with tempfile.TemporaryDirectory(prefix='mount-identity-backend-') as temp:
            with mock.patch.object(macos, '_mounted', return_value=True), \
                    mock.patch.object(macos, '_run') as run:
                with self.assertRaisesRegex(RuntimeError, 'already mounted'):
                    macos.Backend().mount_workspace(Path(temp) / 'image.asif', Path(temp) / 'mnt')
            run.assert_not_called()


if __name__ == '__main__':
    unittest.main()
