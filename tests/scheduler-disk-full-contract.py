#!/usr/bin/env python3
"""A full diagnostic disk must not kill the scheduler or duplicate dispatch."""
import ast
import errno
from pathlib import Path
import sqlite3
import time
import types
import unittest

source = Path(__file__).resolve().parents[1] / "scripts/codex_runtime.py"
module = ast.parse(source.read_text())
cls = next(n for n in module.body if isinstance(n, ast.ClassDef) and n.name == "Runtime")
method = next(n for n in cls.body if isinstance(n, ast.FunctionDef) and n.name == "schedule")
scope = {"time": time}
exec(compile(ast.Module(body=[method], type_ignores=[]), str(source), "exec"), scope)

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
            def clear(self): pass
        rt = types.SimpleNamespace(root=Root(), changed=Wake(), closed=False)
        def rules():
            seen.append("rules")
            if seen.count("rules") == 1: raise sqlite3.OperationalError("disk I/O error")
        def dispatch():
            seen.append("dispatch"); rt.closed = True
        rt.rules_tick = rules
        rt.capacity_tick = lambda: seen.append("capacity")
        rt.dispatch = dispatch
        scope["schedule"](rt)
        expected = ["wait", "rules", "wait", "rules"]
        if "capacity_tick" in scope["schedule"].__code__.co_names:
            expected.append("capacity")
        self.assertEqual(seen, expected + ["dispatch"])
        self.assertIsNone(rt.scheduler_error)
    def test_log_open_failure_recovers(self): self.exercise("open")
    def test_log_write_failure_recovers(self): self.exercise("write")
    def test_normal_error_log_recovers(self): self.exercise(None)

if __name__ == "__main__": unittest.main()
