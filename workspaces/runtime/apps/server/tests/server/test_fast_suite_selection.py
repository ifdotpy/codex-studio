"""Keep the fast server selection tied to the runner's existing inventory."""

import importlib.util
from pathlib import Path
import unittest
import os
import tempfile
import subprocess
import sys
from unittest.mock import patch

RUNNER_PATH = Path(__file__).with_name("run.py")
RUNNER_SPEC = importlib.util.spec_from_file_location("fast_server_suite_runner", RUNNER_PATH)
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
RUNNER_SPEC.loader.exec_module(RUNNER)


class FastSuiteSelection(unittest.TestCase):
    def test_runner_resolves_checkout_native_module_before_shared_environment(self):
        from codex_operations_artifact import operations_extension
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "target"
            debug = target / "debug"
            debug.mkdir(parents=True)
            # A fake library verifies import search order without loading native code.
            with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(target)}):
                with self.assertRaisesRegex(RuntimeError, "checkout requires"):
                    operations_extension(RUNNER.ROOT, profile="debug")
                library = debug / ("studio_operations_native.dll" if sys.platform == "win32" else
                                   "libstudio_operations_native.dylib" if sys.platform == "darwin" else
                                   "libstudio_operations_native.so")
                library.touch()
                scratch = root / "suite"
                scratch.mkdir()
                environment = RUNNER._suite_environment(RUNNER.ROOT, scratch)
                found = subprocess.run(
                    [sys.executable, "-c", "import importlib.util; print(importlib.util.find_spec('studio_operations_native').origin)"],
                    env=environment, capture_output=True, text=True, check=True, timeout=5,
                )
                self.assertEqual(Path(found.stdout.strip()).resolve(), library.resolve())

    def test_fast_selection_is_discovered_default_and_excludes_manual_work(self):
        selected = RUNNER.only_category([], "fast")
        paths = {path for path, _kind in selected}

        self.assertTrue(paths)
        self.assertEqual(len(paths), len(RUNNER.FAST_SUITES))
        self.assertEqual(paths, set(RUNNER.FAST_SUITES))
        self.assertIn("workspaces/runtime/apps/server/tests/server/test_fast_suite_selection.py", paths)
        self.assertTrue(
            all(kind in {"safe", "component", "automated"} for _path, kind in selected)
        )
        self.assertEqual(
            {path for path, kind in selected if kind == "automated"},
            set(RUNNER.FAST_SUITES).intersection(RUNNER.AUTOMATED_SUITES),
        )
        self.assertFalse(paths.intersection(RUNNER.HEAVY_SUITES))
        self.assertFalse(paths.intersection(RUNNER.EXPENSIVE_SUITES))
        benchmark_roots = (*RUNNER.SCENARIO_BENCHMARK_ROOTS,
                           f"{RUNNER.SOURCE_REL}/studio_api/benchmarks/runtime_load")
        self.assertFalse(any(path.startswith(benchmark_roots) for path in paths))


if __name__ == "__main__":
    unittest.main()
