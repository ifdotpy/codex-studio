#!/usr/bin/env python3
"""A guarded source update preserves existing terminal and monitor owners."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_runtime
import codex_monitor_terminal_update as update
from codex_source import signature, source_function

spec = importlib.util.spec_from_file_location(
    "spawn_fixture", ROOT / "tests/terminal-spawn-lock-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class LiveUpdateContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-input-live-update-")
        self.addCleanup(self.temp.cleanup)
        self.runtime = codex_runtime.Runtime.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.changed = threading.Event()
        self.runtime.servers = {"default": object()}
        self.runtime.connection_ids = {"default": object()}
        self.saved = []
        for name, spec in update.SOURCES.items():
            module = importlib.import_module(name)
            baseline = subprocess.check_output(
                ["git", "show", "2e01c1f:scripts/" + spec["source"]], cwd=ROOT)
            for item in spec["functions"]:
                owner, current = update.target(module, item["path"])
                self.saved.append((owner, item["path"][-1], current,
                                   current.__code__, current.__defaults__, current.__kwdefaults__))
                if item["before"] is None:
                    delattr(owner, item["path"][-1])
                else:
                    old, _ = source_function(baseline, item["path"], vars(module))
                    current.__code__, current.__defaults__, current.__kwdefaults__ = (
                        old.__code__, old.__defaults__, old.__kwdefaults__)
        self.addCleanup(self.restore)

    def restore(self):
        for owner, name, function, code, defaults, keywords in self.saved:
            setattr(owner, name, function)
            function.__code__, function.__defaults__, function.__kwdefaults__ = code, defaults, keywords

    def snapshot(self):
        result = []
        for name, spec in update.SOURCES.items():
            for item in spec["functions"]:
                _, current = update.target(sys.modules[name], item["path"])
                result.append((name, item["path"], id(current),
                               signature(current) if current is not None else None))
        return result

    def test_apply_preserves_existing_native_objects_and_lazy_manager_guard(self):
        manager = fixture.TerminalManager(Path(self.temp.name))
        native = fixture.Native()
        native.timeout = True
        manager.server = native
        manager.connection = object()
        identity = (manager.server, manager.connection, manager.processes,
                    self.runtime.servers, self.runtime.connection_ids)
        self.assertNotIn("_start_lock", vars(manager))
        with patch.object(codex_runtime.subprocess, "Popen",
                          side_effect=AssertionError("An update must not start a native child")):
            self.assertEqual(update.apply(self.runtime)["status"], "applied")
            self.assertEqual(update.apply(self.runtime)["status"], "applied")
        current = (manager.server, manager.connection, manager.processes,
                   self.runtime.servers, self.runtime.connection_ids)
        self.assertTrue(all(before is after for before, after in zip(identity, current)))
        body = {"id": "once", "agent": "lead"}
        runtime = fixture.Runtime(Path(self.temp.name))
        record = manager.create(runtime, body)
        self.assertIn("unknown", record["error"])
        self.assertIsNotNone(manager._start_lock)
        native.future.set_result({})
        self.assertNotIn("error", manager.create(runtime, body))
        self.assertEqual(len(native.submitted), 1)
        self.assertIs(manager.server, native)

    def test_unknown_function_rejects_before_any_code_or_helper_changes(self):
        module = sys.modules["codex_rules"]
        current = module.RulesMixin.monitor_input
        def unknown(self, *args, **kwargs):
            raise RuntimeError("Unexpected local implementation")
        current.__code__, current.__defaults__, current.__kwdefaults__ = (
            unknown.__code__, unknown.__defaults__, unknown.__kwdefaults__)
        before = self.snapshot()
        with self.assertRaisesRegex(RuntimeError, "running function differs"):
            update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)

    def test_unreviewed_source_hash_rejects_before_any_code_changes(self):
        before = self.snapshot()
        with patch.dict(update.SOURCES["codex_rules"], sha256="0" * 64):
            with self.assertRaisesRegex(RuntimeError, "reviewed backend source differs"):
                update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()
