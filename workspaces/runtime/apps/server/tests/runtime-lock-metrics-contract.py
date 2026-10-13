#!/usr/bin/env python3
"""Runtime.lock metrics keep reentrancy, bounded samples, and Condition behavior."""

from test_isolation import isolate_supervisor_environment, minimal_runtime_environment

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from server_test_waits import HANG_GUARD_SECONDS
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

isolate_supervisor_environment()
from runtime_test_environment import make_runtime

import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_lock_metrics import MeasuredRLock, SAMPLE_LIMIT, runtime_lock
from codex_runtime import Runtime


class RuntimeLockMetricsContract(unittest.TestCase):
    def test_outer_reentrant_acquisition_reports_callsite_and_percentiles(self):
        lock = MeasuredRLock()
        with lock:
            with lock:
                time.sleep(.001)
        metrics = lock.runtime_lock_metrics()
        self.assertEqual(len(metrics), 1)
        row = metrics[0]
        self.assertEqual(row["count"], 1)
        self.assertIn("runtime-lock-metrics-contract.py:test_outer_reentrant_acquisition", row["callSite"])
        self.assertEqual(row["waitMs"]["samples"], 1)
        self.assertGreaterEqual(row["holdMs"]["max"], 1)
        self.assertEqual(row["holdMs"]["p50"], row["holdMs"]["p95"])

    def test_condition_wait_releases_and_restores_the_wrapped_lock(self):
        lock = MeasuredRLock()
        condition = threading.Condition(lock)
        ready = threading.Event()
        notified = threading.Event()

        def wait_for_notification():
            with condition:
                ready.set()
                self.assertTrue(condition.wait(timeout=HANG_GUARD_SECONDS))
                notified.set()

        thread = threading.Thread(target=wait_for_notification)
        thread.start()
        self.assertTrue(ready.wait(timeout=HANG_GUARD_SECONDS))
        with condition:
            condition.notify()
        thread.join(timeout=HANG_GUARD_SECONDS)
        self.assertTrue(notified.is_set())
        self.assertFalse(thread.is_alive())
        rows = lock.runtime_lock_metrics()
        self.assertTrue(any(row["count"] >= 1 for row in rows))

    def test_samples_are_bounded_and_cumulative_counts_remain_exact(self):
        lock = MeasuredRLock()
        for _ in range(SAMPLE_LIMIT + 5):
            with lock:
                pass
        row = lock.runtime_lock_metrics()[0]
        self.assertEqual(row["count"], SAMPLE_LIMIT + 5)
        self.assertEqual(row["waitMs"]["samples"], SAMPLE_LIMIT)
        self.assertEqual(row["holdMs"]["samples"], SAMPLE_LIMIT)

    def test_concurrent_acquisitions_preserve_exact_callsite_count(self):
        lock = MeasuredRLock()
        workers, per_worker = 8, 250

        def acquire_many():
            for _ in range(per_worker):
                with lock:
                    pass

        threads = [threading.Thread(target=acquire_many) for _ in range(workers)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=HANG_GUARD_SECONDS)
            self.assertFalse(thread.is_alive())
        rows = lock.runtime_lock_metrics()
        self.assertEqual(sum(row["count"] for row in rows), workers * per_worker)
        self.assertEqual(len(rows), 1)

    def test_runtime_lock_is_instrumented_without_opt_in(self):
        home = os.environ.get("HOME", str(Path.home()))
        with patch.dict(os.environ, {"HOME": home}, clear=True):
            self.assertIsInstance(runtime_lock(), MeasuredRLock)

    def test_runtime_uses_metrics_lock_in_temp_state(self):
        with tempfile.TemporaryDirectory(prefix="studio-lock-metrics-") as folder:
            environment = minimal_runtime_environment(folder)
            runtime = make_runtime(Runtime, folder, server_factory=lambda *_args: None, environment_overrides=environment)
            try:
                self.assertIsInstance(runtime.lock, MeasuredRLock)
                with runtime.lock:
                    pass
                self.assertTrue(runtime.lock.runtime_lock_metrics())
            finally:
                runtime.close()


if __name__ == "__main__":
    unittest.main()
