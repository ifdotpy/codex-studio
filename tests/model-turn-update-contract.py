#!/usr/bin/env python3
"""The model update preserves captured callbacks and refuses unknown state."""
from pathlib import Path
import subprocess
import sys
import threading
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_runtime
import codex_canvas
import codex_model_turn_update as update


class ModelTurnUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = subprocess.check_output(["git", "show", "d6cc3e1:scripts/codex_runtime.py"], cwd=ROOT, text=True)

    def setUp(self):
        self.module = ModuleType("codex_runtime")
        vars(self.module).update(vars(codex_runtime))
        self.owner = type("Runtime", (), {"__module__": "codex_runtime"})
        self.module.Runtime = self.owner
        self.runtime = self.owner()
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {"retained": object()}
        self.runtime.database = object()
        for name, (old, _) in update.EXPECTED.items():
            function = update.compile_function(self.old, "Runtime", name, vars(self.module), "<model-test>")
            self.assertEqual(update.signature(function), old)
            setattr(self.owner, name, function)
        self.module_patch = patch.dict(sys.modules, {"codex_runtime": self.module})
        self.module_patch.start()

    def tearDown(self):
        self.module_patch.stop()

    def state(self):
        return ({name: vars(self.owner)[name].__code__ for name in update.EXPECTED},
                self.runtime.connections, self.runtime.database, codex_canvas.BACKEND_BUILD)

    def test_known_baseline_preserves_captured_callbacks_state_and_idempotence(self):
        callbacks = {name: getattr(self.runtime, name) for name in update.EXPECTED}
        before = self.state()
        self.assertEqual(update.apply(self.runtime), {"status": "applied", "baseCommit": "d6cc3e1",
                                                     "methods": list(update.EXPECTED)})
        for name, callback in callbacks.items():
            self.assertIs(callback.__func__, getattr(self.runtime, name).__func__)
            self.assertEqual(update.signature(callback.__func__), update.EXPECTED[name][1])
        self.assertEqual(before[1:], self.state()[1:])
        after = self.state()
        self.assertEqual(update.apply(self.runtime), {"status": "already_applied", "baseCommit": "d6cc3e1"})
        self.assertEqual(after, self.state())

    def test_unknown_method_or_defaults_refuse_before_any_callback_changes(self):
        for name in update.EXPECTED:
            original = getattr(self.owner, name)
            for kind in ("code", "defaults", "namespace"):
                function = update.compile_function(self.old, "Runtime", name,
                    dict(vars(self.module)) if kind == "namespace" else vars(self.module), "<model-test>")
                if kind == "code":
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ("unknown",))
                elif kind == "defaults":
                    function.__defaults__ = ("unknown",)
                setattr(self.owner, name, function)
                before = self.state()
                with self.subTest(name=name, kind=kind), self.assertRaisesRegex(RuntimeError, "Unknown"):
                    update.apply(self.runtime)
                self.assertEqual(before, self.state())
            setattr(self.owner, name, original)

    def test_instance_override_refuses_instead_of_false_success(self):
        for name in update.EXPECTED:
            setattr(self.runtime, name, lambda *args: None)
            before = self.state()
            with self.subTest(name=name), self.assertRaisesRegex(RuntimeError, "Instance model method override"):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())
            delattr(self.runtime, name)

    def test_wrong_source_or_replacement_refuses(self):
        before = self.state()
        with patch.object(self.module, "__file__", "/unknown/codex_runtime.py"), \
                self.assertRaisesRegex(RuntimeError, "Unexpected live runtime source"):
            update.apply(self.runtime)
        old_read = Path.read_text
        def changed(path, *args, **kwargs):
            text = old_read(path, *args, **kwargs)
            return text.replace('"model": a["model"],\n                "clientUserMessageId"',
                                '"model": "unknown-model",\n                "clientUserMessageId"')
        with patch.object(Path, "read_text", changed), self.assertRaisesRegex(RuntimeError, "Unreviewed"):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_mid_install_failure_rolls_back_all_callbacks(self):
        before = self.state()
        armed = [True]
        target = self.owner.start
        def audit(event, args):
            if armed[0] and event == "object.__setattr__" and args[0] is target and args[1] == "__code__":
                armed[0] = False
                raise RuntimeError("Controlled assignment failure")
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, "Controlled assignment"):
                update.apply(self.runtime)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())

    def test_closed_and_busy_runtime_refuse_without_mutation(self):
        before = self.state()
        self.runtime.closed = True
        with self.assertRaisesRegex(RuntimeError, "closed"):
            update.apply(self.runtime)
        self.runtime.closed = False
        runtime = self.runtime
        class Lock:
            close_on_acquire = False
            released = False
            def acquire(self, *, timeout):
                self.timeout = timeout
                if self.close_on_acquire:
                    runtime.closed = True
                return self.close_on_acquire
            def release(self):
                self.released = True
        self.runtime.lock = Lock()
        with self.assertRaisesRegex(RuntimeError, "busy"):
            update.apply(self.runtime)
        self.assertEqual(self.runtime.lock.timeout, 10)
        self.assertFalse(self.runtime.lock.released)
        self.runtime.lock.close_on_acquire = True
        with self.assertRaisesRegex(RuntimeError, "closed"):
            update.apply(self.runtime)
        self.assertTrue(self.runtime.lock.released)
        self.assertEqual(before, self.state())


if __name__ == "__main__":
    unittest.main()
