"""Source transfer archives preserve Git and reject external file references."""
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
import os
import shutil
from types import SimpleNamespace
from unittest.mock import patch

from codex_linux_workspace_sync import archive_source


class SourceArchives(unittest.TestCase):
    def setUp(self):
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {"CODEX_WORKSPACE_MIN_FREE_BYTES": "0"}).start()
        self.tmp = tempfile.TemporaryDirectory(prefix='linux-source-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
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
        self.assertIn('.git/index', names)
        self.assertIn('second', names)
        self.assertIn('dirty', names)
        self.assertNotIn('keep', names)

    def test_delta_imports_new_commit_objects_and_removes_deleted_refs(self):
        self.git('branch','old-ref')
        baseline, _ = self.archive('first.tar.gz')
        copy = self.root / 'guest'
        copy.mkdir()
        with tarfile.open(self.root / 'first.tar.gz') as archive:
            archive.extractall(copy,filter='data')
        (self.repo/'new-commit').write_text('new content')
        self.git('add','new-commit')
        self.git('commit','-qm','New objects')
        self.git('branch','-D','old-ref')
        delta, _ = self.archive('delta.tar.gz',baseline)
        self.assertIn('.git/refs/heads/old-ref',delta['deletePaths'])
        for relative in delta['deletePaths']:
            (copy/relative).unlink(missing_ok=True)
        with tarfile.open(self.root/'delta.tar.gz') as archive:
            archive.extractall(copy,filter='data')
        for args in [('rev-parse','HEAD'),('show','HEAD:new-commit'),('status','--porcelain=v2')]:
            self.assertEqual(subprocess.check_output(['git','-C',str(copy),*args]),self.git(*args).stdout)
        subprocess.run(['git','-C',str(copy),'fsck','--no-reflogs'],check=True,capture_output=True)

    def test_unborn_repository_preserves_staged_index(self):
        self.git('checkout','--orphan','unborn')
        self.git('rm','-rf','.')
        (self.repo/'new').write_text('staged')
        self.git('add','new')
        baseline, _ = self.archive('first.tar.gz')
        (self.repo/'new').write_text('next staged')
        self.git('add','new')
        delta, names = self.archive('delta.tar.gz',baseline)
        self.assertIn('.git/index',names)
        self.assertLess(delta['totalBytes'],64*1024)

    def test_contained_symlink_is_preserved(self):
        (self.repo / 'link').symlink_to('one')
        path = self.root / 'links.tar.gz'
        archive_source(self.repo, path)
        with tarfile.open(path) as archive:
            self.assertTrue(archive.getmember('link').issym())
            self.assertEqual(archive.getmember('link').linkname, 'one')

    def test_delta_preserves_new_staged_blob_and_distinct_working_file(self):
        # An unreachable object makes the initial store large. The delta must not resend it.
        large_bytes = 12*1024*1024
        subprocess.run(['git','-C',str(self.repo),'hash-object','-w','--stdin'],
                       input=os.urandom(large_bytes), check=True, capture_output=True)
        baseline, _ = self.archive('first.tar.gz')
        copy = self.root / 'guest'
        copy.mkdir()
        with tarfile.open(self.root / 'first.tar.gz') as archive:
            archive.extractall(copy, filter='data')
        (self.repo / 'one').write_text('STAGED')
        self.git('add', 'one')
        (self.repo / 'one').write_text('UNSTAGED')
        delta, names = self.archive('delta.tar.gz', baseline)
        self.assertIn('.git/index', names)
        self.assertLess(delta['totalBytes'],64*1024)
        print(f'Git source delta: storeBytes={large_bytes}, transferBytes={delta["totalBytes"]}',flush=True)
        self.assertNotIn('.git', delta['deletePaths'])
        for relative in delta['deletePaths']:
            path = copy / relative
            if path.is_dir():
                shutil.rmtree(path)
            else:
                path.unlink(missing_ok=True)
        with tarfile.open(self.root / 'delta.tar.gz') as archive:
            archive.extractall(copy, filter='data')
        for arguments in [('status', '--porcelain=v2'), ('diff', '--cached')]:
            result = subprocess.check_output(['git', '-C', str(copy), *arguments])
            self.assertEqual(result, self.git(*arguments).stdout)
        self.assertEqual(subprocess.check_output(['git', '-C', str(copy), 'show', ':one']), b'STAGED')
        self.assertEqual((copy / 'one').read_text(), 'UNSTAGED')

    def test_delta_does_not_read_through_a_new_ignored_symlink_parent(self):
        directory = self.repo / 'dir'
        directory.mkdir()
        (directory / 'file').write_text('tracked source')
        self.git('add', 'dir/file')
        self.git('commit', '-qm', 'Track directory')
        baseline, _ = self.archive('first.tar.gz')
        outside = self.root / 'outside'
        outside.mkdir()
        (outside / 'file').write_text('private outside data')
        shutil.rmtree(directory)
        directory.symlink_to(outside, target_is_directory=True)
        with (self.repo / '.git/info/exclude').open('a') as output:
            output.write('\ndir\n')
        self.assertIn(b' D dir/file', self.git('status', '--porcelain').stdout)
        original = tarfile.TarFile.add
        def guarded(archive, name, *args, **kwargs):
            if Path(name).is_file():
                self.assertTrue(Path(name).resolve().is_relative_to(self.repo))
            return original(archive, name, *args, **kwargs)
        with patch.object(tarfile.TarFile, 'add', guarded):
            with self.assertRaisesRegex(ValueError, 'outside the project|passes through a symlink'):
                self.archive('bad-delta.tar.gz', baseline)

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

    def test_disk_floor_stops_archive_before_upload(self):
        with patch('codex_linux_workspace_sync.shutil.disk_usage', return_value=SimpleNamespace(free=0)):
            with self.assertRaisesRegex(RuntimeError, 'insufficient free disk space'):
                self.archive('no-space.tar.gz')

    def test_source_change_during_archive_is_rejected(self):
        original = tarfile.TarFile.add
        def change_after_read(archive, name, *args, **kwargs):
            result = original(archive, name, *args, **kwargs)
            if Path(name).resolve() == (self.repo / 'one').resolve():
                (self.repo / 'one').write_text('changed after archive read')
            return result
        with patch.object(tarfile.TarFile, 'add', change_after_read):
            with self.assertRaisesRegex(ValueError, 'source changed during the archive'):
                self.archive('changed.tar.gz')


if __name__ == '__main__':
    unittest.main()
