#!/usr/bin/env python3
"""Idle history steps do not open a runtime writer for finished budget pages."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from collections import Counter
from contextlib import contextmanager
import importlib.util
import json
from pathlib import Path
import sqlite3
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'idle_history_fixture', Path(__file__).with_name('analytics-history-lock-order-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_analytics_history as history
import codex_runtime


class IdleHistoryCpu(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.HistoryLockOrder()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.runtime = self.fixture.runtime
        self.agent = self.fixture.agent
        self.key = 'budgetUsageMigrationV1:' + self.agent['id']
        for _ in range(4):
            self.runtime.analytics_history_step()

    def observe_connections(self):
        counts = Counter()
        original = codex_runtime.sqlite_connect

        def connect(*args, **options):
            counts[options.get('site')] += 1
            return original(*args, **options)

        return patch.object(codex_runtime, 'sqlite_connect', side_effect=connect), counts

    def add_usage(self, response, amount, *, agent=None):
        owner = agent or self.agent['id']
        record = {'threadId': self.fixture.thread, 'turnId': 'older-turn', 'at': 2,
                  'responseId': response, 'rawTokenUsageRecord': {'response_id': response},
                  'requestUsage': {'totalTokens': amount}, 'total': {'totalTokens': amount},
                  'last': {'totalTokens': amount}, 'timestampSource': 'record'}
        with self.runtime.analytics_db() as db:
            db.execute('INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) '
                       'VALUES (?,?,?,?,?,?,?)', (response, owner, owner, self.fixture.thread,
                       'older-turn', 2, json.dumps(record)))

    def reset_migration(self):
        with self.runtime.analytics_db() as db:
            end = db.execute('SELECT MAX(seq) FROM analytics_usage').fetchone()[0]
            db.execute('UPDATE analytics_meta SET value=? WHERE key=?',
                       (json.dumps({'cursor': 0, 'end': end}), self.key))

    def saved_migration(self):
        with self.runtime.analytics_db() as db:
            return db.execute('SELECT value FROM analytics_meta WHERE key=?',
                              (self.key,)).fetchone()[0]

    def budget(self):
        with self.runtime.read_db() as db:
            row = json.loads(db.execute('SELECT record FROM runtime_budget WHERE id=?',
                                       (self.agent['id'],)).fetchone()[0])
            receipts = tuple(tuple(row) for row in db.execute(
                'SELECT id,tokens FROM runtime_budget_usage WHERE agent=? ORDER BY id',
                (self.agent['id'],)))
            actor = self.runtime.agent(self.agent['id'], db)
        return row, receipts, actor

    def test_finished_pages_do_not_open_runtime_connections(self):
        observing, counts = self.observe_connections()
        before = self.budget()
        with observing:
            self.assertEqual([self.runtime.analytics_history_step() for _ in range(3)],
                             [False, False, False])
        self.assertEqual(counts['Runtime.db'], 0)
        self.assertEqual(counts['Runtime.analytics'], 3)
        self.assertEqual(self.budget(), before)

    def test_empty_page_advances_without_a_runtime_connection(self):
        self.add_usage('foreign-response', 35, agent='another-agent')
        with self.runtime.analytics_db() as db:
            previous = json.loads(db.execute('SELECT value FROM analytics_meta WHERE key=?',
                                            (self.key,)).fetchone()[0])
            end = db.execute('SELECT MAX(seq) FROM analytics_usage').fetchone()[0]
            db.execute('UPDATE analytics_meta SET value=? WHERE key=?',
                       (json.dumps({'cursor': previous['end'], 'end': end}), self.key))
        observing, counts = self.observe_connections()
        before = self.budget()
        with observing:
            self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(counts['Runtime.db'], 0)
        state = json.loads(self.saved_migration())
        self.assertEqual(state['cursor'], state['end'])
        self.assertEqual(self.budget(), before)

    def test_missing_usage_table_does_not_enter_budget_context(self):
        entered = []

        @contextmanager
        def unopened():
            entered.append(True)
            self.fail('An absent usage table opened the budget writer')
            yield

        with sqlite3.connect(':memory:') as db:
            self.assertFalse(history.migrate_budget_usage(db, self.agent, budget_db=unopened()))
        self.assertEqual(entered, [])

    def test_nonempty_page_commits_budget_before_analytics_checkpoint(self):
        self.add_usage('new-exact-response', 35)
        self.reset_migration()
        phases = []
        with self.runtime.analytics_history_db() as db:
            @contextmanager
            def budget_scope():
                self.assertFalse(db.in_transaction)
                phases.append('open')
                with self.runtime.db() as budget_db:
                    yield budget_db
                self.assertFalse(db.in_transaction)
                phases.append('committed')

            self.assertTrue(history.migrate_budget_usage(db, self.agent, budget_db=budget_scope()))
            self.assertEqual(phases, ['open', 'committed'])
            self.assertTrue(db.in_transaction)
            self.assertEqual(self.budget()[0]['spent'], 135)
        first = self.budget()
        with self.runtime.analytics_history_db() as db:
            self.assertFalse(history.migrate_budget_usage(db, self.agent, budget_db=budget_scope()))
        self.assertEqual(phases, ['open', 'committed'])
        self.assertEqual(self.budget(), first)

    def test_open_connection_argument_retains_legacy_behavior(self):
        self.add_usage('legacy-response', 35)
        self.reset_migration()
        with self.runtime.analytics_history_db() as db:
            with self.runtime.db() as budget_db:
                self.assertTrue(history.migrate_budget_usage(db, self.agent, budget_db=budget_db))
                self.assertTrue(budget_db.in_transaction)
        self.assertEqual(self.budget()[0]['spent'], 135)

    def test_failed_capture_rolls_back_budget_and_retains_checkpoint(self):
        self.add_usage('first-new-response', 35)
        self.add_usage('second-new-response', 36)
        self.reset_migration()
        before_budget, before_migration = self.budget(), self.saved_migration()
        original = history.budget_capture

        def fail_second(db, agent, payload, **options):
            if payload.get('responseId') == 'second-new-response':
                raise RuntimeError('Private second capture failure')
            return original(db, agent, payload, **options)

        with patch.object(history, 'budget_capture', side_effect=fail_second):
            with self.assertRaisesRegex(RuntimeError, 'second capture failure'):
                with self.runtime.analytics_history_db() as db:
                    history.migrate_budget_usage(db, self.agent, budget_db=self.runtime.db())
        self.assertEqual(self.budget(), before_budget)
        self.assertEqual(self.saved_migration(), before_migration)
        with self.runtime.analytics_history_db() as db:
            self.assertTrue(history.migrate_budget_usage(db, self.agent, budget_db=self.runtime.db()))
        self.assertEqual(self.budget()[0]['spent'], 171)

    def test_lost_analytics_checkpoint_retries_exact_budget_once(self):
        self.add_usage('committed-response', 35)
        self.reset_migration()
        before_migration = self.saved_migration()

        class FailedCheckpoint:
            def __init__(self, db):
                self.db = db

            def execute(self, sql, *args):
                if sql.startswith('INSERT INTO analytics_meta'):
                    raise RuntimeError('Private lost analytics checkpoint')
                return self.db.execute(sql, *args)

        with self.assertRaisesRegex(RuntimeError, 'lost analytics checkpoint'):
            with self.runtime.analytics_history_db() as db:
                history.migrate_budget_usage(FailedCheckpoint(db), self.agent,
                                             budget_db=self.runtime.db())
        committed = self.budget()
        self.assertEqual(committed[0]['spent'], 135)
        self.assertEqual(self.saved_migration(), before_migration)
        with self.runtime.analytics_history_db() as db:
            self.assertTrue(history.migrate_budget_usage(db, self.agent, budget_db=self.runtime.db()))
        self.assertEqual(self.budget(), committed)
        state = json.loads(self.saved_migration())
        self.assertEqual(state['cursor'], state['end'])


if __name__ == '__main__':
    unittest.main()
