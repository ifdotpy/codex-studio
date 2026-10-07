#!/usr/bin/env python3
"""Background history yields CPU after all import scopes release."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import contextlib
import importlib.util
import json
from pathlib import Path
import sqlite3
import threading
import time
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('history_budget_fixture',
    Path(__file__).with_name('analytics-history-lock-order-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_analytics_history as history
import codex_runtime


class Clock:
    def __init__(self, on_sleep=None):
        self.wall = self.cpu = 0.0
        self.sleeps = []
        self.on_sleep = on_sleep
        self.cpu_reads = 0
        self.time = time.time
        self.debt_on_cpu_read = None

    def monotonic(self):
        return self.wall

    def thread_time(self):
        self.cpu_reads += 1
        if self.debt_on_cpu_read and self.cpu_reads == 2:
            self.work(*self.debt_on_cpu_read)
        return self.cpu

    def work(self, cpu, wall):
        self.cpu += cpu
        self.wall += wall

    def sleep(self, seconds):
        self.sleeps.append(seconds)
        if self.on_sleep:
            self.on_sleep(seconds)
        self.wall += seconds


class HistoryCpuBudget(unittest.TestCase):
    def setUp(self):
        self.fixture = f.HistoryLockOrder()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.runtime = self.fixture.runtime
        self.assertTrue(self.runtime.analytics_history_step())
        for _ in range(3):
            self.runtime.analytics_history_step()
        self.assertIs(self.runtime.analytics_history_step(), False)
        self.runtime.analytics_history_thread = threading.current_thread()
        self.addCleanup(lambda: delattr(self.runtime, 'analytics_history_thread'))

    def clocked_step(self, clock, *, cpu=.03, wall=.03):
        original = history.prepare_budget_migration
        def prepare(runtime):
            clock.work(cpu, wall)
            return original(runtime)
        with patch.object(history, 'time', clock), patch.object(
                history, 'prepare_budget_migration', side_effect=prepare):
            return self.runtime.analytics_history_step()

    def assert_scopes_free(self, seconds):
        self.assertGreater(seconds, 0)
        self.assertLessEqual(seconds, .1)
        self.assertFalse(self.runtime.lock._is_owned())
        self.assertTrue(self.runtime._analytics_history_guard.acquire(blocking=False))
        self.runtime._analytics_history_guard.release()
        self.assertIsNone(getattr(self.runtime._analytics_history_connections, 'step', None))
        for path in (self.runtime.db_path, self.runtime.analytics_db_path):
            db = sqlite3.connect(path, timeout=0)
            try:
                db.execute('BEGIN IMMEDIATE')
            finally:
                db.rollback()
                db.close()

    def append_usage(self):
        record = {'type':'token_usage_record','timestamp':'2026-09-06T18:00:01Z',
            'payload':{'thread_id':self.fixture.thread,'turn_id':'turn-one',
                'response_id':'second-response','usage':{'total_tokens':100},
                'thread_token_usage':{'total_tokens':200}}}
        with self.fixture.path.open('a') as stream:
            stream.write(json.dumps(record)+'\n')

    def test_idle_step_waits_only_after_guard_and_all_database_scopes_release(self):
        clock = Clock(self.assert_scopes_free)
        connections, original = [], codex_runtime.sqlite_connect
        def connect(*args, **options):
            value = original(*args, **options)
            connections.append(value)
            return value
        def check(seconds):
            self.assert_scopes_free(seconds)
            self.assertTrue(connections)
            for db in connections:
                with self.assertRaises(sqlite3.ProgrammingError):
                    db.execute('SELECT 1')
        clock.on_sleep = check
        before = self.fixture.usage()
        cursor = self.runtime._analytics_history_cursor
        with patch.object(codex_runtime, 'sqlite_connect', side_effect=connect):
            self.assertIs(self.clocked_step(clock), False)
        self.assertAlmostEqual(sum(clock.sleeps), .17, places=8)
        self.assertEqual(self.fixture.usage(), before)
        self.assertEqual(self.runtime._analytics_history_cursor, cursor)

    def test_advanced_step_commits_exact_checkpoint_and_budget_before_pause(self):
        self.append_usage()
        observed = []
        def check(seconds):
            self.assert_scopes_free(seconds)
            budget, rows, usage, checkpoint = self.fixture.usage()
            observed.append(checkpoint['offset'])
            self.assertEqual(budget['spent'], 200)
            self.assertEqual(checkpoint['offset'], self.fixture.path.stat().st_size)
            self.assertEqual(len(rows), 3)
            self.assertEqual(len(usage), 3)
        clock = Clock(check)
        self.assertIs(self.clocked_step(clock), True)
        self.assertTrue(observed)
        self.assertAlmostEqual(sum(clock.sleeps), .17, places=8)
        before = self.fixture.usage()
        self.assertIs(self.clocked_step(Clock()), False)
        self.assertEqual(self.fixture.usage(), before)

    def test_error_keeps_exception_and_checkpoint_before_pause(self):
        clock = Clock(self.assert_scopes_free)
        failure = RuntimeError('Exact private history failure')
        before = self.fixture.usage()
        def fail(*args):
            clock.work(.03, .03)
            raise failure
        with patch.object(history, 'time', clock), patch.object(
                self.runtime, '_analytics_rollout_path', side_effect=fail):
            with self.assertRaises(RuntimeError) as result:
                self.runtime.analytics_history_step()
        self.assertIs(result.exception, failure)
        self.assertAlmostEqual(sum(clock.sleeps), .17, places=8)
        self.assertEqual(self.fixture.usage(), before)

    def test_close_interrupts_pause_within_one_small_sleep(self):
        def stop(seconds):
            self.assert_scopes_free(seconds)
            self.runtime.closed = True
        clock = Clock(stop)
        try:
            self.assertIs(self.clocked_step(clock, cpu=1, wall=.03), False)
            self.assertEqual(clock.sleeps, [.1])
        finally:
            self.runtime.closed = False

    def test_foreground_step_does_not_read_thread_cpu_or_pause(self):
        self.runtime.analytics_history_thread = object()
        clock = Clock(lambda _: self.fail('The foreground history call paused'))
        self.assertIs(self.clocked_step(clock), False)
        self.assertEqual(clock.cpu_reads, 0)
        self.assertEqual(clock.sleeps, [])

    def test_slow_io_does_not_create_credit_for_the_next_cpu_batch(self):
        clock = Clock(self.assert_scopes_free)
        self.assertIs(self.clocked_step(clock, cpu=.03, wall=10), False)
        self.assertEqual(clock.sleeps, [])
        self.assertIs(self.clocked_step(clock), False)
        self.assertAlmostEqual(sum(clock.sleeps), .17, places=8)

    def test_busy_guard_return_keeps_borrowed_guard_and_is_paced(self):
        clock = Clock()
        clock.debt_on_cpu_read = (.03, .03)
        self.runtime._analytics_history_guard.acquire()
        try:
            def held(seconds):
                self.assertLessEqual(seconds, .1)
                self.assertFalse(self.runtime._analytics_history_guard.acquire(blocking=False))
            clock.on_sleep = held
            with patch.object(history, 'time', clock):
                self.assertIs(self.runtime.analytics_history_step(), False)
            self.assertAlmostEqual(sum(clock.sleeps), .17, places=8)
        finally:
            self.runtime._analytics_history_guard.release()

    def test_real_background_thread_cpu_stays_below_twenty_percent(self):
        result, original = {}, history.prepare_budget_migration
        def prepare(runtime):
            deadline = time.thread_time() + .002
            while time.thread_time() < deadline:
                pass
            return original(runtime)
        def run():
            started, cpu = time.monotonic(), time.thread_time()
            result['returns'] = [self.runtime.analytics_history_step() for _ in range(8)]
            result.update(wall=time.monotonic()-started, cpu=time.thread_time()-cpu)
        worker = threading.Thread(target=run, name='private-history-budget')
        self.runtime.analytics_history_thread = worker
        with patch.object(history, 'prepare_budget_migration', side_effect=prepare):
            worker.start()
            worker.join(5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(result['returns'], [False]*8)
        self.assertGreater(result['cpu'], .015)
        self.assertLessEqual(result['cpu']/result['wall'], .20, result)
        print('history_cpu_ratio', result, flush=True)


if __name__ == '__main__':
    unittest.main()
