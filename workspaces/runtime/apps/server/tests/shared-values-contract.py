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
from codex_layout import REPOSITORY_ROOT
from codex_shared_values import (
    MAX_SYNC_RESOURCE_COUNT,
    MAX_SYNC_RESOURCE_QUERY_BYTES,
    PROCESS_SUPERVISOR_PROTOCOL_VERSION,
    TRANSCRIPT_MAX_TOMBSTONES,
)
from codex_sync import TRANSCRIPT_MAX_TOMBSTONES as SYNC_TRANSCRIPT_MAX_TOMBSTONES


class SharedValuesContract(unittest.TestCase):
    def test_generated_cross_language_values_match_the_authority(self):
        self.assertEqual(main(["--check"]), 0)

    def test_python_supervisor_uses_the_named_handshake_version(self):
        self.assertEqual(PROTOCOL, PROCESS_SUPERVISOR_PROTOCOL_VERSION)

    def test_sync_store_uses_the_shared_transcript_tombstone_limit(self):
        self.assertEqual(SYNC_TRANSCRIPT_MAX_TOMBSTONES, TRANSCRIPT_MAX_TOMBSTONES)

    def test_sync_stream_bounds_have_one_generated_owner(self):
        source_root = Path(__file__).resolve().parents[1] / "src"
        router = (source_root / "studio_api/sync/router.py").read_text(encoding="utf-8")
        rust_route = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http/src/routes/sync_stream.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual((MAX_SYNC_RESOURCE_QUERY_BYTES, MAX_SYNC_RESOURCE_COUNT), (65_536, 256))
        self.assertIn("MAX_RESOURCE_QUERY_BYTES = MAX_SYNC_RESOURCE_QUERY_BYTES", router)
        self.assertIn("MAX_RESOURCE_COUNT = MAX_SYNC_RESOURCE_COUNT", router)
        self.assertIn("raw.len() > MAX_SYNC_RESOURCE_QUERY_BYTES", rust_route)
        self.assertIn("values.len() > MAX_SYNC_RESOURCE_COUNT", rust_route)


if __name__ == "__main__":
    unittest.main()
