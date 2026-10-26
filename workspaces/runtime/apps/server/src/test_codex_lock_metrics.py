"""Python lock diagnostics preserve the complete closed API percentile shape."""

import unittest

from codex_lock_metrics import MeasuredRLock


class LockMetricSummaryTests(unittest.TestCase):
    def test_runtime_lock_metrics_include_p99_after_samples(self) -> None:
        lock = MeasuredRLock()
        for _ in range(3):
            with lock:
                pass

        [metric] = lock.runtime_lock_metrics()
        self.assertIn("p99", metric["waitMs"])
        self.assertIn("p99", metric["holdMs"])


if __name__ == "__main__":
    unittest.main()
