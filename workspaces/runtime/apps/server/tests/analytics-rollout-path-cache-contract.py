#!/usr/bin/env python3
"""Rollout discovery reads each directory once and keeps exact profile scope."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from collections import Counter
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
import codex_analytics_history as history

THREAD = '01a07781-5d19-7390-bc74-c12094143962'


class RolloutPathCache(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name) / 'profile-one'
        self.directory = self.home / 'sessions' / '2026' / '10' / '07'
        self.directory.mkdir(parents=True)
        (self.home / 'archived_sessions').mkdir()
        self.path = self.directory / ('rollout-' + THREAD + '.jsonl')
        self.path.write_text('Exact private file. No native execution.\n')
        self.runtime = history.AnalyticsHistoryMixin()
        self.runtime._analytics_history_paths = {}
        self.clock = [0.0]
        self.now = patch.object(history.time, 'monotonic', side_effect=lambda: self.clock[0])
        self.now.start()
        self.addCleanup(self.now.stop)

    def lookup(self, home=None):
        return self.runtime._analytics_rollout_path(home or self.home, THREAD)

    def test_one_directory_read_per_cache_refresh(self):
        original, visited = os.scandir, Counter()
        def scan(path):
            visited[str(path)] += 1
            return original(path)
        with patch.object(history.os, 'scandir', side_effect=scan):
            self.assertEqual(self.lookup(), (self.path, None))
        self.assertEqual(len(visited), 5)
        self.assertEqual(set(visited.values()), {1})

    def test_cache_lifetime_starts_after_slow_scan_completes(self):
        original, visits = os.scandir, []
        def scan(path):
            visits.append(str(path))
            if len(visits) == 1:
                self.clock[0] = 35.0
            return original(path)
        with patch.object(history.os, 'scandir', side_effect=scan):
            self.assertEqual(self.lookup(), (self.path, None))
            count = len(visits)
            self.assertEqual(self.lookup(), (self.path, None))
            self.assertEqual(len(visits), count)
            self.clock[0] = 66.0
            self.assertEqual(self.lookup(), (self.path, None))
            self.assertEqual(self.runtime._analytics_history_paths[str(self.home)][0], 66.0)
            self.assertEqual(len(visits), count)

    def test_missing_file_is_found_after_cache_refresh(self):
        self.path.unlink()
        self.assertEqual(self.lookup(), (None, 'missing'))
        self.path.write_text('A later native file.\n')
        self.assertEqual(self.lookup(), (None, 'missing'))
        self.clock[0] = 31.0
        self.assertEqual(self.lookup(), (self.path, None))

    def test_new_nested_directory_is_found_after_cache_refresh(self):
        self.path.unlink()
        self.assertEqual(self.lookup(), (None, 'missing'))
        moved = self.directory / 'later' / self.path.name
        moved.parent.mkdir()
        moved.write_text('Later nested file.\n')
        self.clock[0] = 31.0
        self.assertEqual(self.lookup(), (moved, None))

    def test_archived_move_is_found_after_cache_refresh(self):
        self.assertEqual(self.lookup(), (self.path, None))
        moved = self.home / 'archived_sessions' / self.path.name
        self.path.replace(moved)
        self.clock[0] = 31.0
        self.assertEqual(self.lookup(), (moved, None))

    def test_new_duplicate_remains_ambiguous(self):
        self.assertEqual(self.lookup(), (self.path, None))
        (self.home / 'archived_sessions' / self.path.name).write_text('Another exact UUID.\n')
        self.clock[0] = 31.0
        self.assertEqual(self.lookup(), (None, 'ambiguous'))

    def test_account_homes_keep_separate_cache_entries(self):
        other = Path(self.temp.name) / 'profile-two'
        other.mkdir()
        self.assertEqual(self.lookup(), (self.path, None))
        self.assertEqual(self.lookup(other), (None, 'missing'))
        self.assertEqual(set(self.runtime._analytics_history_paths), {str(self.home), str(other)})

    def test_symlink_outside_profile_is_rejected(self):
        self.path.unlink()
        outside = Path(self.temp.name) / 'outside.jsonl'
        outside.write_text('Outside the managed profile.\n')
        self.path.symlink_to(outside)
        self.assertEqual(self.lookup(), (None, 'outsideProfile'))

    def test_directory_symlink_is_not_followed(self):
        self.path.unlink()
        outside = Path(self.temp.name) / 'outside-directory'
        outside.mkdir()
        (outside / self.path.name).write_text('Do not traverse this directory.\n')
        (self.directory / 'link').symlink_to(outside, target_is_directory=True)
        self.assertEqual(self.lookup(), (None, 'missing'))

    def test_same_path_replacement_remains_visible_to_identity_checks(self):
        self.assertEqual(self.lookup(), (self.path, None))
        old = self.path.stat().st_ino
        replacement = self.directory / 'new-file'
        replacement.write_text('Replacement.\n')
        replacement.replace(self.path)
        found, problem = self.lookup()
        self.assertIsNone(problem)
        self.assertEqual(found, self.path)
        self.assertNotEqual(found.stat().st_ino, old)

    def test_matching_directory_remains_a_candidate_like_rglob(self):
        self.path.unlink()
        self.path.mkdir()
        self.assertEqual(self.lookup(), (self.path, None))

    def test_missing_root_is_retried_after_creation(self):
        other = Path(self.temp.name) / 'later-profile'
        self.assertEqual(self.lookup(other), (None, 'missing'))
        found = other / 'sessions' / self.path.name
        found.parent.mkdir(parents=True)
        found.write_text('A later profile file.\n')
        self.clock[0] = 31.0
        self.assertEqual(self.lookup(other), (found, None))


if __name__ == '__main__':
    unittest.main()
