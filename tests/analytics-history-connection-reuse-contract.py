#!/usr/bin/env python3
"""History phases reuse one connection and retain separate transaction boundaries."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from collections import Counter
from contextlib import closing
import importlib.util
import os
from pathlib import Path
import sqlite3
import subprocess
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'history_fixture', Path(__file__).with_name('analytics-history-lock-order-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
import codex_analytics_history as history
import codex_runtime


class HistoryConnectionReuse(unittest.TestCase):
    def setUp(self):
        source = os.environ.get('STUDIO_HISTORY_REUSE_SOURCE')
        if source:
            from codex_source import source_function
            for name in ('analytics_history_db', 'analytics_history_step'):
                function, _ = source_function(Path(source).read_bytes(),
                    ('AnalyticsHistoryMixin', name), vars(history), '<history-before>')
                replacing = patch.object(history.AnalyticsHistoryMixin, name, function)
                replacing.start()
                self.addCleanup(replacing.stop)
        self.fixture = fixture.HistoryLockOrder()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.runtime = self.fixture.runtime
        for _ in range(4):
            self.runtime.analytics_history_step()

    def observe_connections(self):
        counts, attachments, connections = Counter(), Counter(), []
        original = codex_runtime.sqlite_connect

        def connect(*args, **options):
            site = options.get('site')
            counts[site] += 1
            db = original(*args, **options)
            connections.append(db)
            db.set_trace_callback(lambda sql: attachments.update((site,))
                                  if sql.startswith('ATTACH DATABASE') else None)
            return db

        return patch.object(codex_runtime, 'sqlite_connect', side_effect=connect), counts, attachments, connections

    def test_idle_steps_open_one_analytics_connection_each(self):
        observing, counts, attachments, _ = self.observe_connections()
        with observing:
            self.assertEqual([self.runtime.analytics_history_step() for _ in range(3)],
                             [False, False, False])
        self.assertEqual(counts['Runtime.analytics'], 3)
        self.assertEqual(attachments['Runtime.analytics'], 3)
        self.assertEqual(counts['Runtime.db'], 3)

    def test_each_phase_commits_and_failed_phase_rolls_back(self):
        with history._history_step_connections(self.runtime):
            with self.runtime.analytics_history_db() as first:
                first.execute("INSERT INTO analytics_meta VALUES ('reuse-commit','yes')")
            self.assertFalse(first.in_transaction)
            with closing(sqlite3.connect(self.runtime.analytics_db_path)) as observer:
                self.assertEqual(observer.execute(
                    "SELECT value FROM analytics_meta WHERE key='reuse-commit'").fetchone(), ('yes',))
            with self.assertRaisesRegex(RuntimeError, 'private phase failure'):
                with self.runtime.analytics_history_db() as second:
                    self.assertIs(first, second)
                    second.execute("INSERT INTO analytics_meta VALUES ('reuse-rollback','no')")
                    raise RuntimeError('private phase failure')
            self.assertFalse(first.in_transaction)
            with self.runtime.analytics_history_db() as third:
                self.assertIs(first, third)
                self.assertIsNone(third.execute(
                    "SELECT value FROM analytics_meta WHERE key='reuse-rollback'").fetchone())
        with self.assertRaises(sqlite3.ProgrammingError):
            first.execute('SELECT 1')
        self.assertIsNone(getattr(self.runtime._analytics_history_connections, 'step', None))

    def test_step_error_closes_the_owner_connection(self):
        database = None
        with self.assertRaisesRegex(RuntimeError, 'private step failure'):
            with history._history_step_connections(self.runtime):
                with self.runtime.analytics_history_db() as database:
                    database.execute("INSERT INTO analytics_meta VALUES ('reuse-error','yes')")
                raise RuntimeError('private step failure')
        with self.assertRaises(sqlite3.ProgrammingError):
            database.execute('SELECT 1')
        self.assertIsNone(getattr(self.runtime._analytics_history_connections, 'step', None))

    def test_a_saved_context_cannot_outlive_its_step(self):
        with history._history_step_connections(self.runtime):
            with self.runtime.analytics_history_db() as database:
                database.execute('SELECT 1')
            context = self.runtime.analytics_history_db()
        with self.assertRaisesRegex(RuntimeError, 'scope changed'):
            with context:
                self.fail('A released step was admitted')

    def test_file_read_keeps_both_database_writers_free(self):
        entered, release = threading.Event(), threading.Event()
        errors, observed = [], []
        original = Path.open
        path = self.fixture.path

        def gated_open(target, *args, **options):
            if target == path and args and args[0] == 'rb':
                state = self.runtime._analytics_history_connections.step
                observed.append((state['depth'], state['db'].in_transaction))
                entered.set()
                if not release.wait(3):
                    raise TimeoutError('Private file gate expired')
            return original(target, *args, **options)

        def import_step():
            try:
                self.runtime.analytics_history_step()
            except BaseException as error:
                errors.append(error)

        with patch.object(Path, 'open', gated_open):
            worker = threading.Thread(target=import_step)
            worker.start()
            try:
                self.assertTrue(entered.wait(3))
                for db_path in (self.runtime.db_path, self.runtime.analytics_db_path):
                    db = sqlite3.connect(db_path, timeout=.1)
                    try:
                        db.execute('BEGIN IMMEDIATE')
                        db.rollback()
                    finally:
                        db.close()
            finally:
                release.set()
                worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(observed, [(0, False)])

    def test_state_path_or_opener_change_cannot_reuse_another_scope(self):
        with history._history_step_connections(self.runtime):
            with self.runtime.analytics_history_db() as first:
                first.execute('SELECT 1')
            for name, value in (('db_path', self.fixture.root / 'other.sqlite3'),
                                ('analytics_db_path', self.fixture.root / 'other-analytics.sqlite3'),
                                ('root', self.fixture.root / 'other-state')):
                with self.subTest(name=name), patch.object(self.runtime, name, value):
                    with self.assertRaisesRegex(RuntimeError, 'scope changed'):
                        with self.runtime.analytics_history_db():
                            self.fail('Another state path was admitted')
            with patch.object(self.runtime, 'analytics_db', lambda: None):
                with self.assertRaisesRegex(RuntimeError, 'scope changed'):
                    with self.runtime.analytics_history_db():
                        self.fail('Another database opener was admitted')
            self.assertFalse(first.in_transaction)
        self.assertFalse((self.fixture.root / 'other.sqlite3').exists())
        self.assertFalse((self.fixture.root / 'other-analytics.sqlite3').exists())

    def test_a_context_cannot_move_the_connection_to_another_thread(self):
        errors = []
        with history._history_step_connections(self.runtime):
            with self.runtime.analytics_history_db() as first:
                first.execute('SELECT 1')
            context = self.runtime.analytics_history_db()

            def enter_context():
                try:
                    with context:
                        self.fail('The owner connection crossed threads')
                except BaseException as error:
                    errors.append(error)

            worker = threading.Thread(target=enter_context)
            worker.start()
            worker.join(3)
            self.assertFalse(worker.is_alive())
            self.assertEqual(len(errors), 1)
            self.assertIs(type(errors[0]), RuntimeError)
            self.assertIn('scope changed', str(errors[0]))
            self.assertFalse(first.in_transaction)

    def test_other_callers_keep_independent_connections(self):
        observing, counts, _, _ = self.observe_connections()
        with observing:
            with self.runtime.analytics_history_db() as first:
                first.execute('SELECT 1')
            with self.runtime.analytics_history_db() as second:
                second.execute('SELECT 1')
        self.assertIsNot(first, second)
        self.assertEqual(counts['Runtime.analytics'], 2)

    def test_nested_schema_setup_uses_the_same_owner_connection(self):
        observing, counts, _, _ = self.observe_connections()
        with observing, history._history_step_connections(self.runtime):
            with self.runtime.analytics_history_db() as database:
                self.runtime.analytics_history_init(database)
                self.assertEqual(database.execute('SELECT 1').fetchone()[0], 1)
        self.assertEqual(counts['Runtime.analytics'], 1)

    def test_an_old_active_step_finishes_before_the_next_reused_step(self):
        from codex_source import source_function
        root = Path(__file__).resolve().parents[1]
        source = subprocess.check_output(
            ['git', 'show', '641753ec:scripts/codex_analytics_history.py'], cwd=root)
        old_step, _ = source_function(source,
            ('AnalyticsHistoryMixin', 'analytics_history_step'), vars(history), '<old-history-step>')
        entered, release = threading.Event(), threading.Event()
        errors, outcomes, old_states = [], [], []
        original = Path.open

        def gated_open(target, *args, **options):
            if target == self.fixture.path and args and args[0] == 'rb':
                local = getattr(self.runtime, '_analytics_history_connections', None)
                old_states.append(getattr(local, 'step', None))
                entered.set()
                if not release.wait(3):
                    raise TimeoutError('Private old frame gate expired')
            return original(target, *args, **options)

        def import_old_step():
            try:
                outcomes.append(old_step(self.runtime))
            except BaseException as error:
                errors.append(error)

        with patch.object(Path, 'open', gated_open):
            worker = threading.Thread(target=import_old_step)
            worker.start()
            try:
                self.assertTrue(entered.wait(3))
            finally:
                release.set()
                worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(outcomes, [False])
        self.assertEqual(old_states, [None])
        observing, counts, _, _ = self.observe_connections()
        with observing:
            self.assertFalse(self.runtime.analytics_history_step())
        self.assertEqual(counts['Runtime.analytics'], 1)


if __name__ == '__main__':
    unittest.main()
