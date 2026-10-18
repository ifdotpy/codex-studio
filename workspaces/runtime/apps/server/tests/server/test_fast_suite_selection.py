"""Keep the fast server selection tied to the runner's existing inventory."""

import importlib.util
from pathlib import Path
import unittest

RUNNER_PATH = Path(__file__).with_name("run.py")
RUNNER_SPEC = importlib.util.spec_from_file_location("fast_server_suite_runner", RUNNER_PATH)
RUNNER = importlib.util.module_from_spec(RUNNER_SPEC)
RUNNER_SPEC.loader.exec_module(RUNNER)


class FastSuiteSelection(unittest.TestCase):
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
