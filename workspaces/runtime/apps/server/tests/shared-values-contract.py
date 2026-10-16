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
    ACCOUNT_LIMITS_ERROR_RETRY_SECONDS,
    ACCOUNT_LIMITS_FULL_READ_FRESH_SECONDS,
    ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS,
    MAX_PROJECT_SECRET_VALUE_BYTES,
    MAX_SYNC_RESOURCE_COUNT,
    MAX_SYNC_RESOURCE_QUERY_BYTES,
    MAX_SUPERVISOR_FRAME_BYTES,
    PROCESS_SUPERVISOR_PROTOCOL_VERSION,
    PROGRESS_LAYOUT_FRESH_SECONDS,
    PROGRESS_LAYOUT_MAX_BYTES,
    PROGRESS_LAYOUT_MAX_CLIENTS,
    SYNC_BATCH_LIMIT,
    SYNC_ENTITY_PAGE_LIMIT,
    SYNC_RESOURCE_HEARTBEAT_SECONDS,
    SYNC_STREAM_BYTE_LIMIT,
    TRANSCRIPT_MAX_TOMBSTONES,
)
from codex_sync import TRANSCRIPT_MAX_TOMBSTONES as SYNC_TRANSCRIPT_MAX_TOMBSTONES
from studio_api.agents.models import ProjectSecretSetRequest
from studio_api.models import MAX_SAFE_INTEGER
from studio_api.sync.models import DraftPushRequest


