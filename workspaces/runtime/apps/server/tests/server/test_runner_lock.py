"""Keep runner admission local to a checkout and preserve resource contention."""

import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest import mock

SERVER_APP_BOOTSTRAP = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SERVER_APP_BOOTSTRAP / "src"))

from server_test_waits import HANG_GUARD_SECONDS, wait_for_condition

RUNNER_SPEC = importlib.util.spec_from_file_location(
    "lock_server_suite_runner", Path(__file__).with_name("run.py"),
)
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
RUNNER_SPEC.loader.exec_module(RUNNER)


class RunnerLockTests(unittest.TestCase):
    def test_runner_waits_then_replans_after_lock_release(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-wait-") as temp:
            lock_path = Path(temp) / "runner-live.lock"
            entered = threading.Event()
            release = threading.Event()
            acquired = threading.Event()
            output = io.StringIO()
            def first():
                with RUNNER._runner_live_lock():
                    entered.set()
                    # The test releases the lock; the waits only guard against a hang.
                    release.wait(timeout=HANG_GUARD_SECONDS)
            def second():
                with contextlib.redirect_stdout(output):
                    with RUNNER._runner_live_lock():
                        acquired.set()
                        # Planning happens in run_suites after the lock is acquired.
                        RUNNER.worker_plan([("one.py", "safe")], {}, override=1)
            with mock.patch.object(RUNNER, "RUNNER_LOCK_PATH", lock_path):
                a = threading.Thread(target=first); a.start()
                self.assertTrue(entered.wait(timeout=HANG_GUARD_SECONDS))
                b = threading.Thread(target=second); b.start()
                wait_for_condition(
                    lambda: "waiting for another test run to finish" in output.getvalue(),
                    "runner lock contention announcement",
                )
                self.assertFalse(acquired.is_set())
                release.set(); a.join(timeout=HANG_GUARD_SECONDS); b.join(timeout=HANG_GUARD_SECONDS)
            self.assertTrue(acquired.is_set())
            self.assertIn("waiting for another test run to finish", output.getvalue())

    def test_runner_lock_is_scoped_to_the_checkout(self):
        self.assertEqual(RUNNER.RUNNER_LOCK_PATH.name,
                         f"runner-live-{RUNNER.REPOSITORY_KEY}.lock")
        with tempfile.TemporaryDirectory(prefix="server-runner-independent-") as temp:
            first_path = Path(temp) / "first-checkout.lock"
            second_path = Path(temp) / "second-checkout.lock"
            with mock.patch.object(RUNNER, "RUNNER_LOCK_PATH", first_path):
                with RUNNER._runner_live_lock():
                    with mock.patch.object(RUNNER, "RUNNER_LOCK_PATH", second_path):
                        with RUNNER._runner_live_lock():
                            self.assertTrue(first_path.exists())
                            self.assertTrue(second_path.exists())

    def test_runner_lock_released_after_abnormal_exit(self):
        with tempfile.TemporaryDirectory(prefix="server-runner-abnormal-") as temp:
            lock_path = Path(temp) / "runner-live.lock"
            with mock.patch.object(RUNNER, "RUNNER_LOCK_PATH", lock_path):
                with self.assertRaisesRegex(RuntimeError, "fixture failure"):
                    with RUNNER._runner_live_lock():
                        raise RuntimeError("fixture failure")
                with RUNNER._runner_live_lock():
                    self.assertTrue(lock_path.exists())


if __name__ == "__main__":
    unittest.main()
