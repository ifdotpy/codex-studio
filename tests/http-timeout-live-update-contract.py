#!/usr/bin/env python3
"""A guarded timeout update preserves HTTP closures and existing native objects."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib
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
import codex_runtime
import codex_http_timeout_update as update
from codex_catalog import CatalogPending
from codex_session_costs import SessionCostReader
from codex_source import signature, source_function


class TimeoutUpdateContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-timeout-update-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.runtime = codex_runtime.Runtime.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.changed = threading.Event()
        self.runtime.root = self.root
        self.runtime.closed = False
        self.runtime.accounts = None
        self.runtime.servers = {"default": object()}
        self.runtime.connection_ids = {"default": object()}
        def pending(account):
            raise CatalogPending("Metadata is pending")
        self.runtime.catalog = pending
        self.canvas = codex_canvas.Canvas(self.root)
        self.canvas.runtime = self.runtime
        self.server = codex_canvas.make_server(self.canvas)
        self.addCleanup(self.server.server_close)
        self.saved = []
        for name, spec in update.SOURCES.items():
            module = importlib.import_module(name)
            baseline = subprocess.check_output(
                ["git", "show", "c1e557f:scripts/" + spec["source"]], cwd=ROOT)
            for item in spec["functions"]:
                _, current = update.target(module, item["path"])
                old, _ = source_function(baseline, item["path"], vars(module))
                self.replace(current, old)
        current = self.server.RequestHandlerClass.do_GET
        baseline = subprocess.check_output(["git", "show", "c1e557f:scripts/codex_canvas.py"], cwd=ROOT)
        old, _ = source_function(baseline, ("make_server", "Handler", "do_GET"),
                                vars(codex_canvas), closure=current.__closure__)
        self.replace(current, old)
        self.worker_accounts = sys.modules["codex_worker_accounts"]
        self.addCleanup(self.restore)

    def replace(self, current, desired):
        self.saved.append((current, current.__code__, current.__defaults__, current.__kwdefaults__))
        current.__code__, current.__defaults__, current.__kwdefaults__ = (
            desired.__code__, desired.__defaults__, desired.__kwdefaults__)

    def restore(self):
        for function, code, defaults, keywords in reversed(self.saved):
            function.__code__, function.__defaults__, function.__kwdefaults__ = code, defaults, keywords

    def snapshot(self):
        return [(id(function), signature(function)) for function, *_ in self.saved]

    def test_stale_source_is_rejected_before_mutation(self):
        handler = self.server.RequestHandlerClass.do_GET
        closure = handler.__closure__
        native = self.runtime.servers
        before = self.snapshot()
        with patch.object(codex_runtime.subprocess, "Popen", side_effect=AssertionError("An update must not start a native child")):
            with self.assertRaisesRegex(RuntimeError, "reviewed backend source differs"):
                update.apply(self.runtime)
            with self.assertRaisesRegex(RuntimeError, "reviewed backend source differs"):
                update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)
        self.assertIs(handler, self.server.RequestHandlerClass.do_GET)
        self.assertIs(closure, handler.__closure__)
        self.assertIs(native, self.runtime.servers)
        self.assertNotIn("DISPLAY_READ", vars(self.worker_accounts))

    def test_reviewed_fixture_reaches_http_validation_after_other_sources(self):
        with tempfile.TemporaryDirectory(prefix="studio-reviewed-timeout-fixture-") as directory:
            scripts = Path(directory)
            runtime_path = scripts / "codex_runtime.py"
            canvas_path = scripts / "codex_canvas.py"
            probe_path = scripts / "codex_probe.py"
            patch_path = scripts / "codex_http_timeout_update.py"
            runtime_path.write_text("# fixture runtime\n", encoding="utf-8")
            patch_path.write_text("# patch module path\n", encoding="utf-8")

            canvas_source = """\
def make_server(canvas):
    class Handler:
        def do_GET(self):
            return canvas.value
    return Handler
"""
            old_canvas_source = """\
def make_server(canvas):
    class Handler:
        def do_GET(self):
            return canvas.previous
    return Handler
"""
            probe_source = """\
class Probe:
    def read(self):
        return "reviewed"
"""
            old_probe_source = """\
class Probe:
    def read(self):
        return "old"
