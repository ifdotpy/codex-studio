"""Small deterministic checks for the runtime load report helpers."""
import importlib.util
from pathlib import Path
import unittest

path = Path(__file__).with_name("server.py")
spec = importlib.util.spec_from_file_location("runtime_load_server", path)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class RuntimeLoadHelpersTests(unittest.TestCase):
    def test_nearest_rank_latency_statistics(self):
        self.assertEqual(module.stats([4, 1, 3, 2]),
                         {"samples": 4, "p50": 2, "p95": 4, "p99": 4, "max": 4})

    def test_empty_latency_series_is_explicit(self):
        self.assertEqual(module.stats([]),
                         {"samples": 0, "p50": None, "p95": None, "p99": None, "max": None})


if __name__ == "__main__":
    unittest.main()
