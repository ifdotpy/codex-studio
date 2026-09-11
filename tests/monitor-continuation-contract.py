#!/usr/bin/env python3
"""Terminal monitor receipts continue current work and preserve historical stops."""
import importlib.util
import json
import sqlite3
from pathlib import Path
import sys
import time
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('monitor_fixture', Path(__file__).with_name('monitor-lifecycle-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class MonitorContinuation(unittest.TestCase):
    setUp = f.MonitorLifecycleContract.setUp
    tearDown = f.MonitorLifecycleContract.tearDown
    monitor = f.MonitorLifecycleContract.monitor
    record = f.MonitorLifecycleContract.record
    exits = f.MonitorLifecycleContract.exits

    def starts(self):
        return [p for method, p in self.server.calls if method == 'turn/start']

    def assert_delivered(self, key, before):
        f.fixture.eventually(lambda: len(self.starts()) == before + 1)
        f.fixture.eventually(lambda: self.exits(key)[0]['status'] == 'delivered')
        self.assertIn(key, self.starts()[-1]['input'][0]['text'])
        self.assertEqual(len(self.exits(key)), 1)

    def seed_terminal(self, status='cancelled', *, epoch=None, wake_on='exit'):
        m = self.runtime.monitor(self.agent['id'], {'command': 'never-run', 'wake_on': wake_on})
        with self.runtime.lock, self.runtime.db() as db:
            m.update(status=status, exitCode=0 if status == 'completed' else 137,
                     finished=time.time() - 3600, cancelRequested=status == 'cancelled')
            if epoch is not None:
                m['epoch'] = epoch
            self.runtime.put(db, 'monitors', m)
        return m

    def test_normal_exit_continues_owner_once(self):
        key = self.monitor()
        before = len(self.starts())
        self.server.finish(key, 0)
        self.assert_delivered(key, before)
        self.runtime.finish_monitor(key, 0, None)
        self.assertEqual(len(self.exits(key)), 1)
        self.assertEqual(len(self.starts()), before + 1)

    def test_running_cancellation_continues_owner_after_actual_exit(self):
        key = self.monitor()
        before = len(self.starts())
        self.runtime.cancel_monitor(key)
        self.assertEqual(self.exits(key), [])
        self.server.finish(key, 137)
        self.assert_delivered(key, before)
        self.assertEqual(json.loads(self.exits(key)[0]['text'])['exitCode'], 137)

    def test_cancel_approval_and_starting_monitors_delivers_without_command(self):
        for status in ('approval', 'starting'):
            with self.subTest(status=status):
                current = self.runtime.agent(self.agent['id'])
                if current.get('turnId'):
                    self.server.complete(current['threadId'], current['turnId'])
                m = self.runtime.monitor(self.agent['id'], {'command': 'never-run'})
                with self.runtime.lock, self.runtime.db() as db:
                    m['status'] = status
                    self.runtime.put(db, 'monitors', m)
                before = len(self.starts())
                self.runtime.cancel_monitor(m['id'])
                self.assert_delivered(m['id'], before)
                payload = json.loads(self.exits(m['id'])[0]['text'])
                self.assertEqual(payload['status'], 'cancelled')
                self.assertIsNone(payload['exitCode'])
                self.runtime.cancel_monitor(m['id'])
                self.assertEqual(len(self.exits(m['id'])), 1)
        self.assertEqual(self.server.commands, {})

    def test_paused_or_old_epoch_cancellation_records_without_continuation(self):
        key = self.monitor()
        before = len(self.starts())
        self.runtime.stop(self.agent['id'])
        self.server.finish(key, 137)
        f.fixture.eventually(lambda: bool(self.exits(key)))
        self.assertEqual(self.exits(key)[0]['status'], 'cancelled')
        self.assertEqual(self.exits(key)[0]['epoch'], self.agent['epoch'])
        self.assertEqual(len(self.starts()), before)

    def test_startup_restores_history_without_resurrecting_completed_work(self):
        completed = self.seed_terminal('completed')
        cancelled = self.seed_terminal('cancelled')
        stale = self.seed_terminal('cancelled', epoch=-1)
        quiet = self.seed_terminal('completed', wake_on='failure')
        lost = self.seed_terminal('lost')
        self.runtime.close()
        self.runtime = f.Runtime(Path(self.temp.name), f.MonitorServer)
        self.assertEqual(self.runtime.agent(self.agent['id'])['status'], 'completed')
        self.assertTrue(self.runtime.agent(self.agent['id'])['autoWake'])
        self.assertFalse(any(method == 'turn/start' for server in self.runtime.servers.values()
                             for method, _ in server.calls))
        for m in (completed, cancelled, stale):
            event = self.exits(m['id'])[0]
            self.assertEqual(event['status'], 'cancelled')
            self.assertEqual(event['epoch'], m['epoch'])
            self.assertEqual(event['created'], m['finished'])
        self.assertEqual(self.exits(quiet['id']), [])
        self.assertEqual(self.exits(lost['id']), [])
        with self.runtime.lock, self.runtime.db() as db:
            self.assertEqual(self.runtime.recover_monitor_receipts(db), [])

    def test_explicit_recovery_wakes_only_exact_current_owner_once(self):
        current = self.seed_terminal()
        stale = self.seed_terminal(epoch=-1)
        before = len(self.starts())
        with self.runtime.lock, self.runtime.db() as db:
            restored = self.runtime.recover_monitor_receipts(db, keys=[current['id']], wake=True)
            self.assertEqual(restored, [current['id']])
        self.assert_delivered(current['id'], before)
        self.assertEqual(self.exits(stale['id']), [])
        with self.runtime.lock, self.runtime.db() as db:
            self.assertEqual(self.runtime.recover_monitor_receipts(db, keys=[current['id']], wake=True), [])
            self.assertEqual(self.runtime.recover_monitor_receipts(db, keys=[stale['id']], wake=True), [stale['id']])
        self.assertEqual(self.exits(stale['id'])[0]['status'], 'cancelled')
        self.assertEqual(len(self.starts()), before + 1)

    def test_late_duplicate_terminal_result_repairs_receipt_without_wake(self):
        m = self.seed_terminal()
        before = len(self.starts())
        self.runtime.finish_monitor(m['id'], 0, None)
        payload = json.loads(self.exits(m['id'])[0]['text'])
        self.assertEqual(payload['exitCode'], 137)
        self.assertEqual(payload['status'], 'cancelled')
        self.assertEqual(self.exits(m['id'])[0]['status'], 'cancelled')
        self.assertEqual(len(self.starts()), before)

    def test_scheduler_retries_native_exit_after_transient_full_disk(self):
        key = self.monitor()
        before = len(self.starts())
        original_put = self.runtime.put
        failed = []
        def put(db, table, record):
            if table == 'monitors' and record['id'] == key and record['status'] == 'failed' and not failed:
                failed.append(record['exitCode'])
                raise sqlite3.OperationalError('database or disk is full')
            return original_put(db, table, record)
        self.runtime.put = put
        self.server.finish(key, 23)
        f.fixture.eventually(lambda: bool(getattr(self.runtime, 'pending_monitor_results', {}).get(key)))
        self.assertEqual(self.record(key)['status'], 'running')
        self.assertEqual(self.exits(key), [])
        self.assert_delivered(key, before)
        self.assertEqual(failed, [23])
        self.assertEqual(self.record(key)['exitCode'], 23)
        self.assertIsNone(self.record(key)['error'])
        self.assertEqual(len(self.server.commands), 1)
        self.assertNotIn(key, self.runtime.pending_monitor_results)

    def test_failed_read_retains_first_native_result_and_bounds_retry(self):
        key = self.monitor()
        operation = self.record(key)['operation']
        original_agent = self.runtime.agent
        with self.runtime.lock:
            def unavailable(*args, **kwargs):
                raise OSError('No space left on device')
            self.runtime.agent = unavailable
            try:
                self.runtime.monitor_accepted(key, operation, {'exitCode': 17})
                receipt = self.runtime.pending_monitor_results[key]
                self.assertEqual(receipt['code'], 17)
                self.assertEqual(receipt['attempts'], 1)
                self.runtime.finish_monitor(key, None, 'A later error must not replace native exit')
                self.runtime.retry_monitor_results()
                self.assertEqual(receipt['attempts'], 1)
                for _ in range(8):
                    receipt['retryAt'] = 0
                    started = time.monotonic()
                    with self.assertRaisesRegex(OSError, 'persistence pending'):
                        self.runtime.retry_monitor_results()
                    self.assertGreaterEqual(receipt['retryAt'], started + 1)
                    self.assertLessEqual(receipt['retryAt'], time.monotonic() + 30)
            finally:
                self.runtime.agent = original_agent
            self.assertEqual(self.record(key)['status'], 'running')
            self.assertEqual(self.exits(key), [])
            receipt['retryAt'] = 0
            self.runtime.retry_monitor_results()
        f.fixture.eventually(lambda: self.record(key)['status'] == 'failed')
        self.assertEqual(self.record(key)['exitCode'], 17)
        self.assertIsNone(self.record(key)['error'])
        self.assertEqual(self.record(key)['finished'], receipt['finished'])
        self.assertEqual(len(self.server.commands), 1)
        self.assertEqual(len(self.exits(key)), 1)

    def test_pending_cancel_receipt_does_not_wake_owner_stopped_during_disk_failure(self):
        key = self.monitor()
        operation = self.record(key)['operation']
        self.runtime.cancel_monitor(key)
        original_put = self.runtime.put
        with self.runtime.lock:
            def unavailable(db, table, record):
                if table == 'monitors' and record['id'] == key:
                    raise sqlite3.OperationalError('database or disk is full')
                return original_put(db, table, record)
            self.runtime.put = unavailable
            try:
                self.runtime.monitor_accepted(key, operation, {'exitCode': 137})
            finally:
                self.runtime.put = original_put
            before = len(self.starts())
            self.runtime.stop(self.agent['id'])
            self.runtime.pending_monitor_results[key]['retryAt'] = 0
            self.runtime.retry_monitor_results()
        self.assertEqual(self.record(key)['exitCode'], 137)
        self.assertEqual(self.record(key)['status'], 'cancelled')
        self.assertEqual(self.exits(key)[0]['status'], 'cancelled')
        self.assertEqual(len(self.starts()), before)

    def test_pending_native_result_cannot_replace_disconnect_unknown(self):
        key = self.monitor()
        operation = self.record(key)['operation']
        original_agent = self.runtime.agent
        with self.runtime.lock:
            self.runtime.agent = lambda *args, **kwargs: (_ for _ in ()).throw(OSError('No space left'))
            try:
                self.runtime.monitor_accepted(key, operation, {'exitCode': 0})
            finally:
                self.runtime.agent = original_agent
            self.runtime.disconnected('default', self.runtime.connection_ids['default'])
            self.runtime.pending_monitor_results[key]['retryAt'] = 0
            self.runtime.retry_monitor_results()
        self.assertEqual(self.record(key)['status'], 'lost')
        self.assertIsNone(self.record(key)['exitCode'])
        self.assertEqual(self.exits(key), [])
        self.assertNotIn(key, self.runtime.pending_monitor_results)

    def test_timeout_storage_failure_keeps_lease_and_registers_late_native_result(self):
        self.server.timeout_commands = True
        original_unknown = self.runtime.monitor_unknown
        attempts = []
        def unavailable(*args, **kwargs):
            attempts.append(args[0])
            raise sqlite3.OperationalError('database or disk is full')
        self.runtime.monitor_unknown = unavailable
        try:
            key = self.monitor()
            f.fixture.eventually(lambda: bool(attempts))
            self.assertEqual(self.record(key)['status'], 'running')
            self.assertEqual(self.exits(key), [])
            before = len(self.starts())
            self.server.finish(key, 7)
            self.assert_delivered(key, before)
        finally:
            self.runtime.monitor_unknown = original_unknown
        self.assertEqual(self.record(key)['exitCode'], 7)
        self.assertIsNone(self.record(key)['error'])
        self.assertEqual(len(self.server.commands), 1)


if __name__ == '__main__':
    unittest.main(verbosity=2)