"""
            canvas_path.write_text(canvas_source, encoding="utf-8")
            probe_path.write_text(probe_source, encoding="utf-8")

            runtime_module = ModuleType("codex_runtime")
            runtime_module.__file__ = str(runtime_path)
            runtime_module.Runtime = type("Runtime", (), {})
            runtime = runtime_module.Runtime()
            runtime.lock = threading.RLock()
            runtime.changed = threading.Event()
            runtime.root = scripts
            runtime.closed = False

            canvas_module = ModuleType("codex_canvas")
            canvas_module.__file__ = str(canvas_path)
            exec(compile(canvas_source, str(canvas_path), "exec"), vars(canvas_module))
            canvas = SimpleNamespace(runtime=runtime, previous="old", value="reviewed")
            handler = canvas_module.make_server(canvas)
            current_http = handler.do_GET
            old_http, _ = source_function(
                old_canvas_source.encode(), ("make_server", "Handler", "do_GET"),
                vars(canvas_module), closure=current_http.__closure__,
            )
            current_http.__code__ = old_http.__code__

            probe_module = ModuleType("codex_probe")
            probe_module.__file__ = str(probe_path)
            exec(compile(probe_source, str(probe_path), "exec"), vars(probe_module))
            old_probe, _ = source_function(
                old_probe_source.encode(), ("Probe", "read"), vars(probe_module),
            )
            current_probe = probe_module.Probe.read
            current_probe.__code__ = old_probe.__code__
            desired_probe, _ = source_function(
                probe_path.read_bytes(), ("Probe", "read"), vars(probe_module),
            )
            desired_http, _ = source_function(
                canvas_path.read_bytes(), ("make_server", "Handler", "do_GET"),
                vars(canvas_module), closure=current_http.__closure__,
            )

            sources = {
                "codex_probe": {
                    "source": probe_path.name,
                    "sha256": hashlib.sha256(probe_path.read_bytes()).hexdigest(),
                    "functions": [{
                        "path": ("Probe", "read"),
                        "before": signature(old_probe),
                        "after": signature(desired_probe),
                        "static": False,
                    }],
                },
            }
            handler_spec = {
                "sha256": hashlib.sha256(canvas_path.read_bytes()).hexdigest(),
                "before": signature(old_http),
                "after": signature(desired_http),
            }
            with (
                patch.dict(sys.modules, {
                    "codex_runtime": runtime_module,
                    "codex_canvas": canvas_module,
                    "codex_probe": probe_module,
                }),
                patch.object(update, "__file__", str(patch_path)),
                patch.object(update, "SOURCES", sources),
                patch.object(update, "HANDLER", handler_spec),
            ):
                result = update.apply(runtime)

            self.assertEqual(result, {"status": "applied"})
            self.assertEqual(probe_module.Probe().read(), "reviewed")
            self.assertEqual(handler().do_GET(), "reviewed")
            self.assertTrue(runtime.changed.is_set())

    def test_fastapi_without_legacy_handler_rejects_patch_before_source_changes(self):
        before = self.snapshot()
        with patch.object(update.gc, "get_objects", return_value=[]):
            with self.assertRaisesRegex(RuntimeError, "no compatible legacy handler"):
                update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)
        self.assertNotIn("DISPLAY_READ", vars(self.worker_accounts))

    def test_unknown_http_code_rejects_before_other_mutations(self):
        current = self.server.RequestHandlerClass.do_GET
        current.__code__ = current.__code__.replace(co_exceptiontable=b"")
        before = self.snapshot()
        with self.assertRaisesRegex(RuntimeError, "running HTTP function differs"):
            update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)
        self.assertNotIn("DISPLAY_READ", vars(self.worker_accounts))

    def test_unknown_backend_code_rejects_before_http_mutation(self):
        current = SessionCostReader.snapshot
        current.__code__ = current.__code__.replace(co_consts=current.__code__.co_consts + ("unknown",))
        before = self.snapshot()
        with self.assertRaisesRegex(RuntimeError, "running function differs"):
            update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)

    def test_unreviewed_source_hash_rejects_before_any_mutation(self):
        before = self.snapshot()
        with patch.dict(update.SOURCES["codex_session_costs"], sha256="0" * 64):
            with self.assertRaisesRegex(RuntimeError, "reviewed backend source differs"):
                update.apply(self.runtime)
        self.assertEqual(self.snapshot(), before)


if __name__ == "__main__":
    unittest.main()
