#!/usr/bin/env python3
"""Keep sync protocol version ownership and generated clients aligned."""

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import json
import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codex_layout import REPOSITORY_ROOT, WEB_ROOT
from studio_api.generate_types import (
    OPENAPI_AUTHORITY,
    SYNC_PROTOCOL_NODE_OUTPUT,
    SYNC_PROTOCOL_OUTPUT,
    SYNC_PROTOCOL_PYTHON_OUTPUT,
    render_sync_protocol_python,
    render_sync_protocol_node,
    render_sync_protocol_rust,
    render_sync_protocol_typescript,
)
from studio_api.sync.models import SyncProtocolResponse
from studio_api.sync.protocol import SYNC_ENTITY_SCOPE, SYNC_PROTOCOL_VERSION
from studio_api.sync.resources.models import (
    ResourceChangeEvent,
    ResourceHeartbeatEvent,
    ResourceTokenRatesEvent,
)


def single_value(schema):
    if "const" in schema:
        return schema["const"]
    values = schema.get("enum", [])
    return values[0] if len(values) == 1 else None


class SyncProtocolContract(unittest.TestCase):
    def test_shared_owner_preserves_the_supported_openapi_version(self):
        self.assertEqual(SYNC_PROTOCOL_VERSION, 3)
        self.assertEqual(SYNC_ENTITY_SCOPE, "state:entities:v1")
        schema = SyncProtocolResponse.model_json_schema()["properties"]
        self.assertEqual(single_value(schema["protocolVersion"]), SYNC_PROTOCOL_VERSION)
        self.assertEqual(single_value(schema["supportedVersions"]["items"]), SYNC_PROTOCOL_VERSION)
        for model in (ResourceChangeEvent, ResourceHeartbeatEvent, ResourceTokenRatesEvent):
            with self.subTest(model=model.__name__):
                self.assertEqual(
                    single_value(model.model_json_schema()["properties"]["protocol"]),
                    SYNC_PROTOCOL_VERSION,
                )

    def test_generated_runtime_constants_match_the_python_owner(self):
        document = json.loads(OPENAPI_AUTHORITY.read_text(encoding="utf-8"))
        self.assertEqual(
            SYNC_PROTOCOL_OUTPUT.read_text(encoding="utf-8"),
            render_sync_protocol_rust(document),
        )
        self.assertEqual(
            SYNC_PROTOCOL_PYTHON_OUTPUT.read_text(encoding="utf-8"),
            render_sync_protocol_python(document),
        )
        api_types = (WEB_ROOT / "src/generated/api.ts").read_text(encoding="utf-8")
        self.assertTrue(
            api_types.endswith(f"\n{render_sync_protocol_typescript(document)}\n")
        )
        self.assertEqual(
            SYNC_PROTOCOL_NODE_OUTPUT.read_text(encoding="utf-8"),
            render_sync_protocol_node(document),
        )
        sources = (
            "workspaces/runtime/apps/server/src/codex_api_client.py",
            "workspaces/runtime/apps/server/src/codex_sync.py",
            "workspaces/runtime/apps/server/src/codex_http_traces.py",
            "workspaces/runtime/apps/server/src/studio_api/sync/router.py",
            "workspaces/runtime/apps/server/src/studio_api/testing.py",
            "workspaces/runtime/packages/runtime-http/src/routes/sync_documents.rs",
            "workspaces/runtime/packages/runtime-services/src/sync_policy.rs",
            "workspaces/client/apps/web/src/sync/client.ts",
            "workspaces/client/apps/web/src/sync/entitySequence.ts",
            "workspaces/client/apps/web/src/sync/pullContract.ts",
            "workspaces/client/apps/web/src/sync/uiPreferenceConnection.ts",
            "workspaces/client/apps/web/src/api.ts",
            "workspaces/runtime/apps/server/src/sync/benchmarks/benchmark.py",
            "workspaces/runtime/apps/server/src/studio_api/benchmarks/runtime_load/identity.mjs",
        )
        for source in sources:
            with self.subTest(source=source):
                contents = (REPOSITORY_ROOT / source).read_text(encoding="utf-8")
                self.assertIn("SYNC_ENTITY_SCOPE", contents)
                self.assertNotIn('"state:entities:v1"', contents)


if __name__ == "__main__":
    unittest.main()
