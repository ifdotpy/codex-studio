#!/usr/bin/env python3
"""A full diagnostic disk must not kill the scheduler or duplicate dispatch."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import errno
from pathlib import Path
import sqlite3
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_runtime import Runtime

class SchedulerDiskContract(unittest.TestCase):
    def exercise(self, fail_on):
        seen = []
        class Root:
            def __truediv__(self, _): return self
            def open(self, _):
                if fail_on == "open": raise OSError(errno.ENOSPC, "No space left on device")
                return self
            def __enter__(self): return self
            def __exit__(self, *_): return False
            def write(self, _):
                if fail_on == "write": raise OSError(errno.ENOSPC, "No space left on device")
        class Wake:
            def wait(self, _):
                seen.append("wait")
                if seen.count("wait") > 3: raise AssertionError("Scheduler did not recover")
                if seen.count("wait") == 3: rt.closed = True
                return True
            def clear(self): pass
        rt = Runtime.__new__(Runtime)
        rt.root, rt.changed, rt.closed = Root(), Wake(), False
        rt._retry_dirty_workspace_refresh = lambda: None
        rt._publish_committed_resource_changes = lambda: None
        rt.monitors_tick = lambda: None
        def rules():
            seen.append("rules")
            if seen.count("rules") == 1: raise sqlite3.OperationalError("disk I/O error")
        def dispatch(**_options):
            seen.append("dispatch")
        rt.rules_tick = rules
        rt.capacity_tick = lambda: seen.append("capacity")
        rt.usage_resume_tick = lambda: seen.append("usage")
        rt.accepted_archive_tick = lambda: None
        rt.retry_monitor_results = lambda: None
        rt.runtime_maintenance_tick = lambda: None
        rt.turn_item_links_tick = lambda: None
        rt.dispatch = dispatch
        clock = SimpleNamespace(monotonic=lambda: seen.count("wait") * 1.1,
                                time=lambda: seen.count("wait") * 1.1)
        with patch("codex_runtime.time", clock), patch("codex_runtime.startup_memory_mark"):
            rt.schedule()
        self.assertEqual(seen, ["wait", "dispatch", "rules", "capacity", "usage",
                                "wait", "dispatch", "rules", "capacity", "usage", "wait"])
        self.assertIsNone(rt.scheduler_error)
    def test_log_open_failure_recovers(self): self.exercise("open")
    def test_log_write_failure_recovers(self): self.exercise("write")
    def test_normal_error_log_recovers(self): self.exercise(None)

if __name__ == "__main__": unittest.main()
