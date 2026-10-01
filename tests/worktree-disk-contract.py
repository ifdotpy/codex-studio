#!/usr/bin/env python3
"""Worker worktree retry and disk accounting contracts."""

import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_worktree_creation import create_worker_worktree
from codex_worktree_disk import WorktreeDiskScanner, _allocated_bytes, management_view


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
        self.assertEqual(calls, [60, 180])
        self.assertEqual(self.git('worktree', 'list', '--porcelain').count('worktree '), 2)

    def test_timeout_adopts_only_a_verified_checkout(self):
        def run(cmd, **kwargs):
            subprocess.run(cmd, **kwargs)
            raise subprocess.TimeoutExpired(cmd, kwargs['timeout'])
        self.assertTrue(create_worker_worktree(self.repo, self.path, self.path,
                                               'codex-agent/worker', run=run, sleep=lambda _: None))
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
        scanner.scan_once()
        with patch.dict(os.environ, {'CODEX_WORKTREE_DISK_LIMIT_BYTES': '1'}):
            result = scanner.snapshot()
        self.assertIn('worker', result['workers'])
        self.assertIn('child', result['workers'])
        self.assertEqual(result['workers']['missing']['state'], 'missing')
        self.assertNotIn('lead', result['workers'])
        self.assertGreater(result['totalBytes'], 0)
        self.assertTrue(result['warning'])
        self.assertEqual(result['totalBytes'], _allocated_bytes(self.path, pause=lambda _: None))
        with patch('codex_worktree_disk.scanner', return_value=scanner):
            by_agent, scoped = management_view(SimpleNamespace(root=state), [{'id': 'worker'}])
        self.assertEqual(scoped['totalBytes'], by_agent['worker']['bytes'])
        self.assertLess(scoped['totalBytes'], scoped['allWorkersBytes'])


if __name__ == '__main__':
    unittest.main()
