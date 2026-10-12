"""Unit tests for the shared server-test condition wait."""

from __future__ import annotations

import unittest
from unittest.mock import patch

from test_waits import HANG_GUARD_SECONDS, wait_for_condition


class WaitForConditionTests(unittest.TestCase):
    def test_returns_the_first_truthy_condition_value(self) -> None:
        values = iter((None, None, "ready"))
        with patch("test_waits.time.monotonic", return_value=0), patch(
            "test_waits.time.sleep",
        ) as sleep:
            result = wait_for_condition(lambda: next(values), "fixture readiness")

        self.assertEqual(result, "ready")
        self.assertEqual(sleep.call_count, 2)

    def test_timeout_is_a_shared_hang_guard(self) -> None:
        with (
            patch("test_waits.time.monotonic", side_effect=(0, 0, HANG_GUARD_SECONDS)),
            patch("test_waits.time.sleep") as sleep,
            self.assertRaisesRegex(TimeoutError, "fixture readiness"),
        ):
            wait_for_condition(lambda: None, "fixture readiness")

        sleep.assert_called_once_with(0.05)


if __name__ == "__main__":
    unittest.main()
