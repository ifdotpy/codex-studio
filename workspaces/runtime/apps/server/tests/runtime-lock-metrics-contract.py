#!/usr/bin/env python3
"""Opt-in Runtime.lock metrics keep reentrancy and Condition behavior."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

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
                self.assertTrue(condition.wait(timeout=1))
                notified.set()

        thread = threading.Thread(target=wait_for_notification)
        thread.start()
        self.assertTrue(ready.wait(timeout=1))
        with condition:
            condition.notify()
        thread.join(timeout=1)
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

    def test_runtime_lock_is_uninstrumented_without_opt_in(self):
        home = os.environ.get("HOME", str(Path.home()))
        with patch.dict(os.environ, {"HOME": home}, clear=True):
            self.assertNotIsInstance(runtime_lock(), MeasuredRLock)
        with patch.dict(os.environ, {"HOME": home, "CODEX_RUNTIME_LOCK_METRICS": "1"}, clear=True):
            self.assertIsInstance(runtime_lock(), MeasuredRLock)

    def test_runtime_uses_the_opt_in_metrics_lock_in_temp_state(self):
        with tempfile.TemporaryDirectory(prefix="studio-lock-metrics-") as folder:
            with patch.dict(os.environ, {"CODEX_RUNTIME_LOCK_METRICS": "1",
                                         "HOME": folder, "CODEX_HOME": folder}, clear=True):
                runtime = Runtime(folder, server_factory=lambda *_args: None)
                try:
                    self.assertIsInstance(runtime.lock, MeasuredRLock)
                    with runtime.lock:
                        pass
                    self.assertTrue(runtime.lock.runtime_lock_metrics())
                finally:
                    runtime.close()


if __name__ == "__main__":
    unittest.main()
