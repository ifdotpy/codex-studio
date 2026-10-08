#!/usr/bin/env python3
"""Periodic scheduler scans yield the runtime lock between real wakeups."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from pathlib import Path
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_runtime import Runtime


class SchedulerCadence(unittest.TestCase):
    def test_idle_ticks_do_not_rescan_and_changed_wakes_dispatch(self):
        runtime = Runtime.__new__(Runtime)
        runtime.closed = False
        clock = [0.0]
        idle_tick = threading.Event()
        release_changed = threading.Event()
        dispatched_twice = threading.Event()

        class Changed:
            calls = 0
            pending = False

            def wait(self, _timeout):
                self.calls += 1
                if self.pending:
                    self.pending = False
                    return True
                if self.calls == 2:
                    clock[0] += 2.1
                    return False
                if self.calls == 3:
                    idle_tick.set()
                    release_changed.wait(2)
                    return True
                runtime.closed = True
                return False

            def clear(self):
                self.pending = False

            def set(self):
                self.pending = True

        runtime.changed = Changed()
        ticks = []
        runtime.monitors_tick = lambda: ticks.append(clock[0])
        runtime.rules_tick = lambda: None
        runtime.capacity_tick = lambda: None
        runtime.usage_resume_tick = lambda: None
        runtime.accepted_archive_tick = lambda: None
        runtime.retry_monitor_results = lambda: None
        runtime.runtime_maintenance_tick = lambda: None
        runtime.turn_item_links_tick = lambda: None
        runtime._retry_dirty_workspace_refresh = lambda: None
        runtime._publish_committed_resource_changes = lambda: None
        dispatched = []
        runtime.dispatch = lambda **_options: (
            dispatched.append(clock[0]),
            dispatched_twice.set() if len(dispatched) == 2 else None,
        )
        runtime.changed.set()

        with tempfile.TemporaryDirectory() as directory, \
                patch('codex_runtime.startup_memory_mark'), \
                patch('codex_runtime.time.monotonic', side_effect=lambda: clock[0]):
            runtime.root = Path(directory)
            worker = threading.Thread(target=runtime.schedule)
            worker.start()
            try:
                self.assertTrue(idle_tick.wait(2))
                self.assertEqual(len(dispatched), 1)
                self.assertEqual(len(ticks), 2)
                self.assertEqual(len(dispatched), 1)
                woke_at = clock[0]
                release_changed.set()
                self.assertTrue(dispatched_twice.wait(2))
                self.assertEqual(len(dispatched), 2)
                self.assertEqual(dispatched[1], woke_at)
            finally:
                runtime.closed = True
                release_changed.set()
                runtime.changed.set()
                worker.join(2)
                self.assertFalse(worker.is_alive())


if __name__ == '__main__':
    unittest.main()
