#!/usr/bin/env python3
"""Generated Rust and TypeScript runtime values stay in sync with Python owners."""

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import unittest

from studio_api.generate_shared_values import main
from codex_process_supervisor import PROTOCOL
from codex_shared_values import PROCESS_SUPERVISOR_PROTOCOL_VERSION, TRANSCRIPT_MAX_TOMBSTONES
from codex_sync import TRANSCRIPT_MAX_TOMBSTONES as SYNC_TRANSCRIPT_MAX_TOMBSTONES


class SharedValuesContract(unittest.TestCase):
    def test_generated_cross_language_values_match_the_authority(self):
        self.assertEqual(main(["--check"]), 0)

    def test_python_supervisor_uses_the_named_handshake_version(self):
        self.assertEqual(PROTOCOL, PROCESS_SUPERVISOR_PROTOCOL_VERSION)

    def test_sync_store_uses_the_shared_transcript_tombstone_limit(self):
        self.assertEqual(SYNC_TRANSCRIPT_MAX_TOMBSTONES, TRANSCRIPT_MAX_TOMBSTONES)


if __name__ == "__main__":
    unittest.main()
