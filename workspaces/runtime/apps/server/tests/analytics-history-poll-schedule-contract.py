#!/usr/bin/env python3
"""Archive reconciliation reduces idle work and preserves native checkpoint proof."""

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
import threading
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('history_schedule_fixture',
    Path(__file__).with_name('analytics-history-lock-order-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_analytics_history as history


class HistoryPollSchedule(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.HistoryLockOrder()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.runtime = self.fixture.runtime
        self.clock = [100.0]
        for clock_patch in (patch.object(history.time, 'monotonic', side_effect=lambda: self.clock[0]),
                            patch.object(history.time, 'thread_time', return_value=0)):
            clock_patch.start()
            self.addCleanup(clock_patch.stop)
        self.actors = []
        with self.runtime.db() as db:
            for index in range(65):
                actor = {**self.fixture.agent, 'id': self.fixture.agent['id'] if index == 0 else 'archive-' + str(index),
                         'threadId': self.fixture.thread if index == 0 else str(uuid.UUID(int=index)),
                         'deletedAt': 1, 'status': 'completed', 'inFlight': False}
                db.execute('INSERT OR REPLACE INTO runtime_agents VALUES (?,?)',
                           (actor['id'], json.dumps(actor)))
                self.actors.append(actor)
                if index:
                    self.path(index).write_text(json.dumps({'type': 'session_meta',
                        'payload': {'id': actor['threadId']}}) + '\n')
        for _ in range(3 * len(self.actors)):
            self.runtime.analytics_history_step()
        self.runtime._analytics_history_cursor = 0
        self.runtime.analytics_history_thread = threading.current_thread()
        self.addCleanup(lambda: delattr(self.runtime, 'analytics_history_thread'))

    def path(self, index):
        return self.fixture.home / 'sessions' / ('rollout-' + self.actors[index]['threadId'] + '.jsonl')

    def edit(self, index, **fields):
        with self.runtime.db() as db:
            actor = self.runtime.agent(self.actors[index]['id'], db)
            actor.update(fields)
            self.runtime.put(db, 'agents', actor)
        return actor

    def state(self, index):
        actor = self.actors[index]
        key = actor['id'] + ':' + actor.get('accountKey', 'default') + ':' + actor['threadId']
        with self.runtime.analytics_db() as db:
            return json.loads(db.execute('SELECT record FROM analytics_history WHERE id=?',
                                         (key,)).fetchone()[0])

    def append_usage(self, index, response='schedule-exact', amount=35):
        actor = self.actors[index]
        record = {'type': 'token_usage_record', 'timestamp': '2026-09-06T18:00:01Z', 'payload': {
            'thread_id': actor['threadId'], 'turn_id': 'schedule-turn', 'response_id': response,
            'usage': {'total_tokens': amount}, 'thread_token_usage': {'total_tokens': amount}}}
        with self.path(index).open('a') as stream:
            stream.write(json.dumps(record) + '\n')

    def test_idle_archives_do_not_open_profiles_files_or_actor_rows(self):
        before = self.fixture.usage()
        counts = Counter()
        original = self.runtime.analytics_connection
        from contextlib import contextmanager
        @contextmanager
        def connection(*args, **options):
            with original(*args, **options) as db:
                db.set_trace_callback(lambda sql: counts.update(['sql']))
                yield db
        with patch.object(self.runtime, 'analytics_connection', side_effect=connection), \
                patch.object(self.runtime, 'agent', side_effect=AssertionError('Idle actor SQL')), \
                patch.object(self.runtime.accounts, 'home', side_effect=AssertionError('Idle profile read')), \
                patch.object(Path, 'open', side_effect=AssertionError('Idle archive opened')):
            self.assertEqual([self.runtime.analytics_history_step() for _ in range(4)], [False] * 4)
        self.assertLess(counts['sql'], len(self.actors))
        self.assertEqual(self.fixture.usage(), before)

    def test_existing_worker_adopts_poll_state_without_resetting_its_guard(self):
        guard = self.runtime._analytics_history_guard
        paths = self.runtime._analytics_history_paths
        del self.runtime._analytics_history_polls
        self.assertFalse(self.runtime.analytics_history_step())
        self.assertIs(self.runtime._analytics_history_guard, guard)
        self.assertIs(self.runtime._analytics_history_paths, paths)
        self.assertEqual(len(self.runtime._analytics_history_polls), 32)

    def test_reconciliation_prioritizes_changed_files_and_keeps_budget_exact(self):
        self.append_usage(64)
        self.clock[0] += history.ARCHIVED_RECONCILE_SECONDS
        changed = history._history_file_changed
        def checked(poll):
            connection = self.runtime._analytics_history_connections.step
            self.assertEqual(connection['depth'], 0)
            self.assertFalse(connection['db'].in_transaction)
            self.assertFalse(self.runtime.lock._is_owned())
            return changed(poll)
        with patch.object(history, '_history_file_changed', side_effect=checked), \
                patch.object(self.runtime.accounts, 'home', wraps=self.runtime.accounts.home) as profiles:
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(profiles.call_count, 1)
        self.assertEqual(self.state(64)['offset'], self.path(64).stat().st_size)
        actor = self.actors[64]
        with self.runtime.read_db() as db:
            budget = json.loads(db.execute('SELECT record FROM runtime_budget WHERE id=?',
                                           (actor['id'],)).fetchone()[0])
            receipts = list(db.execute('SELECT tokens FROM runtime_budget_usage WHERE agent=?', (actor['id'],)))
        self.assertEqual(budget['spent'], 35)
        self.assertEqual([row[0] for row in receipts], [35])
        for _ in range(5):
            self.runtime.analytics_history_step()
        with self.runtime.read_db() as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_budget_usage WHERE agent=?',
                                        (actor['id'],)).fetchone()[0], 1)

    def test_new_active_actor_precedes_due_archives(self):
        self.clock[0] += history.ARCHIVED_RECONCILE_SECONDS
        self.append_usage(64)
        self.edit(64, deletedAt=None, status='running', inFlight=True)
        calls = []
        original = self.runtime._analytics_history_import
        def imported(actor, *args):
            calls.append(actor['id'])
            return original(actor, *args)
        with patch.object(self.runtime, '_analytics_history_import', side_effect=imported):
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(calls, [self.actors[64]['id']])

    def test_resume_and_account_move_bypass_the_archive_deadline(self):
        thread = str(uuid.UUID(int=1000))
        actor = self.edit(64, accountKey='fresh-account', threadId=thread, deletedAt=None)
        path = self.fixture.home / 'sessions' / ('rollout-' + thread + '.jsonl')
        path.write_text(json.dumps({'type': 'session_meta', 'payload': {'id': actor['threadId']}}) + '\n')
        self.runtime._analytics_history_paths.clear()
        with patch.object(self.runtime.accounts, 'home', wraps=self.runtime.accounts.home) as profiles:
            self.assertTrue(self.runtime.analytics_history_step())
        profiles.assert_called_once_with('fresh-account')
        with self.runtime.analytics_db() as db:
            row = db.execute('SELECT record FROM analytics_history WHERE id=?',
                (actor['id'] + ':fresh-account:' + thread,)).fetchone()
        self.assertEqual(json.loads(row[0])['status'], 'current')

    def test_new_native_actor_is_discovered_before_the_archive_deadline(self):
        thread = str(uuid.UUID(int=1001))
        actor = {**self.actors[64], 'id': 'new-active', 'threadId': thread,
                 'deletedAt': None, 'status': 'starting', 'inFlight': True}
        with self.runtime.db() as db:
            self.runtime.put(db, 'agents', actor)
        path = self.fixture.home / 'sessions' / ('rollout-' + thread + '.jsonl')
        path.write_text(json.dumps({'type': 'session_meta', 'payload': {'id': thread}}) + '\n')
        self.runtime._analytics_history_paths.clear()
        with patch.object(self.runtime.accounts, 'home', wraps=self.runtime.accounts.home) as profiles:
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(profiles.call_count, 1)
        with self.runtime.analytics_db() as db:
            state = json.loads(db.execute('SELECT record FROM analytics_history WHERE agent=?',
                                          ('new-active',)).fetchone()[0])
        self.assertEqual(state['status'], 'current')

    def test_resume_during_a_round_precedes_the_remaining_archive_rows(self):
        self.clock[0] += history.ARCHIVED_RECONCILE_SECONDS
        self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 32)
        self.clock[0] += history.ACTIVE_RECONCILE_SECONDS
        self.append_usage(64)
        self.edit(64, deletedAt=None, status='running', inFlight=True)
        calls = []
        original = self.runtime._analytics_history_import
        def imported(actor, *args):
            calls.append(actor['id'])
            return original(actor, *args)
        with patch.object(self.runtime, '_analytics_history_import', side_effect=imported):
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(calls, [self.actors[64]['id']])

    def test_active_actor_is_revisited_during_a_long_archive_round(self):
        # An archive flag must not delay an actor that still has a native turn.
        self.edit(0, status='running', inFlight=True)
        self.clock[0] += history.ARCHIVED_RECONCILE_SECONDS
        self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 32)
        self.append_usage(0, 'active-exact-response')
        self.clock[0] += history.ACTIVE_RECONCILE_SECONDS
        calls = []
        original = self.runtime._analytics_history_import
        def imported(actor, *args):
            calls.append(actor['id'])
            return original(actor, *args)
        with patch.object(self.runtime, '_analytics_history_import', side_effect=imported):
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(calls, [self.actors[0]['id']])
        self.assertEqual(self.state(0)['offset'], self.path(0).stat().st_size)

    def test_continuous_active_progress_retains_fair_archive_reconciliation(self):
        for index in (0, 1):
            self.edit(index, deletedAt=None, status='running', inFlight=True)
        self.clock[0] += history.ARCHIVED_RECONCILE_SECONDS
        imported = []
        def advancing(actor, *args):
            imported.append(actor['id'])
            self.clock[0] += history.ACTIVE_RECONCILE_SECONDS
            return True
        with patch.object(self.runtime, '_analytics_history_import', side_effect=advancing):
            for _ in range(24):
                self.assertTrue(self.runtime.analytics_history_step())
        counts = Counter(imported)
        self.assertGreater(counts[self.actors[0]['id']], 1)
        self.assertGreater(counts[self.actors[1]['id']], 1)
        archives = set(imported) - {self.actors[0]['id'], self.actors[1]['id']}
        self.assertGreaterEqual(len(archives), 2)

    def test_reconciliation_retains_replacement_truncation_and_anchor_checks(self):
        before = {index: self.state(index)['offset'] for index in (61, 62, 63)}
        replacement = self.path(61).with_suffix('.replacement')
        replacement.write_bytes(self.path(61).read_bytes())
        os.replace(replacement, self.path(61))
        self.path(62).write_bytes(b'')
        content = self.path(63).read_bytes()
        self.path(63).write_bytes(content.replace(b'session_meta', b'session_metu'))
        self.clock[0] += history.ARCHIVED_RECONCILE_SECONDS
        for _ in range(3):
            self.assertFalse(self.runtime.analytics_history_step())
        for index in before:
            self.assertEqual(self.state(index)['status'], 'identityChanged')
            self.assertEqual(self.state(index)['offset'], before[index])

    def test_catching_up_archive_remains_due_before_reconciliation(self):
        for index in range(4):
            self.append_usage(64, 'page-' + str(index))
        self.clock[0] += history.ARCHIVED_RECONCILE_SECONDS
        self.assertTrue(self.runtime.analytics_history_step(max_records=1))
        self.assertEqual(self.state(64)['status'], 'catchingUp')
        for _ in range(3):
            self.runtime.analytics_history_step(max_records=1)
        self.assertTrue(self.runtime.analytics_history_step(max_records=1))
        self.assertEqual(self.state(64)['status'], 'catchingUp')


if __name__ == '__main__':
    unittest.main()
