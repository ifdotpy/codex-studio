#!/usr/bin/env python3
"""History worker liveness and recovery. Only local temporary files and SQLite."""
import contextlib
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
from pathlib import Path
import sqlite3
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('history_fixture', Path(__file__).with_name('analytics-history-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_analytics import AnalyticsMixin
from codex_runtime import AppServer, Runtime


def eventually(check, timeout=3):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(.01)
    raise AssertionError('History worker did not reach the required state')


class MeasuredFixture(AnalyticsMixin, f.Fixture):
    def __init__(self, root):
        super().__init__(root)
        with self.db() as db:
            db.row_factory = sqlite3.Row
            self.analytics_init(db)


class WorkerTests(unittest.TestCase):
    def setUp(self):
        import tempfile
        self.temp = tempfile.TemporaryDirectory()
        self.f = f.Fixture(self.temp.name)
        self.f.factory = AppServer
        self.f.path.write_bytes(f.line('session_meta', {'id': f.THREAD}) +
            f.line('response_item', {'type': 'message', 'role': 'user', 'content': []}))

    def tearDown(self):
        self.f.closed = True
        worker = getattr(self.f, 'analytics_history_thread', None)
        if worker:
            worker.join(2)
            self.assertFalse(worker.is_alive())
        self.temp.cleanup()

    def test_legacy_missing_state_and_schema_are_created_by_the_worker(self):
        for name in ('_analytics_history_guard', '_analytics_history_cursor', '_analytics_history_paths', '_analytics_history_schema_ready'):
            delattr(self.f, name)
        with self.f.db() as db:
            db.execute('DROP TABLE analytics_history')
        self.assertTrue(self.f.analytics_history_ensure_running())
        eventually(lambda: len(self.f.captured()) == 1)
        self.assertEqual(self.f.state()['status'], 'current')
        self.assertFalse(self.f.analytics_history_ensure_running())

    def test_existing_active_worker_and_state_keep_their_identities(self):
        stop = threading.Event()
        worker = threading.Thread(target=stop.wait)
        worker.start()
        self.f.analytics_history_thread = worker
        guard = self.f._analytics_history_guard
        paths = self.f._analytics_history_paths
        self.f._analytics_history_cursor = 17
        try:
            self.assertFalse(self.f.analytics_history_ensure_running())
            with self.f.db() as db:
                self.f.analytics_history_init(db)
            self.assertIs(self.f.analytics_history_thread, worker)
            self.assertIs(self.f._analytics_history_guard, guard)
            self.assertIs(self.f._analytics_history_paths, paths)
            self.assertEqual(self.f._analytics_history_cursor, 17)
        finally:
            stop.set()

    def test_dead_worker_restarts_without_resetting_import_state(self):
        old = threading.Thread(target=lambda: None)
        old.start()
        old.join()
        self.f.analytics_history_thread = old
        guard = self.f._analytics_history_guard
        paths = self.f._analytics_history_paths
        self.assertTrue(self.f.analytics_history_ensure_running())
        eventually(lambda: len(self.f.captured()) == 1)
        self.assertIsNot(self.f.analytics_history_thread, old)
        self.assertIs(self.f._analytics_history_guard, guard)
        self.assertIs(self.f._analytics_history_paths, paths)

    def test_concurrent_health_checks_start_one_worker(self):
        entered, release = threading.Event(), threading.Event()
        def step():
            entered.set()
            release.wait(2)
            return False
        with patch.object(self.f, 'analytics_history_step', side_effect=step):
            try:
                with ThreadPoolExecutor(max_workers=8) as pool:
                    results = list(pool.map(lambda _: self.f.analytics_history_ensure_running(), range(32)))
                self.assertEqual(sum(results), 1)
                self.assertTrue(entered.wait(1))
            finally:
                release.set()

    def test_import_and_error_write_failures_keep_worker_alive_until_recovery(self):
        original_db = self.f.db
        @contextlib.contextmanager
        def broken_db():
            raise sqlite3.OperationalError('Fixture storage failure')
            yield
        self.f.db = broken_db
        self.assertTrue(self.f.analytics_history_ensure_running())
        eventually(lambda: getattr(self.f, 'analytics_history_health', {}).get('consecutiveFailures', 0) >= 2)
        worker = self.f.analytics_history_thread
        self.assertTrue(worker.is_alive())
        self.assertFalse(self.f.analytics_history_health['errorPersisted'])
        self.assertIn('OperationalError', self.f.analytics_history_health['error'])
        self.assertIn('Fixture storage failure', self.f.analytics_history_health['errorPersistenceError'])
        self.f.db = original_db
        eventually(lambda: len(self.f.captured()) == 1)
        self.assertIs(self.f.analytics_history_thread, worker)
        self.assertEqual(self.f.analytics_history_health['status'], 'running')

    def test_successful_step_clears_saved_error_and_preserves_its_details(self):
        original = self.f.analytics_history_step
        calls = []
        def step():
            calls.append(True)
            if len(calls) == 1:
                raise OSError('Fixture read failure')
            return original()
        with patch.object(self.f, 'analytics_history_step', side_effect=step):
            self.assertTrue(self.f.analytics_history_ensure_running())
            eventually(lambda: getattr(self.f, 'analytics_history_health', {}).get('errorPersisted') is True)
            def recovered():
                with self.f.db() as db:
                    row = db.execute("SELECT record FROM analytics_history WHERE id='importer'").fetchone()
                return row and json.loads(row[0]).get('status') == 'current'
            eventually(recovered)
        with self.f.db() as db:
            diagnostic = json.loads(db.execute("SELECT record FROM analytics_history WHERE id='importer'").fetchone()[0])
        self.assertIsNone(diagnostic['error'])
        self.assertIn('Fixture read failure', diagnostic['lastError'])
        self.assertEqual(len(self.f.captured()), 1)

    def test_fake_factory_and_closed_runtime_do_not_start_background_work(self):
        self.f.factory = object()
        self.assertFalse(self.f.analytics_history_ensure_running())
        self.assertFalse(hasattr(self.f, 'analytics_history_thread'))
        self.f.factory = AppServer
        self.f.closed = True
        self.assertFalse(self.f.analytics_history_ensure_running())
        self.assertFalse(hasattr(self.f, 'analytics_history_thread'))

    def test_failed_thread_start_can_be_retried(self):
        with patch('codex_analytics_history.threading.Thread.start', side_effect=RuntimeError('Fixture thread failure')):
            self.assertFalse(self.f.analytics_history_ensure_running())
        self.assertIn('History worker could not start', self.f.analytics_history_health['error'])
        self.assertIn('Fixture thread failure', self.f.analytics_history_health['error'])
        self.assertFalse(hasattr(self.f, 'analytics_history_thread'))
        self.assertTrue(self.f.analytics_history_ensure_running())
        eventually(lambda: len(self.f.captured()) == 1)

    def test_runtime_closes_after_thread_start_failure_without_another_retry(self):
        runtime = Runtime(Path(self.temp.name) / 'runtime-state', server_factory=lambda *args: None)
        try:
            with patch('codex_analytics_history.threading.Thread.start', side_effect=RuntimeError('Fixture thread failure')):
                self.assertFalse(runtime.analytics_history_start())
            self.assertFalse(hasattr(runtime, 'analytics_history_thread'))
        finally:
            runtime.close()
        self.assertTrue(runtime.closed)

    def test_worker_advances_existing_budget_and_terminal_error_migrations(self):
        self.f = MeasuredFixture(self.temp.name)
        self.f.factory = AppServer
        self.f.path.write_bytes(f.line('session_meta', {'id': f.THREAD}))
        with self.f.db() as db:
            db.execute('DROP INDEX analytics_usage_migration')
            db.execute('CREATE TABLE runtime_items (id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)')
            usage = {'threadId': f.THREAD, 'turnId': 'old', 'at': 1, 'responseId': 'response',
                     'rawTokenUsageRecord': {'response_id': 'response'}, 'last': {'totalTokens': 100}}
            db.execute('INSERT INTO analytics_usage(id,agent,at,record) VALUES (?,?,?,?)', ('old', 'agent', 1, json.dumps(usage)))
            terminal = {'id': 'agent:' + f.THREAD + ':old', 'agentId': 'agent', 'threadId': f.THREAD,
                        'turnId': 'old', 'status': 'completed', 'error': None}
            db.execute('INSERT INTO analytics_turns VALUES (?,?,?,?,?)', (terminal['id'], 'agent', 'agent', 1, json.dumps(terminal)))
            db.execute('INSERT INTO runtime_items VALUES (?,?,?,?)', ('agent:native-notice:error:old', 'agent',
                json.dumps({'turnId': 'old', 'nativeError': {'message': 'Usage limit reached'}}), 1))
        self.assertTrue(self.f.analytics_history_ensure_running())
        def complete():
            with self.f.db() as db:
                a = self.f.agent('agent', db)
                error = json.loads(db.execute('SELECT record FROM analytics_turns').fetchone()[0])
                return a.get('tokensUsed') == 100 and error['status'] == 'failed'
        eventually(complete)
        with self.f.db() as db:
            self.assertIsNotNone(db.execute("SELECT 1 FROM sqlite_master WHERE name='analytics_usage_migration'").fetchone())
            budget_state = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='budgetUsageMigrationV1:agent'").fetchone()[0])
            state = json.loads(db.execute("SELECT value FROM analytics_meta WHERE key='terminalErrorRepairV1'").fetchone()[0])
        self.assertEqual(budget_state['cursor'], budget_state['end'])
        self.assertEqual(state['repaired'], 1)
        self.assertEqual(self.f.state()['status'], 'current')


if __name__ == '__main__':
    unittest.main()
