#!/usr/bin/env python3
"""The fit cutover retains callbacks, closure cells, and live runtime state."""
import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_canvas
import codex_progress
import codex_runtime
import codex_progress_fit_update as update


def cell(value=None):
    return (lambda: value).__closure__[0]


class ProgressFitUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(
            ["git", "show", "193d962:scripts/" + name + ".py"], cwd=ROOT, text=True)
            for name in ("codex_canvas", "codex_progress")}

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="studio-progress-fit-update-")
        self.modules = {}
        for original in (codex_canvas, codex_progress, codex_runtime):
            module = ModuleType(original.__name__)
            module.__dict__.update(vars(original))
            self.modules[original.__name__] = module
        self.owner = type("Runtime", (), {"__module__": "codex_runtime"})
        self.modules["codex_runtime"].Runtime = self.owner
        self.runtime = self.owner()
        self.runtime.root = Path(self.temporary.name)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {"retained": object()}
        self.runtime.database = object()
        self.handler = type("Handler", (), {"__module__": "codex_canvas",
                            "__qualname__": "make_server.<locals>.Handler"})
        self.cells = {key: cell(value) for key, value in {
            "canvas": SimpleNamespace(runtime=self.runtime), "remote": object(), "token": "token",
            "snapshot": None, "sync": None, "terminals": None, "cost_reader": [None],
            "terminal_lock": threading.RLock(), "sync_store": [None], "terminal_manager": [None],
        }.items()}
        for name in ("snapshot", "sync", "terminals"):
            self.cells[name].cell_contents = self.compile("codex_canvas", ("make_server", name))
        for name in ("do_GET", "do_POST", "trusted", "stream_sync"):
            setattr(self.handler, name, self.compile("codex_canvas", ("make_server", "Handler", name)))
        self.modules["codex_progress"].progress_context = self.compile("codex_progress", ("progress_context",))
        self.assertEqual(update.signature(self.handler.do_POST), update._EXPECTED["route_old"])
        self.assertEqual(update.signature(self.modules["codex_progress"].progress_context), update._EXPECTED["context_old"])
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        sys.modules.pop("codex_progress_layout", None)
        # The production digest is pinned by its owner after review. This fixture
        # pins the exact current source, then exercises refusal after a change.
        self.digest_patch = patch.object(update, "_LAYOUT_SHA256", hashlib.sha256(
            (ROOT / "scripts/codex_progress_layout.py").read_bytes()).hexdigest())
        self.digest_patch.start()

    def tearDown(self):
        self.digest_patch.stop()
        self.module_patch.stop()
        self.temporary.cleanup()

    def compile(self, module, path, source=None, namespace=None, cells=None):
        source = source or self.old[module]
        namespace = vars(self.modules[module]) if namespace is None else namespace
        function, _ = update.source_function(source, path, namespace)
        known = self.cells if cells is None else cells
        closure = tuple(known[name] for name in function.__code__.co_freevars) or None
        return update.source_function(source, path, namespace, closure=closure)[0]

    def state(self):
        return (self.handler.do_POST.__code__, self.modules["codex_progress"].progress_context.__code__,
                self.handler.do_POST.__closure__, self.runtime.connections, self.runtime.database,
                self.modules["codex_canvas"].BACKEND_BUILD)

    def test_real_baseline_preserves_callbacks_cells_and_state_then_is_idempotent(self):
        callback = self.handler.do_POST
        context = self.modules["codex_progress"].progress_context
        cells = callback.__closure__
        contents = tuple(item.cell_contents for item in cells)
        before = self.state()
        self.assertEqual(update.apply(self.runtime, self.handler), {
            "status": "applied", "baseCommit": "193d962", "methods": ["progress_context", "do_POST"]})
        self.assertIs(callback, self.handler.do_POST)
        self.assertIs(context, self.modules["codex_progress"].progress_context)
        self.assertIs(callback.__closure__, cells)
        self.assertTrue(all(old is current.cell_contents for old, current in zip(contents, cells)))
        self.assertEqual(update.signature(callback), update._EXPECTED["route_new"])
        self.assertEqual(update.signature(context), update._EXPECTED["context_new"])
        self.assertEqual(before[2:], self.state()[2:])
        self.assertEqual(list(self.runtime.root.iterdir()), [])
        text = context(self.runtime.root, "first")
        self.assertIn("--wait 3", text)
        self.assertIn("no scroll", text)
        self.assertIn("2 means no current measurement", text)
        after = self.state()
        self.assertEqual(update.apply(self.runtime, self.handler),
                         {"status": "already_applied", "baseCommit": "193d962"})
        self.assertEqual(after, self.state())

    def test_unknown_code_defaults_or_globals_refuse_without_partial_update(self):
        for name in ("route", "context"):
            owner, key = ((self.handler, "do_POST") if name == "route"
                          else (self.modules["codex_progress"], "progress_context"))
            for kind in ("code", "defaults", "globals"):
                original = getattr(owner, key)
                module = "codex_canvas" if name == "route" else "codex_progress"
                path = ("make_server", "Handler", "do_POST") if name == "route" else ("progress_context",)
                changed = self.compile(module, path, namespace=dict(vars(self.modules[module])) if kind == "globals" else None)
                if kind == "code":
                    changed.__code__ = changed.__code__.replace(co_consts=changed.__code__.co_consts + ("unknown",))
                elif kind == "defaults":
                    changed.__defaults__ = ("unknown",)
                setattr(owner, key, changed)
                before = self.state()
                with self.subTest(name=name, kind=kind), self.assertRaisesRegex(RuntimeError, "Unknown"):
                    update.apply(self.runtime, self.handler)
                self.assertEqual(self.state(), before)
                setattr(owner, key, original)

    def test_unknown_disk_source_hash_decorator_and_live_origin_refuse(self):
        before = self.state()
        with patch.object(update, "_LAYOUT_SHA256", "0" * 64), self.assertRaisesRegex(RuntimeError, "Unreviewed"):
            update.apply(self.runtime, self.handler)
        original_read = Path.read_text
        def changed(path, *args, **kwargs):
            value = original_read(path, *args, **kwargs)
            return (value.replace("def progress_context(", "@staticmethod\ndef progress_context(")
                    if path.name == "codex_progress.py" else value)
        with patch.object(Path, "read_text", changed), self.assertRaisesRegex(RuntimeError, "Unreviewed"):
            update.apply(self.runtime, self.handler)
        with patch.object(self.modules["codex_progress"], "__file__", "/unknown/codex_progress.py"), \
                self.assertRaisesRegex(RuntimeError, "Unknown live progress source"):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_loaded_helper_refused_before_any_mutation(self):
        before = self.state()
        with patch.dict(sys.modules, {"codex_progress_layout": ModuleType("codex_progress_layout")}), \
                self.assertRaisesRegex(RuntimeError, "already loaded"):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_separate_http_closure_cells_and_other_runtime_refuse(self):
        original = self.handler.do_POST
        self.handler.do_POST = self.compile("codex_canvas", ("make_server", "Handler", "do_POST"),
                                           cells={key: cell(value.cell_contents) for key, value in self.cells.items()})
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, "closure cells"):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())
        self.handler.do_POST = original
        self.cells["canvas"].cell_contents = SimpleNamespace(runtime=object())
        with self.assertRaisesRegex(RuntimeError, "another runtime"):
            update.apply(self.runtime, self.handler)

    def test_second_code_assignment_failure_rolls_back_both_callbacks(self):
        before = self.state()
        armed = [True]
        target = self.handler.do_POST
        def audit(event, args):
            if armed[0] and event == "object.__setattr__" and args[0] is target and args[1] == "__code__":
                armed[0] = False
                raise RuntimeError("Controlled second code assignment failure")
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, "Controlled second"):
                update.apply(self.runtime, self.handler)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())

    def test_busy_and_closed_after_wait_refuse_and_release_only_owned_lock(self):
        runtime = self.runtime
        class Lock:
            close_on_acquire = False
            released = False
            def acquire(self, *, timeout):
                self.timeout = timeout
                if self.close_on_acquire:
                    runtime.closed = True
                    return True
                return False
            def release(self):
                self.released = True
        runtime.lock = Lock()
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, "remains busy"):
            update.apply(runtime, self.handler)
        self.assertEqual(runtime.lock.timeout, 10)
        self.assertFalse(runtime.lock.released)
        runtime.lock.close_on_acquire = True
        with self.assertRaisesRegex(RuntimeError, "closed"):
            update.apply(runtime, self.handler)
        self.assertTrue(runtime.lock.released)
        self.assertEqual(before, self.state())


if __name__ == "__main__":
    unittest.main()