class SharedValuesContract(unittest.TestCase):
    def test_generated_cross_language_values_match_the_authority(self):
        self.assertEqual(main(["--check"]), 0)

    def test_account_limits_freshness_uses_generated_rust_and_python_values(self):
        rust_values = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-schema/src/shared_values.rs"
        ).read_text(encoding="utf-8")
        rust_reader = (
            REPOSITORY_ROOT
            / "workspaces/runtime/packages/runtime-services/src/account_session_runtime.rs"
        ).read_text(encoding="utf-8")
        python_reader = (
            REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/codex_runtime.py"
        ).read_text(encoding="utf-8")
        web_values = (
            REPOSITORY_ROOT / "workspaces/client/apps/web/src/generated/sharedValues.ts"
        ).read_text(encoding="utf-8")
        web_readers = (
            REPOSITORY_ROOT / "workspaces/client/apps/web/src/usage/accountUsage.ts",
            REPOSITORY_ROOT / "workspaces/client/apps/web/src/App.tsx",
        )
        values = (
            ("ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS", ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS),
            ("ACCOUNT_LIMITS_FULL_READ_FRESH_SECONDS", ACCOUNT_LIMITS_FULL_READ_FRESH_SECONDS),
            ("ACCOUNT_LIMITS_ERROR_RETRY_SECONDS", ACCOUNT_LIMITS_ERROR_RETRY_SECONDS),
        )
        self.assertEqual(tuple(value for _, value in values), (60, 600, 30))
        for name, value in values:
            with self.subTest(contract=name):
                self.assertIn(f"pub const {name}: f64 = {value}.0;", rust_values)
                self.assertIn(name, rust_reader)
                self.assertIn(name, python_reader)
                if name == "ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS":
                    self.assertIn(f"export const {name} = {value};", web_values)
                    for web_reader in web_readers:
                        self.assertIn(name, web_reader.read_text(encoding="utf-8"))

    def test_javascript_safe_integer_ceiling_has_generated_rust_and_web_consumers(self):
        self.assertEqual(MAX_SAFE_INTEGER, 9_007_199_254_740_991)
        rust_values = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-schema/src/shared_values.rs"
        ).read_text(encoding="utf-8")
        web_values = (
            REPOSITORY_ROOT / "workspaces/client/apps/web/src/generated/sharedValues.ts"
        ).read_text(encoding="utf-8")
        web_sync = (
            REPOSITORY_ROOT / "workspaces/client/apps/web/src/sync/client.ts"
        ).read_text(encoding="utf-8")
        self.assertIn(f"pub const MAX_SAFE_INTEGER: i64 = {MAX_SAFE_INTEGER};", rust_values)
        self.assertIn(f"export const MAX_SAFE_INTEGER = {MAX_SAFE_INTEGER};", web_values)
        self.assertIn("maximum: MAX_SAFE_INTEGER", web_sync)
        for relative in (
            "workspaces/runtime/packages/runtime-http/src/validation.rs",
            "workspaces/runtime/packages/runtime-services/src/change_notifications.rs",
            "workspaces/runtime/packages/runtime-services/src/workspace_views.rs",
            "workspaces/runtime/packages/runtime-services/src/agent_management.rs",
            "workspaces/runtime/packages/runtime-services/src/progress_panel.rs",
            "workspaces/runtime/packages/runtime-services/src/voice.rs",
            "workspaces/runtime/packages/runtime-services/src/continuation.rs",
            "workspaces/runtime/packages/runtime-services/src/analytics_capture.rs",
            "workspaces/runtime/packages/runtime-services/src/analytics_history.rs",
        ):
            source = (REPOSITORY_ROOT / relative).read_text(encoding="utf-8")
            with self.subTest(source=relative):
                self.assertIn("MAX_SAFE_INTEGER", source)
                self.assertNotIn("9_007_199_254_740_991", source)
        for relative in (
            "workspaces/runtime/apps/server/src/studio_api/analytics_capture_models.py",
            "workspaces/runtime/apps/server/src/studio_api/persisted_continuations.py",
            "workspaces/runtime/apps/server/src/studio_api/relocation_models.py",
            "workspaces/runtime/apps/server/src/studio_api/request_schema_audit.py",
            "workspaces/runtime/apps/server/src/studio_api/sync/preferences.py",
            "workspaces/runtime/apps/server/src/studio_api/sync/resources/models.py",
            "workspaces/runtime/apps/server/src/codex_progress_layout.py",
        ):
            source = (REPOSITORY_ROOT / relative).read_text(encoding="utf-8")
            with self.subTest(source=relative):
                self.assertIn("MAX_SAFE_INTEGER", source)
                self.assertNotIn("9_007_199_254_740_991", source)

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
        rust_store = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-store/src/repositories/sync_documents.rs"
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
        self.assertIn("limit.clamp(1, SYNC_ENTITY_PAGE_LIMIT)", rust_store)
        self.assertIn("limit.clamp(1, SYNC_BATCH_LIMIT)", rust_store)
        self.assertIn("SYNC_BATCH_LIMIT", python_store)
        self.assertIn("SYNC_ENTITY_PAGE_LIMIT", python_store)
        self.assertIn('query_integer(&query, "limit", sync_policy::OTHER_PAGE_LIMIT)', rust_http)
        self.assertIn("SYNC_RESOURCE_HEARTBEAT_SECONDS", rust_stream)

    def test_sync_batch_and_mutation_delta_use_shared_limits(self):
        self.assertEqual(
            DraftPushRequest.model_json_schema()["properties"]["rows"]["maxItems"],
            SYNC_BATCH_LIMIT,
        )
        route = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-http/src/routes/sync_delta.rs"
        ).read_text(encoding="utf-8")
        store = (
            REPOSITORY_ROOT
            / "workspaces/runtime/packages/runtime-store/src/repositories/sync_documents.rs"
        ).read_text(encoding="utf-8")
        models = (
            Path(__file__).resolve().parents[1] / "src/studio_api/sync/models.py"
        ).read_text(encoding="utf-8")
        self.assertIn("rows: list[DraftPushRow] = Field(max_length=SYNC_BATCH_LIMIT)",
                      models)
        self.assertIn("usize::try_from(SYNC_ENTITY_PAGE_LIMIT)", route)
        self.assertIn("let fetch_limit = SYNC_ENTITY_PAGE_LIMIT", route)
        self.assertIn(".checked_add(1)", route)
        self.assertIn("rows.len() <= page_limit", route)
        self.assertIn(".take(page_limit)", route)
        self.assertIn(
            "limit.clamp(1, SYNC_ENTITY_PAGE_LIMIT.saturating_add(1))",
            store,
        )

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

    def test_project_secret_schema_bound_uses_the_byte_limit_owner(self):
        schema = ProjectSecretSetRequest.model_json_schema()
        self.assertEqual(
            schema["properties"]["value"]["maxLength"],
            MAX_PROJECT_SECRET_VALUE_BYTES,
        )

    def test_model_catalog_limits_have_generated_rust_and_python_consumers(self):
        names = (
            "MODEL_CATALOG_MAX_FRAME_BYTES",
            "MODEL_CATALOG_MAX_RESPONSE_BYTES",
            "MODEL_CATALOG_MAX_PAGES",
            "MODEL_CATALOG_ITEMS_PER_PAGE",
            "MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS",
            "MODEL_CATALOG_CACHE_TTL_SECONDS",
        )
        rust_schema = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-schema/src/shared_values.rs"
        ).read_text(encoding="utf-8")
        rust_catalog = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-services/src/model_catalog.rs"
        ).read_text(encoding="utf-8")
        python_reader = (
            REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/codex_model_catalog_reader.py"
        ).read_text(encoding="utf-8")
        python_cache = (
            REPOSITORY_ROOT / "workspaces/runtime/apps/server/src/codex_catalog.py"
        ).read_text(encoding="utf-8")
        for name in names:
            self.assertIn(name, rust_schema)
            self.assertIn(name, rust_catalog)
        for name in names[:5]:
            self.assertIn(name, python_reader)
        self.assertIn("MODEL_CATALOG_CACHE_TTL_SECONDS", python_cache)
        self.assertIn("MODEL_CATALOG_ITEMS_PER_PAGE", python_cache)

    def test_progress_layout_contract_uses_generated_limits(self):
        source_root = Path(__file__).resolve().parents[1] / "src"
        python_source = (source_root / "codex_progress_layout.py").read_text(encoding="utf-8")
        rust_source = (
            REPOSITORY_ROOT / "workspaces/runtime/packages/runtime-services/src/progress_panel.rs"
        ).read_text(encoding="utf-8")
        self.assertEqual(
            (PROGRESS_LAYOUT_FRESH_SECONDS, PROGRESS_LAYOUT_MAX_CLIENTS, PROGRESS_LAYOUT_MAX_BYTES),
            (90, 32, 65_536),
        )
        for name in (
            "PROGRESS_LAYOUT_FRESH_SECONDS",
            "PROGRESS_LAYOUT_MAX_CLIENTS",
            "PROGRESS_LAYOUT_MAX_BYTES",
        ):
            self.assertIn(name, python_source)
            self.assertIn(name, rust_source)

if __name__ == "__main__":
    unittest.main()
