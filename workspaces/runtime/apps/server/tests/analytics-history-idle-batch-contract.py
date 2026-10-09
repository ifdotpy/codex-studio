#!/usr/bin/env python3
"""Idle background actors share setup without extending SQL or native scopes."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from collections import Counter
import importlib.util
import json
from pathlib import Path
import sqlite3
import threading
import unittest
import uuid
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('idle_batch_fixture',
    Path(__file__).with_name('analytics-history-lock-order-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_analytics_history as history
import codex_runtime


class IdleBatch(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.HistoryLockOrder()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.runtime = self.fixture.runtime
        self.actors = [self.fixture.agent]
        with self.runtime.db() as db:
            for index in range(64):
                actor = {**self.fixture.agent, 'id': 'batch-' + str(index),
                         'threadId': str(uuid.UUID(int=index + 1))}
                self.runtime.put(db, 'agents', actor)
                self.actors.append(actor)
        # Finish native checkpoints and legacy budget pages before quiet checks.
        for _ in range(2 * len(self.actors)):
            self.runtime.analytics_history_step()
        self.runtime._analytics_history_cursor = 0

    def background(self):
        self.runtime.analytics_history_thread = threading.current_thread()
        self.addCleanup(lambda: delattr(self.runtime, 'analytics_history_thread'))

    def observe_connections(self):
        counts, connections = Counter(), []
        original = codex_runtime.sqlite_connect
        def connect(*args, **options):
            counts[options.get('site')] += 1
            db = original(*args, **options)
            connections.append(db)
            db.set_trace_callback(lambda sql: counts.update(['attach'])
                if sql.startswith('ATTACH DATABASE') else None)
            return db
        return patch.object(codex_runtime, 'sqlite_connect', side_effect=connect), counts, connections

    def assert_closed(self, connections):
        self.assertTrue(connections)
        for db in connections:
            with self.assertRaises(sqlite3.ProgrammingError):
                db.execute('SELECT 1')
        self.assertIsNone(getattr(self.runtime._analytics_history_connections, 'step', None))
        self.assertTrue(self.runtime._analytics_history_guard.acquire(blocking=False))
        self.runtime._analytics_history_guard.release()

    def new_rollout(self, index, amount=35):
        actor = self.actors[index]
        path = self.fixture.home / 'sessions' / ('rollout-' + actor['threadId'] + '.jsonl')
        records = [{'type': 'session_meta', 'payload': {'id': actor['threadId']}},
                   {'type': 'token_usage_record', 'payload': {'thread_id': actor['threadId'],
                    'turn_id': 'batch-turn', 'response_id': 'batch-exact-' + str(index),
                    'usage': {'total_tokens': amount}, 'thread_token_usage': {'total_tokens': amount}}}]
        with path.open('a' if index == 0 else 'w') as stream:
            for record in records[1:] if index == 0 else records:
                stream.write(json.dumps({'timestamp': '2026-09-06T18:00:00Z', **record}) + '\n')
        self.runtime._analytics_history_paths.clear()
        return path

    def test_background_checks_thirty_two_idle_actors_with_one_connection(self):
        before = self.fixture.usage()
        self.background()
        observing, counts, connections = self.observe_connections()
        home = self.runtime.accounts.home
        with observing, patch.object(self.runtime.accounts, 'home', wraps=home) as profiles:
            self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 32)
        self.assertEqual(profiles.call_count, 32)
        self.assertEqual(counts['Runtime.analytics'], 1)
        self.assertEqual(counts['attach'], 1)
        self.assertEqual(counts['Runtime.db'], 0)
        self.assert_closed(connections)
        self.assertEqual(self.fixture.usage(), before)

    def test_foreground_keeps_the_one_actor_contract(self):
        observing, counts, connections = self.observe_connections()
        with observing:
            self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 1)
        self.assertEqual(counts['Runtime.analytics'], 1)
        self.assert_closed(connections)

    def test_progress_stops_the_batch_and_preserves_exact_budget(self):
        self.new_rollout(5)
        self.background()
        observing, counts, connections = self.observe_connections()
        with observing:
            self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 6)
        self.assertEqual(counts['Runtime.analytics'], 1)
        self.assert_closed(connections)
        actor = self.actors[5]
        with self.runtime.read_db() as db:
            budget = json.loads(db.execute('SELECT record FROM runtime_budget WHERE id=?',
                                          (actor['id'],)).fetchone()[0])
            self.assertEqual(budget['spent'], 35)
            count = db.execute('SELECT COUNT(*) FROM runtime_budget_usage WHERE agent=?',
                               (actor['id'],)).fetchone()[0]
            self.assertEqual(count, 1)
        # The batch resumes at the next actor. It does not replay the new receipt.
        self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 38)

    def test_round_end_does_not_cross_into_new_progress(self):
        self.new_rollout(0)
        self.background()
        self.runtime._analytics_history_cursor = 60
        self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 0)
        self.assertTrue(self.runtime.analytics_history_step())
        self.assertEqual(self.runtime._analytics_history_cursor, 1)

    def test_each_actor_reads_its_current_account_and_thread(self):
        self.background()
        target = self.actors[4]
        home, calls = self.runtime.accounts.home, []
        def profile(key):
            calls.append(key)
            if len(calls) == 3:
                with self.runtime.db() as db:
                    current = self.runtime.agent(target['id'], db)
                    current.update(accountKey='new-profile', threadId='fresh-native-thread')
                    self.runtime.put(db, 'agents', current)
            return home(key)
        with patch.object(self.runtime.accounts, 'home', side_effect=profile):
            self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(calls[4], 'new-profile')
        with self.runtime.analytics_db() as db:
            row = db.execute('SELECT record FROM analytics_history WHERE id=?',
                (target['id'] + ':new-profile:fresh-native-thread',)).fetchone()
        self.assertIsNotNone(row)

    def test_error_closes_the_connection_and_releases_the_guard(self):
        self.background()
        original, calls = self.runtime._analytics_rollout_path, []
        failure = RuntimeError('Private middle actor failure')
        def path(*args):
            calls.append(True)
            if len(calls) == 4:
                raise failure
            return original(*args)
        observing, counts, connections = self.observe_connections()
        with observing, patch.object(self.runtime, '_analytics_rollout_path', side_effect=path):
            with self.assertRaises(RuntimeError) as result:
                self.runtime.analytics_history_step()
        self.assertIs(result.exception, failure)
        self.assertEqual(counts['Runtime.analytics'], 1)
        self.assert_closed(connections)

    def test_close_finishes_the_current_actor_and_stops_the_batch(self):
        self.background()
        home, calls = self.runtime.accounts.home, []
        def profile(key):
            calls.append(key)
            if len(calls) == 4:
                self.runtime.closed = True
            return home(key)
        try:
            with patch.object(self.runtime.accounts, 'home', side_effect=profile):
                self.assertFalse(self.runtime.analytics_history_step())
            self.assertEqual(len(calls), 4)
            self.assertEqual(self.runtime._analytics_history_cursor, 4)
        finally:
            self.runtime.closed = False

    def test_no_sql_transaction_survives_between_actor_file_reads(self):
        self.background()
        original, calls = self.runtime._analytics_rollout_path, []
        def path(*args):
            calls.append(True)
            state = self.runtime._analytics_history_connections.step
            self.assertEqual(state['depth'], 0)
            self.assertFalse(state['db'].in_transaction)
            for name in (self.runtime.db_path, self.runtime.analytics_db_path):
                db = sqlite3.connect(name, timeout=0)
                try:
                    db.execute('BEGIN IMMEDIATE')
                finally:
                    db.rollback()
                    db.close()
            return original(*args)
        with patch.object(self.runtime, '_analytics_rollout_path', side_effect=path):
            self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(len(calls), 32)


if __name__ == '__main__':
    unittest.main()
