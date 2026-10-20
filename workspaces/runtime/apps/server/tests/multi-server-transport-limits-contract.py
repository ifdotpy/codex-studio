#!/usr/bin/env python3
"""Cross-language parity for signed multi-server transport byte limits."""

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codex_multi_server_transport_limits import (
    MULTI_SERVER_MAX_REQUEST_BYTES,
    MULTI_SERVER_MAX_RESPONSE_BYTES,
)


class MultiServerTransportLimitsContract(unittest.TestCase):
    def test_existing_transport_boundaries_remain_unchanged(self):
        self.assertEqual(MULTI_SERVER_MAX_REQUEST_BYTES, 256 * 1024)
        self.assertEqual(MULTI_SERVER_MAX_RESPONSE_BYTES, 1024 * 1024)


if __name__ == "__main__":
    unittest.main()
