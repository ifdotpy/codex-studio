#!/usr/bin/env python3
"""Worker worktree retry and disk accounting contracts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_worktree_creation import WorktreeNeedsReview, _run_checkout, create_worker_worktree
from codex_worktree_disk import (
    WorktreeDiskScanner,
    _allocated_bytes,
    _apfs_private_bytes,
    _measure_worktree,
    management_view,
)


class WorktreeContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.git('init')
        (self.repo / 'tracked.txt').write_text('original')
        self.git('add', 'tracked.txt')
        self.git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                 'commit', '-m', 'Base')
        self.path = self.repo / '.worktrees' / 'codex-agents' / 'worker'

    def tearDown(self):
        self.temp.cleanup()

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def test_creation_retries_timeout_with_longer_bound(self):
        calls = []
        def run(cmd, **kwargs):
            calls.append(kwargs['timeout'])
            if len(calls) == 1:
                raise subprocess.TimeoutExpired(cmd, kwargs['timeout'])
            return subprocess.run(cmd, **kwargs)
        recovered = create_worker_worktree(self.repo, self.path, self.path, 'codex-agent/worker',
                                            run=run, sleep=lambda _: None)
        self.assertFalse(recovered)
        self.assertEqual(calls, [900, 1800])
        self.assertEqual(self.git('worktree', 'list', '--porcelain').count('worktree '), 2)

    def test_checkout_timeout_stops_child_processes(self):
        marker = self.root / 'late-child.txt'
        child = ("import time,pathlib; time.sleep(0.5); "
                 f"pathlib.Path({str(marker)!r}).write_text('late')")
        parent = ("import subprocess,sys,time; "
                  f"subprocess.Popen([sys.executable,'-c',{child!r}]); time.sleep(3)")
        with self.assertRaises(subprocess.TimeoutExpired):
            _run_checkout([sys.executable, '-c', parent], timeout=0.1,
                          check=True, capture_output=True, text=True)
        time.sleep(0.7)
        self.assertFalse(marker.exists())

    def test_timeout_adopts_only_a_verified_checkout(self):
        def run(cmd, **kwargs):
            subprocess.run(cmd, **kwargs)
            raise subprocess.TimeoutExpired(cmd, kwargs['timeout'])
        self.assertTrue(create_worker_worktree(self.repo, self.path, self.path,
                                               'codex-agent/worker', run=run, sleep=lambda _: None))
        self.assertEqual((self.path / 'tracked.txt').read_text(), 'original')

    def test_timeout_finishes_registered_worktree_without_index(self):
        def run(cmd, **kwargs):
            insert = cmd.index('add') + 1
            subprocess.run([*cmd[:insert], '--no-checkout', *cmd[insert:]], **kwargs)
            raise subprocess.TimeoutExpired(cmd, kwargs['timeout'])
        self.assertTrue(create_worker_worktree(self.repo, self.path, self.path,
                                               'codex-agent/worker', run=run, sleep=lambda _: None))
        self.assertEqual((self.path / 'tracked.txt').read_text(), 'original')
        index = subprocess.run(['git', '-C', str(self.path), 'ls-files', '-s'],
                               check=True, capture_output=True, text=True).stdout
        self.assertEqual(len(index.splitlines()), 1)
        self.assertEqual(self.git('worktree', 'list', '--porcelain').count('worktree '), 2)

    def test_preexisting_worktree_without_index_needs_review(self):
        self.git('worktree', 'add', '--no-checkout', '-b', 'codex-agent/worker',
                 str(self.path), 'HEAD')
        (self.path / 'user-file.txt').write_text('keep')
        with self.assertRaisesRegex(ValueError, 'repair the checkout, then resume this worker'):
            create_worker_worktree(self.repo, self.path, self.path, 'codex-agent/worker')
        self.assertEqual((self.path / 'user-file.txt').read_text(), 'keep')

    def test_timeout_preserves_staged_changes_from_checkout_hook(self):
        def run(cmd, **kwargs):
            subprocess.run(cmd, **kwargs)
            (self.path / 'tracked.txt').write_text('from hook')
            subprocess.run(['git', '-C', str(self.path), 'add', 'tracked.txt'], check=True)
            raise subprocess.TimeoutExpired(cmd, kwargs['timeout'])
        with self.assertRaisesRegex(ValueError, 'index differs from HEAD'):
            create_worker_worktree(self.repo, self.path, self.path,
                                   'codex-agent/worker', run=run, sleep=lambda _: None)
        self.assertEqual((self.path / 'tracked.txt').read_text(), 'from hook')

    def test_failed_new_checkout_removes_only_its_own_partial_folder(self):
        def run(cmd, **kwargs):
            insert = cmd.index('add') + 1
            subprocess.run([*cmd[:insert], '--no-checkout', *cmd[insert:]], **kwargs)
            raise subprocess.CalledProcessError(1, cmd)
        with self.assertRaisesRegex(WorktreeNeedsReview, 'removed its new partial folder'):
            create_worker_worktree(self.repo, self.path, self.path,
                                   'codex-agent/worker', run=run)
        self.assertFalse(self.path.exists())
        self.assertEqual(subprocess.run(['git', '-C', str(self.repo), 'show-ref', '--verify',
                                         '--quiet', 'refs/heads/codex-agent/worker']).returncode, 1)

    def test_twice_timed_out_checkout_removes_its_new_partial_folder(self):
        original_run = subprocess.run
        original_checkout = _run_checkout
        def add_timeout(cmd, **kwargs):
            insert = cmd.index('add') + 1
            original_run([*cmd[:insert], '--no-checkout', *cmd[insert:]], **kwargs)
            raise subprocess.TimeoutExpired(cmd, kwargs['timeout'])
        def reset_timeout(cmd, **kwargs):
            if 'reset' in cmd:
                raise subprocess.TimeoutExpired(cmd, kwargs['timeout'])
            return original_checkout(cmd, **kwargs)
        with patch('codex_worktree_creation._run_checkout', side_effect=reset_timeout):
            with self.assertRaisesRegex(WorktreeNeedsReview, 'removed its new partial folder'):
                create_worker_worktree(self.repo, self.path, self.path,
                                       'codex-agent/worker', run=add_timeout)
        self.assertFalse(self.path.exists())

    def test_failed_checkout_preserves_foreign_file(self):
        def run(cmd, **kwargs):
            insert = cmd.index('add') + 1
            subprocess.run([*cmd[:insert], '--no-checkout', *cmd[insert:]], **kwargs)
            (self.path / 'user-file.txt').write_text('keep')
            raise subprocess.CalledProcessError(1, cmd)
        with self.assertRaises(WorktreeNeedsReview):
            create_worker_worktree(self.repo, self.path, self.path,
                                   'codex-agent/worker', run=run)
        self.assertEqual((self.path / 'user-file.txt').read_text(), 'keep')

    def test_failed_checkout_preserves_changed_tracked_file(self):
        def run(cmd, **kwargs):
            insert = cmd.index('add') + 1
            subprocess.run([*cmd[:insert], '--no-checkout', *cmd[insert:]], **kwargs)
            (self.path / 'tracked.txt').write_text('user change')
            raise subprocess.CalledProcessError(1, cmd)
        with self.assertRaises(WorktreeNeedsReview):
            create_worker_worktree(self.repo, self.path, self.path,
                                   'codex-agent/worker', run=run)
        self.assertEqual((self.path / 'tracked.txt').read_text(), 'user change')

    def test_failed_checkout_keeps_preexisting_empty_directory(self):
        self.path.mkdir(parents=True)
        def run(cmd, **kwargs):
            insert = cmd.index('add') + 1
            subprocess.run([*cmd[:insert], '--no-checkout', *cmd[insert:]], **kwargs)
            raise subprocess.CalledProcessError(1, cmd)
        with self.assertRaises(WorktreeNeedsReview):
            create_worker_worktree(self.repo, self.path, self.path,
                                   'codex-agent/worker', run=run)
        self.assertTrue(self.path.exists())

    def test_explicit_base_commit_is_used_after_repository_head_moves(self):
        base = self.git('rev-parse', 'HEAD')
        (self.repo / 'tracked.txt').write_text('newer')
        self.git('add', 'tracked.txt')
        self.git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                 'commit', '-m', 'Newer')
        create_worker_worktree(self.repo, self.path, self.path,
                               'codex-agent/worker', base_commit=base)
        head = subprocess.run(['git', '-C', str(self.path), 'rev-parse', 'HEAD'],
                              check=True, capture_output=True, text=True).stdout.strip()
        self.assertEqual(head, base)
        self.assertEqual((self.path / 'tracked.txt').read_text(), 'original')

    def test_unregistered_content_is_preserved(self):
        self.path.mkdir(parents=True)
        (self.path / 'keep.txt').write_text('keep')
        with self.assertRaisesRegex(ValueError, 'unregistered content'):
            create_worker_worktree(self.repo, self.path, self.path, 'codex-agent/worker')
        self.assertEqual((self.path / 'keep.txt').read_text(), 'keep')

    def test_incomplete_registered_checkout_is_not_adopted(self):
        self.git('worktree', 'add', '-b', 'codex-agent/worker', str(self.path), 'HEAD')
        (self.path / 'tracked.txt').unlink()
        with self.assertRaisesRegex(ValueError, 'checkout is incomplete'):
            create_worker_worktree(self.repo, self.path, self.path, 'codex-agent/worker')
        self.assertFalse((self.path / 'tracked.txt').exists())

    def test_scanner_counts_only_worker_folders_and_warns_at_limit(self):
        state = self.root / 'state'
        state.mkdir()
        db = sqlite3.connect(state / 'canvas.sqlite3')
        db.execute('CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
        self.path.mkdir(parents=True)
        (self.path / 'tracked.txt').write_bytes(b'x' * 8192)
        (self.path / 'link.txt').hardlink_to(self.path / 'tracked.txt')
        child = self.path / '.worktrees' / 'codex-agents' / 'child'
        child.mkdir(parents=True)
        (child / 'child.txt').write_bytes(b'z' * 4096)
        for row in ({'id': 'worker', 'cwd': str(self.repo), 'worktree': True, 'isLead': False},
                    {'id': 'child', 'cwd': str(child), 'worktree': True, 'isLead': False},
                    {'id': 'missing', 'cwd': str(self.repo), 'worktree': True, 'isLead': False},
                    {'id': 'lead', 'cwd': str(self.repo), 'worktree': False, 'isLead': True}):
            db.execute('INSERT INTO runtime_agents VALUES (?,?)', (row['id'], json.dumps(row)))
        db.commit()
        db.close()
        scanner = WorktreeDiskScanner(state, pause=lambda _: None)
        with patch('codex_worktree_disk._measure_worktree', wraps=_measure_worktree) as measure:
            order = scanner.scan_once(priority_ids=['child', 'missing'])
            scanner.scan_once()
        self.assertEqual(measure.call_count, 2, 'unchanged worktrees use the path cache')
        self.assertEqual(measure.call_args_list[0].args[0], child)
        self.assertEqual(order[:2], ['child', 'missing'])
        with patch('codex_worktree_disk._measure_worktree', wraps=_measure_worktree) as measure:
            (self.path / 'new-root-entry').write_bytes(b'changed')
            scanner.scan_once()
        self.assertEqual(measure.call_count, 1, 'a changed root signature invalidates its cache')
        with patch.dict(os.environ, {'CODEX_WORKTREE_DISK_LIMIT_BYTES': '1'}):
            result = scanner.snapshot()
        self.assertIn('worker', result['workers'])
        self.assertIn('child', result['workers'])
        self.assertEqual(result['workers']['missing']['state'], 'missing')
        self.assertNotIn('lead', result['workers'])
        self.assertGreater(result['totalBytes'], 0)
        self.assertTrue(result['warning'])
        self.assertEqual(result['totalBytes'], sum(
            row.get('bytes', 0) for row in result['workers'].values()
            if row['state'] == 'ready'))
        self.assertIn(result['workers']['worker']['measure'],
                      ('private on APFS', 'allocated blocks'))
        self.assertEqual(set(scanner.cache), {str(self.path), str(child)})
        with patch('codex_worktree_disk.scanner', return_value=scanner):
            by_agent, scoped = management_view(SimpleNamespace(root=state), [{'id': 'worker'}])
        self.assertEqual(scoped['totalBytes'], by_agent['worker']['bytes'])
        self.assertLess(scoped['totalBytes'], scoped['allWorkersBytes'])

    def test_apfs_private_measure_excludes_a_clone_fixture(self):
        if sys.platform != 'darwin':
            self.skipTest('ATTR_CMNEXT_PRIVATESIZE requires macOS')
        root = self.root / 'apfs-clone-fixture'
        root.mkdir()
        source = root / 'source.bin'
        clone = root / 'clone.bin'
        source.write_bytes(os.urandom(32768))
        subprocess.run(['cp', '-c', str(source), str(clone)], check=True)
        source_private = _apfs_private_bytes(source)
        clone_private = _apfs_private_bytes(clone)
        if source_private is None or clone_private is None:
            self.skipTest('Temporary volume does not support APFS private-size attributes')
        measured, measure = _measure_worktree(root, pause=lambda _: None)
        allocated = _allocated_bytes(root, pause=lambda _: None)
        self.assertEqual(measure, 'private on APFS')
        self.assertEqual(measured, source_private + clone_private)
        self.assertLess(measured, allocated, 'the clone shares allocated extents')

    def test_non_apfs_uses_allocated_blocks(self):
        self.path.mkdir(parents=True)
        (self.path / 'file.bin').write_bytes(b'x' * 2048)
        with patch('codex_worktree_disk._apfs_private_bytes', return_value=None):
            measured, measure = _measure_worktree(self.path, pause=lambda _: None)
        self.assertEqual(measure, 'allocated blocks')
        self.assertEqual(measured, _allocated_bytes(self.path, pause=lambda _: None))


if __name__ == '__main__':
    unittest.main()
