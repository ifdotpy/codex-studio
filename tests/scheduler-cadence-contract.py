#!/usr/bin/env python3
"""Periodic scheduler scans yield the runtime lock between real wakeups."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime


class SchedulerCadence(unittest.TestCase):
    def test_idle_ticks_do_not_rescan_and_changed_wakes_dispatch(self):
        runtime = Runtime.__new__(Runtime)
        runtime.closed = False
        runtime.changed = threading.Event()
        runtime.monitors_tick = lambda: None
        runtime.rules_tick = lambda: None
        runtime.capacity_tick = lambda: None
        runtime.usage_resume_tick = lambda: None
        runtime.accepted_archive_tick = lambda: None
        runtime.retry_monitor_results = lambda: None
        runtime.runtime_maintenance_tick = lambda: None
        runtime._retry_dirty_workspace_refresh = lambda: None
        runtime._publish_committed_resource_changes = lambda: None
        dispatched = []
        runtime.dispatch = lambda **_options: dispatched.append(time.monotonic())
        runtime.changed.set()
        with tempfile.TemporaryDirectory() as directory, patch('codex_runtime.startup_memory_mark'):
            runtime.root = Path(directory)
            worker = threading.Thread(target=runtime.schedule)
            worker.start()
            try:
                deadline = time.monotonic() + 1
                while not dispatched and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertEqual(len(dispatched), 1)
                time.sleep(2.1)
                self.assertEqual(len(dispatched), 1)
                woke_at = time.monotonic()
                runtime.changed.set()
                deadline = woke_at + 1
                while len(dispatched) < 2 and time.monotonic() < deadline:
                    time.sleep(.01)
                self.assertEqual(len(dispatched), 2)
                self.assertLess(dispatched[1] - woke_at, .5)
            finally:
                runtime.closed = True
                runtime.changed.set()
                worker.join(2)
                self.assertFalse(worker.is_alive())


if __name__ == '__main__':
    unittest.main()
