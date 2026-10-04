#!/usr/bin/env python3
"""A guarded timeout update preserves legacy handler closures safely."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import hashlib
from pathlib import Path
import sys
import tempfile
import threading
from contextlib import contextmanager
from types import ModuleType, SimpleNamespace
from typing import Iterator
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_runtime
import codex_http_timeout_update as update
from codex_source import signature, source_function


@contextmanager
def reviewed_fixture() -> Iterator[SimpleNamespace]:
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
        modules = {
            "codex_runtime": runtime_module,
            "codex_canvas": canvas_module,
            "codex_probe": probe_module,
        }
        with (
            patch.dict(sys.modules, modules),
            patch.object(update, "__file__", str(patch_path)),
            patch.object(update, "SOURCES", sources),
            patch.object(update, "HANDLER", handler_spec),
        ):
            yield SimpleNamespace(
                runtime=runtime,
                canvas=canvas,
                handler=handler,
                probe=probe_module,
                probe_path=probe_path,
                probe_source=probe_source,
                sources=sources,
            )


class TimeoutUpdateContract(unittest.TestCase):
    def test_fastapi_without_legacy_handler_rejects_patch(self):
        with tempfile.TemporaryDirectory(prefix="studio-fastapi-runtime-") as directory:
            runtime = codex_runtime.Runtime.__new__(codex_runtime.Runtime)
            runtime.root = Path(directory)
            runtime.closed = False
            self.assertEqual(update.legacy_handlers(runtime), [])
            with self.assertRaisesRegex(RuntimeError, "no compatible legacy handler"):
                update.apply(runtime)

    def test_stale_source_is_rejected_before_mutation(self):
        with reviewed_fixture() as fixture:
            fixture.probe_path.write_text("# changed source\n" + fixture.probe_source, encoding="utf-8")
            with self.assertRaisesRegex(RuntimeError, "reviewed backend source differs: codex_probe"):
                update.apply(fixture.runtime)
            self.assertEqual(fixture.probe.Probe().read(), "old")
            self.assertEqual(fixture.handler().do_GET(), "old")

    def test_reviewed_fixture_reaches_http_validation_after_other_sources(self):
        with reviewed_fixture() as fixture:
            result = update.apply(fixture.runtime)
            self.assertEqual(result, {"status": "applied"})
            self.assertEqual(fixture.probe.Probe().read(), "reviewed")
            self.assertEqual(fixture.handler().do_GET(), "reviewed")
            self.assertTrue(fixture.runtime.changed.is_set())

    def test_unknown_http_code_rejects_before_backend_mutation(self):
        with reviewed_fixture() as fixture:
            current = fixture.handler.do_GET
            current.__code__ = current.__code__.replace(co_consts=current.__code__.co_consts + ("unknown",))
            with self.assertRaisesRegex(RuntimeError, "running HTTP function differs"):
                update.apply(fixture.runtime)
            self.assertEqual(fixture.probe.Probe().read(), "old")

    def test_unknown_backend_code_rejects_before_http_mutation(self):
        with reviewed_fixture() as fixture:
            current = fixture.probe.Probe.read
            current.__code__ = current.__code__.replace(co_consts=current.__code__.co_consts + ("unknown",))
            with self.assertRaisesRegex(RuntimeError, "running function differs"):
                update.apply(fixture.runtime)
            self.assertEqual(fixture.handler().do_GET(), "old")

    def test_unreviewed_source_hash_rejects_before_any_mutation(self):
        with reviewed_fixture() as fixture:
            with patch.dict(fixture.sources["codex_probe"], sha256="0" * 64):
                with self.assertRaisesRegex(RuntimeError, "reviewed backend source differs: codex_probe"):
                    update.apply(fixture.runtime)
            self.assertEqual(fixture.probe.Probe().read(), "old")
            self.assertEqual(fixture.handler().do_GET(), "old")


if __name__ == "__main__":
    unittest.main()
