"""Real macOS image checks for generic folder workspace copies."""

import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / 'scripts'))

import codex_workspace_images as images
from codex_workspace_macos import Backend


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
        self.agent_id = 'mac-copy-test'
        self.git('init', '-q')
        self.git('config', 'user.name', 'Workspace Test')
        self.git('config', 'user.email', 'workspace-test@example.invalid')
        (self.folder / 'tracked.txt').write_text('base\n')
        (self.folder / 'node_modules' / '.cache').mkdir(parents=True)
        (self.folder / 'node_modules' / '.cache' / 'cache.bin').write_bytes(b'cache')
        (self.folder / '.worktrees' / 'legacy').mkdir(parents=True)
        (self.folder / '.worktrees' / 'legacy' / 'ignored').write_bytes(b'excluded')
        self.git('add', '-A')
        self.git('commit', '-m', 'base')

    def tearDown(self):
        try:
            images.remove_workspace(self.agent_id)
        except (OSError, RuntimeError, ValueError):
            pass
        if self.store.exists():
            for state_file in self.store.glob('bases/*/base.json'):
                state = json.loads(state_file.read_text())
                for version in (state_file.parent / 'versions').iterdir():
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
                              capture_output=True, text=True, timeout=60).stdout.strip()

    def test_full_copy_delta_archive_restore_and_remove(self):
        done = threading.Event()
        result = []
        images.start_base_build(self.folder, lambda value: (result.append(value), done.set()))
        self.assertTrue(done.wait(180), 'base build did not finish')
        self.assertEqual(result[-1]['state'], 'ready', result[-1])

        (self.folder / 'tracked.txt').write_text('user edit after base\n')
        self.git('add', 'tracked.txt')
        workspace = images.create_workspace(self.folder, self.agent_id)
        target = pathlib.Path(workspace['path'])
        self.assertEqual((target / 'tracked.txt').read_text(), 'user edit after base\n')
        self.assertEqual((target / '.git' / 'index').read_bytes(),
                         (self.folder / '.git' / 'index').read_bytes())
        self.assertEqual((target / '.git' / 'objects').is_dir(), True)
        self.assertEqual((target / 'node_modules' / '.cache' / 'cache.bin').read_bytes(), b'cache')
        self.assertFalse((target / '.worktrees').exists())
        archived = images.archive_workspace(self.agent_id)
        self.assertEqual(archived['state'], 'archived')
        image = pathlib.Path(images._read_json(images._agent_state_path(self.agent_id), {})['image'])
        self.assertTrue(image.exists())
        self.assertEqual(images.ensure_mounted(self.agent_id)['path'], workspace['path'])
        self.assertEqual(images.remove_workspace(self.agent_id)['state'], 'removed')
        self.assertFalse(image.exists())


if __name__ == '__main__':
    unittest.main()
