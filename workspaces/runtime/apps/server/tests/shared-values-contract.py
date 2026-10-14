#!/usr/bin/env python3
"""Generated Rust and TypeScript runtime values stay in sync with Python owners."""

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import unittest

from studio_api.generate_shared_values import main
from studio_api.generate_http_body_limits import OUTPUT as HTTP_LIMITS_OUTPUT, render_rust_http_body_limits
from codex_process_supervisor import MAX_FRAME_BYTES, PROTOCOL
from codex_layout import REPOSITORY_ROOT
from codex_http_body_limits import REQUEST_BODY_READ_TIMEOUT_SECONDS
from codex_shared_values import (
    MAX_SYNC_RESOURCE_COUNT,
    MAX_SYNC_RESOURCE_QUERY_BYTES,
    MAX_SUPERVISOR_FRAME_BYTES,
    PROCESS_SUPERVISOR_PROTOCOL_VERSION,
    SYNC_BATCH_LIMIT,
    SYNC_ENTITY_PAGE_LIMIT,
    SYNC_RESOURCE_HEARTBEAT_SECONDS,
    SYNC_STREAM_BYTE_LIMIT,
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

    def test_sync_protocol_capabilities_share_owned_limits(self):
        source_root = Path(__file__).resolve().parents[1] / "src"
        python_route = (source_root / "studio_api/sync/router.py").read_text(encoding="utf-8")
        python_store = (source_root / "codex_sync.py").read_text(encoding="utf-8")
        rust_policy = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-services/src/sync_policy.rs"
        ).read_text(encoding="utf-8")
        rust_docs = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-services/src/sync_documents.rs"
        ).read_text(encoding="utf-8")
        rust_http = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http/src/routes/sync_documents.rs"
        ).read_text(encoding="utf-8")
        rust_stream = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http/src/routes/sync_stream.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual(
            (SYNC_BATCH_LIMIT, SYNC_ENTITY_PAGE_LIMIT, SYNC_STREAM_BYTE_LIMIT,
             SYNC_RESOURCE_HEARTBEAT_SECONDS),
            (100, 500, 1_048_576, 15),
        )
        for value in ("SYNC_BATCH_LIMIT", "SYNC_ENTITY_PAGE_LIMIT", "SYNC_STREAM_BYTE_LIMIT"):
            self.assertIn(value, python_route)
            self.assertIn(value, rust_http if value == "SYNC_STREAM_BYTE_LIMIT" else rust_policy)
        self.assertIn("SYNC_ENTITY_PAGE_LIMIT as usize", rust_docs)
        self.assertIn("SYNC_BATCH_LIMIT", python_store)
        self.assertIn("SYNC_ENTITY_PAGE_LIMIT", python_store)
        self.assertIn('query_integer(&query, "limit", sync_policy::OTHER_PAGE_LIMIT)', rust_http)
        self.assertIn("SYNC_RESOURCE_HEARTBEAT_SECONDS", rust_stream)

    def test_http_request_body_deadline_has_generated_rust_consumer(self):
        middleware = (
            REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/studio_api/middleware.py"
        ).read_text(encoding="utf-8")
        rust_middleware = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http/src/middleware.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual(REQUEST_BODY_READ_TIMEOUT_SECONDS, 10)
        self.assertEqual(HTTP_LIMITS_OUTPUT.read_text(encoding="utf-8"), render_rust_http_body_limits())
        self.assertIn("REQUEST_READ_TIMEOUT_SECONDS = float(REQUEST_BODY_READ_TIMEOUT_SECONDS)", middleware)
        self.assertIn("Duration::from_secs(REQUEST_BODY_READ_TIMEOUT_SECONDS)", rust_middleware)

    def test_supervisor_frame_cap_uses_the_shared_owner(self):
        python_source = (
            REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/codex_process_supervisor.py"
        ).read_text(encoding="utf-8")
        windows_source = (
            REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/codex_windows_supervisor.py"
        ).read_text(encoding="utf-8")
        rust_source = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-process/src/transport.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual(MAX_FRAME_BYTES, MAX_SUPERVISOR_FRAME_BYTES)
        self.assertEqual(MAX_SUPERVISOR_FRAME_BYTES, 1_048_576)
        self.assertIn("MAX_FRAME_BYTES = MAX_SUPERVISOR_FRAME_BYTES", python_source)
        self.assertIn("reader.readline(MAX_SUPERVISOR_FRAME_BYTES + 1", windows_source)
        self.assertIn("MAX_SUPERVISOR_FRAME_BYTES", rust_source)


if __name__ == "__main__":
    unittest.main()
