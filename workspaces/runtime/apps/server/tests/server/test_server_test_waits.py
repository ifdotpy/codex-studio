"""Unit tests for the shared server-test condition wait."""

from __future__ import annotations

import unittest
import threading
from unittest.mock import patch

from server_test_waits import HANG_GUARD_SECONDS, wait_for_condition


class WaitForConditionTests(unittest.TestCase):
    def test_returns_the_first_truthy_condition_value(self) -> None:
        values = iter((None, None, "ready"))
        with patch("server_test_waits.time.monotonic", return_value=0), patch(
            "server_test_waits.time.sleep",
        ) as sleep:
            result = wait_for_condition(lambda: next(values), "fixture readiness")

        self.assertEqual(result, "ready")
        self.assertEqual(sleep.call_count, 2)

    def test_timeout_is_a_shared_hang_guard(self) -> None:
        with (
            patch(
                "server_test_waits.time.monotonic",
                side_effect=(0, 0, 0, 0, HANG_GUARD_SECONDS, HANG_GUARD_SECONDS),
            ),
            patch("server_test_waits.time.sleep") as sleep,
            self.assertRaisesRegex(AssertionError, "fixture readiness"),
        ):
            wait_for_condition(lambda: None, "fixture readiness")

        sleep.assert_called_once_with(0.05)

    def test_event_driven_wait_rechecks_after_a_signal_without_polling(self) -> None:
        changed = threading.Event()
        calls = 0

        def condition() -> str | None:
            nonlocal calls
            calls += 1
            if calls == 1:
                changed.set()
                return None
            return "ready"

        with patch("server_test_waits.time.sleep") as sleep:
            result = wait_for_condition(
                condition, "fixture completion", wake_event=changed, event_driven=True,
            )

        self.assertEqual(result, "ready")
        self.assertEqual(calls, 2)
        sleep.assert_not_called()


if __name__ == "__main__":
    unittest.main()
