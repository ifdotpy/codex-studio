#!/usr/bin/env python3
"""Scheduler wakes preserve dispatch without repeating due maintenance or DB setup."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
import codex_runtime
from codex_runtime import Runtime


class WakeClock:
    def __init__(self, runtime, steps):
        self.runtime = runtime
        self.steps = iter(steps)
        self.now = 100.0
        self.waits = []

    def wait(self, timeout):
        self.waits.append(timeout)
        step = next(self.steps, None)
        if step is None:
            self.runtime.closed = True
            return True
        seconds, woke = step
        self.now += seconds
        return woke

    def clear(self):
        pass

    def set(self):
        pass

    @contextmanager
    def installed(self):
        clock = SimpleNamespace(monotonic=lambda: self.now, time=lambda: self.now)
        with patch.object(codex_runtime, 'time', clock), \
                patch.object(codex_runtime, 'startup_memory_mark'):
            yield


class SchedulerMaintenance(unittest.TestCase):
    def runtime(self):
        rt = Runtime.__new__(Runtime)
        rt.closed = False
        rt.root = Path(self.directory.name)
        rt._retry_dirty_workspace_refresh = lambda: None
        rt._publish_committed_resource_changes = lambda: None
        return rt

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='scheduler-maintenance-')
        self.addCleanup(self.directory.cleanup)

    def exercise(self, steps, failed=None):
        rt = self.runtime()
        seen = []
        clock = WakeClock(rt, steps)
        rt.changed = clock

        def phase(name):
            def run():
                seen.append((name, clock.now))
                if failed == name and sum(item[0] == name for item in seen) == 1:
                    raise sqlite3.OperationalError('fixture maintenance failure')
            return run

        for name in ('monitors_tick', 'rules_tick', 'capacity_tick', 'usage_resume_tick',
                     'accepted_archive_tick', 'retry_monitor_results', 'runtime_maintenance_tick', 'turn_item_links_tick'):
            setattr(rt, name, phase(name))
        rt._cross_server_service = SimpleNamespace(tick=phase('cross_server'))
        rt.dispatch = lambda **options: seen.append(('dispatch', clock.now, options))
        with clock.installed():
            rt.schedule()
        return rt, seen, clock

    def test_wake_storm_dispatches_without_repeating_maintenance(self):
        rt, seen, clock = self.exercise([(.001, True)] * 100)
        dispatched = [item for item in seen if item[0] == 'dispatch']
        self.assertEqual(len(dispatched), 100)
        self.assertTrue(all(item[2] == {'maintenance': False} for item in dispatched))
        for name in ('monitors_tick', 'rules_tick', 'capacity_tick', 'usage_resume_tick',
                     'accepted_archive_tick', 'retry_monitor_results', 'runtime_maintenance_tick', 'turn_item_links_tick',
                     'cross_server'):
            self.assertEqual(sum(item[0] == name for item in seen), 1, name)
        self.assertTrue(all(0 <= wait <= 1 for wait in clock.waits))
        self.assertIsNone(rt.scheduler_error)

    def test_due_maintenance_runs_without_another_dispatch(self):
        _, seen, _ = self.exercise([(.001, True), (1.01, False), (4.01, False)])
        self.assertEqual(sum(item[0] == 'dispatch' for item in seen), 2)
        self.assertEqual(sum(item[0] == 'rules_tick' for item in seen), 3)
        self.assertEqual(sum(item[0] == 'runtime_maintenance_tick' for item in seen), 2)

    def test_turn_item_backfill_has_its_own_completion_deadline(self):
        _, seen, _ = self.exercise([(.001, True), (.20, True), (.06, False), (.20, False), (.06, False)])
        self.assertEqual(sum(item[0] == 'turn_item_links_tick' for item in seen), 3)
        self.assertEqual(sum(item[0] == 'runtime_maintenance_tick' for item in seen), 1)

    def test_failed_phase_does_not_block_dispatch_or_repeat_on_each_wake(self):
        rt, seen, _ = self.exercise([(.001, True)] * 100, failed='rules_tick')
        self.assertEqual(rt.scheduler_error['error'], 'fixture maintenance failure')
        self.assertEqual(sum(item[0] == 'rules_tick' for item in seen), 1)
        rt, seen, _ = self.exercise([(.001, True)] * 100 + [(1.01, False)], failed='rules_tick')
        self.assertEqual(sum(item[0] == 'dispatch' for item in seen), 100)
        self.assertEqual(sum(item[0] == 'rules_tick' for item in seen), 2)
        self.assertEqual(sum(item[0] == 'capacity_tick' for item in seen), 2)
        self.assertIsNone(rt.scheduler_error)

    def test_close_during_dispatch_prevents_later_maintenance(self):
        rt = self.runtime()
        clock = WakeClock(rt, [(.001, True)])
        rt.changed = clock
        seen = []

        def dispatch(**_options):
            seen.append('dispatch')
            rt.closed = True

        rt.dispatch = dispatch
        for name in ('monitors_tick', 'rules_tick', 'capacity_tick', 'usage_resume_tick',
                     'accepted_archive_tick', 'retry_monitor_results', 'runtime_maintenance_tick', 'turn_item_links_tick'):
            setattr(rt, name, lambda: seen.append('late maintenance'))
        with clock.installed():
            rt.schedule()
        self.assertEqual(seen, ['dispatch'])


class QuietRuntime(Runtime):
    def schedule(self):
        pass


class SchedulerConnection(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='scheduler-connection-')
        self.addCleanup(self.directory.cleanup)
        self.rt = QuietRuntime(Path(self.directory.name), server_factory=object)
        self.addCleanup(self.rt.close)

    def configure_dispatch(self, dispatch, *, rules=lambda: None):
        rt = self.rt
        rt._cross_server_service = None
        rt._retry_dirty_workspace_refresh = lambda: None
        rt._publish_committed_resource_changes = lambda: None
        rt.dispatch = dispatch
        for name in ('monitors_tick', 'capacity_tick', 'usage_resume_tick',
                     'accepted_archive_tick', 'retry_monitor_results', 'runtime_maintenance_tick', 'turn_item_links_tick'):
            setattr(rt, name, lambda: None)
        rt.rules_tick = rules

    def drive(self, dispatch, *, rules=lambda: None, steps=None):
        rt = self.rt
        self.configure_dispatch(dispatch, rules=rules)
        clock = WakeClock(rt, steps or [(.001, True), (.001, True)])
        rt.changed = clock
        try:
            with clock.installed():
                Runtime.schedule(rt)
        finally:
            rt.closed = False
            rt.changed = threading.Event()

    def test_reuses_one_owner_connection_with_independent_commit_and_rollback(self):
        connections = []
        commits = []

        def dispatch(**_options):
            with self.rt.db(busy_timeout=7) as db:
                connections.append(db)
                self.assertEqual(db.execute('PRAGMA busy_timeout').fetchone()[0], 7)
                db.execute("INSERT OR IGNORE INTO runtime_tasks VALUES('committed','{}')")
            self.assertFalse(db.in_transaction)
            commits.append(True)

        def rules():
            with self.rt.db() as db:
                connections.append(db)
                self.assertEqual(db.execute('PRAGMA busy_timeout').fetchone()[0], 15000)
                db.execute("INSERT INTO runtime_tasks VALUES('rolled-back','{}')")
                raise sqlite3.OperationalError('fixture transaction failure')

        self.drive(dispatch, rules=rules)
        self.assertEqual(len(commits), 2)
        self.assertEqual(len(set(connections)), 1)
        with self.rt.read_db() as reader:
            self.assertEqual([row[0] for row in reader.execute('SELECT id FROM runtime_tasks')], ['committed'])
        with self.assertRaises(sqlite3.ProgrammingError):
            connections[0].execute('SELECT 1')
        self.assertIsNone(getattr(self.rt._callback_db, 'connection', None))
        self.assertFalse(getattr(self.rt._callback_db, 'reuse', False))

    def test_shutdown_closes_the_connection_on_its_scheduler_thread(self):
        rt = self.rt
        entered = threading.Event()
        owners, releases = [], []
        original_connect = codex_runtime.sqlite_connect

        def connect(*args, **options):
            db = original_connect(*args, **options)
            owners.append(threading.get_ident())
            original_close = db.close

            def close():
                releases.append(threading.get_ident())
                original_close()

            db.close = close
            return db

        def dispatch(**_options):
            with rt.db() as db:
                db.execute('SELECT COUNT(*) FROM runtime_tasks').fetchone()
            self.assertFalse(db.in_transaction)
            entered.set()

        self.configure_dispatch(dispatch)
        with patch.object(codex_runtime, 'sqlite_connect', side_effect=connect):
            worker = threading.Thread(target=lambda: Runtime.schedule(rt))
            rt.scheduler = worker
            worker.start()
            try:
                rt.changed.set()
                self.assertTrue(entered.wait(2))
                rt.closed = True
                rt.changed.set()
                worker.join(2)
                self.assertFalse(worker.is_alive())
            finally:
                rt.closed = True
                rt.changed.set()
                worker.join(5)
                rt.closed = False
        self.assertEqual(owners, [worker.ident])
        self.assertEqual(releases, [worker.ident])

    def test_due_monitor_result_wakes_dispatch_and_delivers_once(self):
        rt = self.rt
        delivered = threading.Event()
        order = []

        def dispatch(**_options):
            with rt.db() as db:
                changed = db.execute("UPDATE runtime_events SET status='delivered' "
                                     "WHERE id='monitor-fixture' AND status='pending'").rowcount
            if changed:
                order.append('dispatch')
                delivered.set()

        def monitor():
            if order:
                return
            order.append('monitor')
            with rt.db() as db:
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) "
                           "VALUES('monitor-fixture', 'fixture', 'monitor_exit', 'test', 'pending', 1, 1)")

        self.configure_dispatch(dispatch)
        rt.monitors_tick = monitor
        worker = threading.Thread(target=lambda: Runtime.schedule(rt))
        rt.scheduler = worker
        worker.start()
        try:
            rt.changed.set()
            self.assertTrue(delivered.wait(2))
        finally:
            rt.closed = True
            rt.changed.set()
            worker.join(5)
            rt.closed = False
        self.assertFalse(worker.is_alive())
        self.assertEqual(order, ['monitor', 'dispatch'])

    def test_unexpected_scheduler_exit_still_closes_its_connection(self):
        rt = self.rt
        connections = []

        class FailedWait(WakeClock):
            def wait(self, timeout):
                if self.waits:
                    raise RuntimeError('fixture wait failure')
                return super().wait(timeout)

        def dispatch(**_options):
            with rt.db() as db:
                connections.append(db)
                db.execute('SELECT 1').fetchone()

        clock = FailedWait(rt, [(.001, True)])
        self.configure_dispatch(dispatch)
        rt.changed = clock
        try:
            with clock.installed(), self.assertRaisesRegex(RuntimeError, 'fixture wait failure'):
                Runtime.schedule(rt)
        finally:
            rt.changed = threading.Event()
        self.assertEqual(len(connections), 1)
        with self.assertRaises(sqlite3.ProgrammingError):
            connections[0].execute('SELECT 1')
        self.assertIsNone(getattr(rt._callback_db, 'connection', None))

    def test_failed_attach_closes_each_partial_connection(self):
        connections, releases = [], []
        original_connect = codex_runtime.sqlite_connect

        def connect(*args, **options):
            db = original_connect(*args, **options)
            connections.append(db)
            execute, close = db.execute, db.close

            def execute_or_fail(sql, *parameters):
                if sql.startswith('ATTACH DATABASE'):
                    raise sqlite3.OperationalError('fixture attach failure')
                return execute(sql, *parameters)

            def release():
                releases.append(db)
                close()

            db.execute, db.close = execute_or_fail, release
            return db

        def dispatch(**_options):
            with self.rt.db():
                self.fail('An attach failure must not enter the transaction body')

        with patch.object(codex_runtime, 'sqlite_connect', side_effect=connect):
            self.drive(dispatch)
        self.assertEqual(len(connections), 2)
        self.assertEqual(releases, connections)
        self.assertIsNone(getattr(self.rt._callback_db, 'connection', None))

    def test_schema_cache_repairs_a_changed_schema_and_keeps_resource_triggers(self):
        statements = []
        connections = []
        calls = []
        original_connect = codex_runtime.sqlite_connect

        def connect(*args, **options):
            db = original_connect(*args, **options)
            db.set_trace_callback(statements.append)
            connections.append(db)
            return db

        def dispatch(**_options):
            calls.append(True)
            with self.rt.db() as db:
                self.assertIsNotNone(db.execute("SELECT 1 FROM sqlite_master WHERE name='sync_entity_event_INSERT'").fetchone())
            if len(calls) == 1:
                writer = sqlite3.connect(self.rt.db_path)
                try:
                    with writer:
                        writer.execute('DROP TRIGGER sync_entity_event_INSERT')
                finally:
                    writer.close()
            with self.rt.db() as db:
                self.assertIsNotNone(db.execute("SELECT 1 FROM sqlite_master WHERE name='sync_entity_event_INSERT'").fetchone())
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) "
                           "VALUES(?, 'fixture', 'user', 'test', 'pending', 1, 1)", (str(len(calls)),))
                db.execute("INSERT INTO runtime_items VALUES(?, 'fixture', '{}', 1)", (str(len(calls)),))

        with patch.object(codex_runtime, 'sqlite_connect', side_effect=connect):
            self.drive(dispatch)
        self.assertEqual(len(connections), 1)
        self.assertEqual(sum(sql.startswith('ATTACH DATABASE') for sql in statements), 1)
        resources = [resource.model_dump() for resource in self.rt._committed_resource_changes.values()]
        self.assertEqual({resource['kind'] for resource in resources}, {'queue', 'receipts', 'transcript'})

    def test_isolated_connection_setup_cpu(self):
        sample_count = 100
        normal_statements = []
        reused_statements = []
        original_connect = codex_runtime.sqlite_connect

        def measure(statements, reusable):
            opened = []

            def connect(*args, **options):
                db = original_connect(*args, **options)
                db.set_trace_callback(statements.append)
                opened.append(db)
                return db

            def transaction(**_options):
                with self.rt.db() as db:
                    db.execute('SELECT COUNT(*) FROM runtime_tasks').fetchone()

            began = time.thread_time()
            with patch.object(codex_runtime, 'sqlite_connect', side_effect=connect):
                if reusable:
                    self.drive(transaction, steps=[(.001, True)] * sample_count)
                else:
                    for _ in range(sample_count):
                        transaction()
            return time.thread_time() - began, len(opened)

        baseline_cpu, baseline_connections = measure(normal_statements, False)
        reused_cpu, reused_connections = measure(reused_statements, True)
        self.assertEqual(baseline_connections, sample_count)
        self.assertEqual(reused_connections, 1)
        schema_checks = [sql for sql in reused_statements if 'sqlite_master' in sql]
        self.assertLess(len(schema_checks), 20)
        print(json.dumps({'fixture': 'scheduler-db-setup', 'contexts': sample_count,
                          'baselineCpuMs': round(baseline_cpu * 1000, 3),
                          'reusedCpuMs': round(reused_cpu * 1000, 3),
                          'baselineConnections': baseline_connections,
                          'reusedConnections': reused_connections,
                          'reusedSchemaChecks': len(schema_checks)}))


if __name__ == '__main__':
    unittest.main()
