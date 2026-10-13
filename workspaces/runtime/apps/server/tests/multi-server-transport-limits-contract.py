#!/usr/bin/env python3
"""Cross-language parity for signed multi-server transport byte limits."""

import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codex_multi_server_transport_limits import (
    MULTI_SERVER_MAX_REQUEST_BYTES,
    MULTI_SERVER_MAX_RESPONSE_BYTES,
)
from studio_api.generate_multi_server_transport_limits import OUTPUTS


class MultiServerTransportLimitsContract(unittest.TestCase):
    def test_existing_transport_boundaries_remain_unchanged(self):
        self.assertEqual(MULTI_SERVER_MAX_REQUEST_BYTES, 256 * 1024)
        self.assertEqual(MULTI_SERVER_MAX_RESPONSE_BYTES, 1024 * 1024)

    def test_committed_rust_and_node_constants_match_the_python_owner(self):
        for path, expected in OUTPUTS.items():
            with self.subTest(path=path.name):
                self.assertEqual(path.read_text(encoding="utf-8"), expected)


if __name__ == "__main__":
    unittest.main()
