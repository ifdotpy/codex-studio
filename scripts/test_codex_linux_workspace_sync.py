"""Source transfer archives preserve Git and reject external file references."""
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import os
from unittest.mock import patch

from codex_linux_workspace_sync import archive_source


class SourceArchives(unittest.TestCase):
    def setUp(self):
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"CODEX_WORKSPACE_MIN_FREE_BYTES": "0"}).start()
        self.tmp = tempfile.TemporaryDirectory(prefix='linux-source-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.test')
        (self.repo / 'one').write_text('first')
        (self.repo / 'keep').write_text('unchanged')
        self.git('add', '.')
        self.git('commit', '-qm', 'First')

    def git(self, *arguments):
        return subprocess.run(['git', '-C', str(self.repo), *arguments], check=True,
                              capture_output=True, timeout=10)

    def archive(self, name, baseline=None):
        path = self.root / name
        result = archive_source(self.repo, path, baseline)
        with tarfile.open(path) as archive:
            names = archive.getnames()
        return result, names

    def test_full_copy_includes_dirty_index_and_untracked_files(self):
        (self.repo / 'one').write_text('staged')
        self.git('add', 'one')
        (self.repo / 'extra').write_text('untracked')
        (self.repo / '.worktrees').mkdir()
        (self.repo / '.worktrees/excluded').write_text('skip')
        result, names = self.archive('full.tar.gz')
        self.assertEqual(result['mode'], 'full')
        self.assertIn('.git/index', names)
        self.assertIn('extra', names)
        self.assertNotIn('.worktrees/excluded', names)

    def test_delta_uses_head_changes_dirty_paths_and_deletions(self):
        baseline, _ = self.archive('first.tar.gz')
        (self.repo / 'one').unlink()
        (self.repo / 'second').write_text('next commit')
        self.git('add', '-A')
        self.git('commit', '-qm', 'Second')
        (self.repo / 'dirty').write_text('untracked')
        result, names = self.archive('delta.tar.gz', baseline)
        self.assertEqual(result['mode'], 'delta')
        self.assertIn('one', result['deletePaths'])
        self.assertIn('.git', result['deletePaths'])
        self.assertIn('second', names)
        self.assertIn('dirty', names)
        self.assertNotIn('keep', names)

    def test_contained_symlink_is_preserved(self):
        (self.repo / 'link').symlink_to('one')
        path = self.root / 'links.tar.gz'
        archive_source(self.repo, path)
        with tarfile.open(path) as archive:
            self.assertTrue(archive.getmember('link').issym())
            self.assertEqual(archive.getmember('link').linkname, 'one')

    def test_external_symlink_is_rejected(self):
        (self.repo / 'link').symlink_to(self.root / 'outside')
        with self.assertRaisesRegex(ValueError, 'outside the project'):
            self.archive('bad.tar.gz')

    def test_absolute_contained_symlink_becomes_guest_relative(self):
        (self.repo / 'link').symlink_to(self.repo / 'one')
        path = self.root / 'links.tar.gz'
        archive_source(self.repo, path)
        with tarfile.open(path) as archive:
            self.assertEqual(archive.getmember('link').linkname, 'one')

    def test_non_git_copy_refreshes_the_complete_tree(self):
        (self.repo / '.git').rename(self.root / 'old-git')
        baseline, _ = self.archive('first.tar.gz')
        result, names = self.archive('second.tar.gz', baseline)
        self.assertEqual(result['mode'], 'full')
        self.assertIn('keep', names)


if __name__ == '__main__':
    unittest.main()
