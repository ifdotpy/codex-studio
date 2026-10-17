#!/usr/bin/env python3
"""Mutation response entity envelopes match the durable sync range."""

from __future__ import annotations

import hashlib
import json
import os
import sqlite3
import sys
import unittest
from contextlib import closing

from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

from http_backend_harness import TimedHTTPContractTest, start_contract_backend


# Captured from the Python HTTP backend. Hashes use canonical JSON after replacing
# only workspace paths, timestamps, and process-local server ids with sentinels.
PYTHON_ENVELOPE_GOLDENS: dict[str, list[dict[str, object]]] = {
    "agent_creation": [
        {
            "id": "entity:project:<backend-cwd>",
            "payloadHash": "f34f0dbb560a04c0bc57687ef1322777656790ae2f85d24c664b4b1ce27bf61e",
            "relativeSeq": 1,
        },
        {
            "id": "entity:agent:00000000-0000-4000-8000-000000000001",
            "payloadHash": "d28f2fef85136a7e43910c66da03c49d966a6d5cba310f380f41eee2df002952",
            "relativeSeq": 2,
        },
    ],
    "conversation_delete": [
        {
            "id": "entity:agent:00000000-0000-4000-8000-000000000002",
            "payloadHash": "4fdf130c0bf773a9d28f07efe401ea598f4d11135d4ccc1fd1136a716953f5ce",
            "relativeSeq": 1,
        },
    ],
    "settings_write": [
        {
            "id": "entity:uiPreferences:user",
            "payloadHash": "6c440f0529b84d1297a23ddff642bd4b8361979aa81b9712df794d0997bb54c5",
            "relativeSeq": 1,
        },
    ],
}


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
            high = db.execute(
                "SELECT COALESCE(MAX(seq),0) FROM sync_entities "
                "WHERE collection NOT LIKE 'transcript:%'"
            ).fetchone()[0]
            floor = db.execute(
                "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_floor'"
            ).fetchone()
            return max(high, int(floor[0]) if floor else 0)

    def assert_envelope_matches_projection(
        self, body: dict[str, object], after: int, scenario: str
    ) -> list[str]:
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
        python_comparable = []
        for entity, row in zip(typed_entities, projected, strict=True):
            collection, identity, sequence, payload_hash, payload, deleted = row
            self.assertEqual(entity["id"], f"entity:{collection}:{identity}")
            self.assertEqual(entity["seq"], sequence)
            self.assertEqual(entity["payload"], payload)
            self.assertEqual(hashlib.sha256(payload.encode("utf-8")).hexdigest(), payload_hash)
            self.assertEqual(entity["_deleted"], bool(deleted))
            relative_sequences.append(sequence - after)
            entity_ids.append(entity["id"])
            normalized = self._normalize_payload(json.loads(payload), str(self.backend.cwd))
            if collection == "project" and sys.platform == "darwin":
                # The portable golden predates macOS-only vault readiness. Check
                # that contract explicitly before comparing the shared fields.
                self.assertEqual(normalized["value"].pop("layrReadiness"),
                                 {"state": "not_imported"})
            normalized_payload = json.dumps(
                normalized,
                sort_keys=True,
                separators=(",", ":"),
            )
            python_comparable.append(
                {
                    "id": entity["id"].replace(str(self.backend.cwd), "<backend-cwd>"),
                    "payloadHash": hashlib.sha256(normalized_payload.encode("utf-8")).hexdigest(),
                    "relativeSeq": sequence - after,
                }
            )
        self.assertEqual(relative_sequences, sorted(relative_sequences))
        self.assertEqual(python_comparable, PYTHON_ENVELOPE_GOLDENS[scenario],
                         [self._normalize_payload(json.loads(row[4]), str(self.backend.cwd))
                          for row in projected])
        return entity_ids

    @classmethod
    def _normalize_payload(cls, value: object, backend_cwd: str) -> object:
        if isinstance(value, dict):
            return {
                key: (
                    "<timestamp>"
                    if key == "created"
                    else "<server-id>"
                    if key in {"serverId", "homeServerId"}
                    else cls._normalize_payload(child, backend_cwd)
                )
                for key, child in value.items()
            }
        if isinstance(value, list):
            return [cls._normalize_payload(child, backend_cwd) for child in value]
        if isinstance(value, str):
            return value.replace(backend_cwd, "<backend-cwd>")
        return value

    def test_agent_creation_carries_its_entity_range(self) -> None:
        before = self.cursor()
        identity = "00000000-0000-4000-8000-000000000001"
        response = self.post(
            "/api/leads",
            {"id": identity, "cwd": str(self.backend.cwd), "workspaceMode": "worktree"},
        )
        self.assertEqual(response.status, 200, response.body)
        ids = self.assert_envelope_matches_projection(response.json(), before, "agent_creation")
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
        ids = self.assert_envelope_matches_projection(response.json(), before, "conversation_delete")
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
            self.assert_envelope_matches_projection(response.json(), before, "settings_write"),
        )

    def test_failed_mutation_has_no_envelope(self) -> None:
        response = self.post(
            "/api/conversation/delete", {"id": "00000000-0000-4000-8000-000000000003"}
        )
        self.assertGreaterEqual(response.status, 400, response.body)
        self.assertNotIn("_syncEntities", response.json())
        self.assertNotIn("_syncEntitiesAfter", response.json())

    @unittest.skipUnless(os.environ.get("STUDIO_HTTP_BACKEND_COMMAND"), "Rust mutation failure")
    def test_invalid_native_action_has_no_envelope(self) -> None:
        # The native-action consumer is present; the unknown agent is rejected
        # with a client error, whose body must never receive a success envelope.
        response = self.post(
            "/api/action",
            {
                "id": "00000000-0000-4000-8000-000000000004",
                "action": "review",
                "request_id": "sync-envelope-native-action",
            },
        )
        self.assertEqual(response.status, 400, response.body)
        self.assertNotIn("_syncEntities", response.json())
        self.assertNotIn("_syncEntitiesAfter", response.json())


if __name__ == "__main__":
    unittest.main()
