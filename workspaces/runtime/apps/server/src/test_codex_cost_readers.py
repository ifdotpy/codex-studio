#!/usr/bin/env python3
"""Refresh completion state exposed by cost readers and their callbacks."""

import tempfile
import threading
import unittest
from pathlib import Path

from codex_costs import ClaudeCostReader


class _Pricing:
    def __init__(self) -> None:
        self.started = threading.Event()
        self.release = threading.Event()

    def wait_ready(self) -> dict[str, object]:
        self.started.set()
        if not self.release.wait(2):
            raise TimeoutError("test did not release Claude cost refresh")
        return {}


class CostReaderCompletionTests(unittest.TestCase):
    def test_claude_completion_notifies_after_refreshing_clears_even_for_empty_scan(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            config_dir = Path(directory) / "claude"
            config_dir.mkdir()
            pricing = _Pricing()
            observations: list[dict[str, object]] = []
            completed = threading.Event()
            reader: ClaudeCostReader

            def on_change() -> None:
                observations.append(reader.snapshot())
                completed.set()

            reader = ClaudeCostReader(
                Path(directory), config_dir, pricing, clock=lambda: 1_000.0,
                on_change=on_change,
            )
            initial = reader.snapshot()
            self.assertTrue(initial["refreshing"])
            self.assertTrue(pricing.started.wait(1))
            self.assertTrue(reader.snapshot()["refreshing"])

            pricing.release.set()
            self.assertTrue(completed.wait(1))

            self.assertEqual(len(observations), 1)
            settled = observations[0]
            self.assertFalse(settled["refreshing"])
            self.assertIsInstance(settled["data"], dict)
            self.assertEqual(settled["data"]["coverage"], "unverified")


if __name__ == "__main__":
    unittest.main()
