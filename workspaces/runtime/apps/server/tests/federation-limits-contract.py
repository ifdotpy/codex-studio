#!/usr/bin/env python3
"""Parity and boundary contract for federation message and batch limits."""

import sys
from pathlib import Path
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from codex_agent_message_limits import MAX_AGENT_MESSAGE_CODEPOINTS
from codex_federation_limits import (
    FEDERATION_BODY_LIMIT,
    FEDERATION_PATHS,
    MAX_BATCH_BYTES,
    MAX_BATCH_ITEMS,
    MAX_MESSAGE_CODEPOINTS,
    MAX_ROOM_MEMBERS,
)
from codex_federation import MAX_BODY, MAX_BATCH, MAX_MESSAGE
from studio_api.federation.models import CreateRoomRequest, PullRequest
from studio_api.federation.router import FEDERATION_PATHS as ROUTER_PATHS, MAX_BODY_BYTES
from studio_api.middleware import FEDERATION_PATHS as MIDDLEWARE_PATHS
from studio_api.persisted_federation import (
    FederationChatPayload,
    FederationRoomPayload,
    PersistedFederationRoom,
)
from studio_api.generate_federation_limits import OUTPUTS


class FederationLimitsContract(unittest.TestCase):
    def test_protocol_limits_keep_their_existing_values(self):
        self.assertEqual(MAX_MESSAGE_CODEPOINTS, MAX_AGENT_MESSAGE_CODEPOINTS)
        self.assertEqual(MAX_MESSAGE_CODEPOINTS, 12_000)
        self.assertEqual(MAX_BATCH_ITEMS, 50)
        self.assertEqual(MAX_BATCH, MAX_BATCH_ITEMS)
        self.assertEqual(MAX_MESSAGE, MAX_MESSAGE_CODEPOINTS)
        self.assertEqual(MAX_BATCH_BYTES, 200 * 1024)
        self.assertEqual(MAX_ROOM_MEMBERS, 50)
        self.assertEqual(FEDERATION_BODY_LIMIT, 256 * 1024)

    def test_generated_rust_output_matches_the_python_owner(self):
        for path, expected in OUTPUTS.items():
            with self.subTest(path=path.name):
                self.assertEqual(path.read_text(encoding="utf-8"), expected)

    def test_message_and_list_schemas_use_the_owner(self):
        self.assertEqual(
            FederationChatPayload.model_json_schema()["properties"]["text"]["maxLength"],
            MAX_MESSAGE_CODEPOINTS,
        )
        self.assertEqual(
            FederationRoomPayload.model_json_schema()["properties"]["participants"]["maxItems"],
            MAX_ROOM_MEMBERS,
        )
        self.assertEqual(
            CreateRoomRequest.model_json_schema()["properties"]["local_members"]["maxItems"],
            MAX_ROOM_MEMBERS,
        )
        room_schema = PersistedFederationRoom.model_json_schema()["properties"]
        for field in ("localMembers", "remoteMembers", "localParticipants"):
            with self.subTest(field=field):
                self.assertEqual(room_schema[field]["maxItems"], MAX_ROOM_MEMBERS)
        self.assertEqual(
            PullRequest.model_json_schema()["properties"]["acks"]["maxItems"],
            MAX_BATCH_ITEMS,
        )

    def test_router_and_http_middleware_share_body_cap_and_paths(self):
        self.assertEqual(MAX_BODY, FEDERATION_BODY_LIMIT)
        self.assertEqual(MAX_BODY_BYTES, FEDERATION_BODY_LIMIT)
        self.assertEqual(ROUTER_PATHS, FEDERATION_PATHS)
        self.assertEqual(MIDDLEWARE_PATHS, FEDERATION_PATHS)

    def test_federation_chat_payload_counts_codepoints(self):
        base = {"sender": "worker", "created": 1.0, "sequence": 1}
        FederationChatPayload.model_validate(
            {**base, "text": "😀" * MAX_MESSAGE_CODEPOINTS}
        )
        with self.assertRaises(ValueError):
            FederationChatPayload.model_validate(
                {**base, "text": "😀" * (MAX_MESSAGE_CODEPOINTS + 1)}
            )


if __name__ == "__main__":
    unittest.main()
