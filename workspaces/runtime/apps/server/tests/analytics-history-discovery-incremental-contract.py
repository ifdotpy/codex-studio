#!/usr/bin/env python3
"""Directory changes invalidate discovery; Claude does not read Codex rollouts."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from collections import Counter
import importlib.util
import json
import os
from pathlib import Path
import shutil
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('incremental_path_fixture',
    Path(__file__).with_name('analytics-rollout-path-cache-contract.py'))
paths = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paths)
import codex_analytics_history as history


class IncrementalDiscovery(paths.RolloutPathCache):
    def advance(self):
        self.clock[0] += 31

    def test_unchanged_directories_are_checked_without_entry_enumeration(self):
        self.assertEqual(self.lookup(), (self.path, None))
        original, checked = history._history_directory_signature, []
        with patch.object(history, '_history_directory_signature',
                side_effect=lambda path, **options: checked.append(str(path)) or original(path, **options)), \
                patch.object(history.os, 'scandir', side_effect=AssertionError('Unchanged entries enumerated')):
            for _ in range(3):
                self.advance()
                self.assertEqual(self.lookup(), (self.path, None))
        self.assertEqual(len(checked), 15)
        self.assertEqual(set(Counter(checked).values()), {3})

    def test_only_the_changed_directory_is_enumerated(self):
        self.assertEqual(self.lookup(), (self.path, None))
        (self.directory / ('rollout-' + 'a' * 36 + '.jsonl')).write_text('Later file.\n')
        original, scanned = os.scandir, []
        with patch.object(history.os, 'scandir',
                side_effect=lambda path: scanned.append(str(path)) or original(path)):
            self.advance()
            self.assertEqual(self.lookup(), (self.path, None))
        self.assertEqual(scanned, [str(self.directory)])

    def test_deleting_a_file_drops_its_cached_candidate(self):
        self.assertEqual(self.lookup(), (self.path, None))
        self.path.unlink()
        self.advance()
        self.assertEqual(self.lookup(), (None, 'missing'))

    def test_removed_subtree_drops_all_old_candidates(self):
        self.assertEqual(self.lookup(), (self.path, None))
        shutil.rmtree(self.directory)
        self.advance()
        self.assertEqual(self.lookup(), (None, 'missing'))
        self.assertNotIn(str(self.directory), self.runtime._analytics_history_paths[str(self.home)][2])

    def test_replaced_directory_does_not_follow_a_new_symlink(self):
        self.assertEqual(self.lookup(), (self.path, None))
        outside = Path(self.temp.name) / 'outside'
        outside.mkdir()
        (outside / self.path.name).write_text('Outside the account.\n')
        shutil.rmtree(self.directory)
        self.directory.symlink_to(outside, target_is_directory=True)
        self.advance()
        self.assertEqual(self.lookup(), (None, 'missing'))

    def test_legacy_two_field_cache_rebuilds_directory_proof(self):
        self.assertEqual(self.lookup(), (self.path, None))
        cache = self.runtime._analytics_history_paths[str(self.home)]
        self.runtime._analytics_history_paths[str(self.home)] = cache[:2]
        original, scanned = os.scandir, []
        with patch.object(history.os, 'scandir',
                side_effect=lambda path: scanned.append(str(path)) or original(path)):
            self.advance()
            self.assertEqual(self.lookup(), (self.path, None))
        self.assertEqual(len(scanned), 5)

    def test_a_changed_scan_is_repeated_next_discovery_round(self):
        original, changed = os.scandir, []
        late = self.directory / ('rollout-' + 'b' * 36 + '.jsonl')
        def scan(path):
            scanner = original(path)
            if path == self.directory and not changed:
                class RacingScanner:
                    def __enter__(self):
                        entries = list(scanner.__enter__())
                        late.write_text('Created after enumeration.\n')
                        changed.append(True)
                        return iter(entries)
                    def __exit__(self, *args):
                        return scanner.__exit__(*args)
                return RacingScanner()
            return scanner
        with patch.object(history.os, 'scandir', side_effect=scan):
            self.assertEqual(self.lookup(), (self.path, None))
        self.assertIsNone(self.runtime._analytics_history_paths[str(self.home)][2][str(self.directory)][0])
        self.advance()
        self.assertEqual(self.runtime._analytics_rollout_path(self.home, 'b' * 36), (late, None))

    def test_failed_scan_drops_partial_matches_and_recovers(self):
        self.assertEqual(self.lookup(), (self.path, None))
        (self.directory / 'changed').touch()
        original = os.scandir
        def scan(path):
            if path == self.directory:
                raise PermissionError('Private fixture directory error')
            return original(path)
        with patch.object(history.os, 'scandir', side_effect=scan):
            self.advance()
            self.assertEqual(self.lookup(), (None, 'missing'))
        self.advance()
        self.assertEqual(self.lookup(), (self.path, None))

    def test_profile_home_symlink_preserves_the_managed_scope(self):
        alias = Path(self.temp.name) / 'profile-alias'
        alias.symlink_to(self.home, target_is_directory=True)
        expected = alias / self.path.relative_to(self.home)
        self.assertEqual(self.lookup(alias), (expected, None))
        self.advance()
        self.assertEqual(self.lookup(alias), (expected, None))

    def test_root_directory_symlink_keeps_contained_files(self):
        archive = self.home / 'archived_sessions'
        target = self.home / 'native-archive'
        archive.replace(target)
        archive.symlink_to(target, target_is_directory=True)
        archived = archive / self.path.name
        self.path.replace(archived)
        self.assertEqual(self.lookup(), (archived, None))
        self.advance()
        self.assertEqual(self.lookup(), (archived, None))

    def test_final_containment_check_remains_fresh_inside_discovery_interval(self):
        self.assertEqual(self.lookup(), (self.path, None))
        outside = Path(self.temp.name) / 'outside.jsonl'
        outside.write_text('Outside replacement.\n')
        self.path.unlink()
        self.path.symlink_to(outside)
        self.assertEqual(self.lookup(), (None, 'outsideProfile'))


class ClaudeHistory(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location('claude_history_budget_fixture',
            Path(__file__).with_name('analytics-history-lock-order-contract.py'))
        fixture = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture)
        self.fixture = fixture.HistoryLockOrder()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.runtime = self.fixture.runtime
        with self.runtime.db() as db:
            actor = self.runtime.agent(self.fixture.agent['id'], db)
            actor['provider'] = 'claude'
            self.runtime.put(db, 'agents', actor)

    def test_claude_does_not_read_codex_files_or_refresh_authentication(self):
        with patch.object(self.runtime.accounts, 'home',
                side_effect=AssertionError('Claude auth refresh in Codex importer')), \
                patch.object(self.runtime, '_analytics_rollout_path',
                side_effect=AssertionError('Claude actor searched Codex rollouts')):
            self.assertFalse(self.runtime.analytics_history_step())
            self.assertFalse(self.runtime.analytics_history_step())
        with self.runtime.analytics_db() as db:
            state = json.loads(db.execute('SELECT record FROM analytics_history').fetchone()[0])
        self.assertEqual(state['status'], 'liveOnly')
        self.assertIsNone(state['error'])
        self.assertEqual(state['offset'], 0)

    def test_existing_claude_usage_still_migrates_exact_budget_receipts_once(self):
        record = {'threadId': self.fixture.thread, 'turnId': 'claude-turn', 'at': 2,
            'responseId': 'claude-exact-response', 'rawTokenUsageRecord': {'response_id': 'claude-exact-response'},
            'requestUsage': {'totalTokens': 35}, 'total': {'totalTokens': 35},
            'last': {'totalTokens': 35}, 'timestampSource': 'record'}
        with self.runtime.analytics_db() as db:
            db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?)',
                ('claude', self.fixture.agent['id'], self.fixture.agent['id'], self.fixture.thread,
                 'claude-turn', 2, json.dumps(record)))
        self.assertTrue(self.runtime.analytics_history_step())
        self.assertFalse(self.runtime.analytics_history_step())
        with self.runtime.read_db() as db:
            receipts = list(db.execute('SELECT tokens FROM runtime_budget_usage WHERE agent=?',
                (self.fixture.agent['id'],)))
            budget = json.loads(db.execute('SELECT record FROM runtime_budget WHERE id=?',
                (self.fixture.agent['id'],)).fetchone()[0])
        self.assertEqual([row[0] for row in receipts], [35])
        self.assertEqual(budget['spent'], 35)

    def test_provider_transfer_is_read_fresh_before_native_import(self):
        self.assertFalse(self.runtime.analytics_history_step())
        with self.runtime.db() as db:
            actor = self.runtime.agent(self.fixture.agent['id'], db)
            actor['provider'] = 'codex'
            self.runtime.put(db, 'agents', actor)
        self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(self.fixture.usage()[0]['spent'], 100)
        self.assertEqual(self.fixture.usage()[3]['status'], 'current')


if __name__ == '__main__':
    unittest.main()
