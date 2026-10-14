#!/usr/bin/env python3
"""Mutation response entity envelopes match the durable sync range."""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import unittest
from contextlib import closing

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


class MutationSyncEnvelopeHTTPContract(TimedHTTPContractTest, unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.rust_backend = bool(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"))
        cls.backend = start_contract_backend(cls)

    @classmethod
    def tearDownClass(cls) -> None:
        cls.backend.stop()

    def headers(self) -> dict[str, str]:
        return {
            "Origin": self.backend.origin or "",
            "X-Canvas-Token": self.backend.session["token"],
            "X-Studio-API-Schema": self.backend.schema_hash,
        }

    def post(self, path: str, body: dict[str, object]):
        return self.backend.request("POST", path, body=body, headers=self.headers())

    def cursor(self) -> int:
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            return db.execute(
                "SELECT COALESCE(MAX(seq),0) FROM sync_entities "
                "WHERE collection NOT LIKE 'transcript:%'"
            ).fetchone()[0]

    def assert_envelope_matches_projection(self, body: dict[str, object], after: int) -> list[str]:
        entities = body.get("_syncEntities")
        self.assertIsInstance(entities, list)
        self.assertGreater(len(entities), 0)
        typed_entities = entities
        through = max(row["seq"] for row in typed_entities)
        self.assertEqual(body.get("_syncEntitiesAfter"), after)
        with closing(sqlite3.connect(self.backend.state / "canvas.sqlite3")) as db:
            projected = db.execute(
                "SELECT collection,id,seq,hash,payload,deleted FROM sync_entities "
                "WHERE seq>? AND seq<=? AND collection NOT LIKE 'transcript:%' ORDER BY seq",
                (after, through),
            ).fetchall()
        self.assertEqual(len(typed_entities), len(projected))
        relative_sequences = []
        entity_ids = []
        for entity, row in zip(typed_entities, projected, strict=True):
            collection, identity, sequence, payload_hash, payload, deleted = row
            self.assertEqual(entity["id"], f"entity:{collection}:{identity}")
            self.assertEqual(entity["seq"], sequence)
            self.assertEqual(entity["payload"], payload)
            self.assertEqual(hashlib.sha256(payload.encode("utf-8")).hexdigest(), payload_hash)
            self.assertEqual(entity["_deleted"], bool(deleted))
            relative_sequences.append(sequence - after)
            entity_ids.append(entity["id"])
        self.assertEqual(relative_sequences, sorted(relative_sequences))
        return entity_ids

    def test_agent_creation_carries_its_entity_range(self) -> None:
        before = self.cursor()
        identity = "00000000-0000-4000-8000-000000000001"
        response = self.post(
            "/api/leads",
            {"id": identity, "cwd": str(self.backend.cwd), "workspaceMode": "worktree"},
        )
        self.assertEqual(response.status, 200, response.body)
        ids = self.assert_envelope_matches_projection(response.json(), before)
        self.assertIn(f"entity:agent:{identity}", ids)

    def test_conversation_delete_carries_its_tombstone(self) -> None:
        identity = "00000000-0000-4000-8000-000000000002"
        created = self.post(
            "/api/leads",
            {"id": identity, "cwd": str(self.backend.cwd), "workspaceMode": "worktree"},
        )
        self.assertEqual(created.status, 200, created.body)
        before = self.cursor()
        response = self.post("/api/conversation/delete", {"id": identity})
        self.assertEqual(response.status, 200, response.body)
        ids = self.assert_envelope_matches_projection(response.json(), before)
        self.assertIn(f"entity:agent:{identity}", ids)

    def test_settings_write_preserves_the_python_response_envelope(self) -> None:
        before = self.cursor()
        response = self.post(
            "/api/sync/preferences",
            {
                "scope": "user",
                "fields": {
                    '["codex-studio-preferences-v1","theme"]': {
                        "value": "dark",
                        "timestamp": 1,
                        "writer": "sync-envelope-contract",
                    }
                },
            },
        )
        self.assertEqual(response.status, 200, response.body)
        self.assertIn(
            "entity:uiPreferences:user",
            self.assert_envelope_matches_projection(response.json(), before),
        )

    def test_failed_mutation_has_no_envelope(self) -> None:
        response = self.post(
            "/api/conversation/delete", {"id": "00000000-0000-4000-8000-000000000003"}
        )
        self.assertGreaterEqual(response.status, 400, response.body)
        self.assertNotIn("_syncEntities", response.json())
        self.assertNotIn("_syncEntitiesAfter", response.json())

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust named refusal")
    def test_named_501_has_no_envelope(self) -> None:
        response = self.post(
            "/api/action",
            {
                "id": "00000000-0000-4000-8000-000000000004",
                "action": "review",
                "request_id": "sync-envelope-native-action",
            },
        )
        self.assertEqual(response.status, 501, response.body)
        self.assertIn("Not implemented: requires ", response.json()["error"])
        self.assertNotIn("_syncEntities", response.json())
        self.assertNotIn("_syncEntitiesAfter", response.json())


if __name__ == "__main__":
    unittest.main()
