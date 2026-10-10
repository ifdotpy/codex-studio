"""Direct state updates preserve complete, bounded checkpoint coverage."""

from __future__ import annotations

import asyncio
import json
from pathlib import Path
import sqlite3
import tempfile
from typing import Any
import unittest
from unittest.mock import patch

from codex_sync_entities import ensure_tables, put
from studio_api.models import SyncEntity
from studio_api.sync.resources import hub as hub_module
from studio_api.sync.resources.hub import (
    EntityPublicationScheduler, ResourceHub, register_resource_hub, unregister_resource_hub,
)
from studio_api.sync.resources.models import (
    EntityChangeBatch, MAX_ENTITY_CHANGE_BYTES, MAX_ENTITY_CHANGE_DOCUMENTS,
    ResourceChangeEvent, ResourceRef, StateResource,
)


def batch(after: int, through: int, identity: str = "a", payload_size: int = 0) -> EntityChangeBatch:
    return EntityChangeBatch(after=after, through=through, documents=[SyncEntity(
        id=f"entity:project:{identity}", seq=through, deleted=False,
        payload=json.dumps({"collection": "project", "id": identity,
                            "value": {"id": identity, "name": "x" * payload_size}}),
    )])


class EntityChangesTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.path = self.root / "canvas.sqlite3"
        self.database = sqlite3.connect(self.path)
        self.database.execute("PRAGMA journal_mode=WAL")
        ensure_tables(self.database)
        self.hub = ResourceHub("workspace-a")
        register_resource_hub(self.root, self.hub)
        self.state = ResourceRef(StateResource(kind="state"))
        self.subscription = self.hub.subscribe([self.state], loop=asyncio.get_running_loop())

    async def asyncTearDown(self) -> None:
        self.hub.close()
        unregister_resource_hub(self.root, self.hub)
        self.database.close()
        self.temporary.cleanup()

    def write(self, identity: str, name: str = "A", *, deleted: bool = False) -> None:
        with self.database:
            put(self.database, "project", identity, {"id": identity, "name": name}, deleted)

    def publish(self, sequence: int = 0, sequences: set[int] | None = None) -> None:
        EntityPublicationScheduler._publish(
            str(self.root), self.path, 0.0, 1.0, sequence, sequences or set(), False,
        )

    async def receive(self) -> ResourceChangeEvent:
        event = await self.subscription.next_event(timeout=1)
        assert isinstance(event, ResourceChangeEvent)
        return event

    async def test_committed_update_and_delete_have_direct_payloads(self) -> None:
        self.assertIsNone(self.subscription.initial.resourceVersions[0].entityChanges)
        self.write("a")
        self.publish()
        first = (await self.receive()).resourceVersions[0].entityChanges
        assert first is not None
        self.assertEqual((first.after, first.through), (0, 1))
        self.assertEqual(json.loads(first.documents[0].payload)["value"]["name"], "A")
        self.write("a", "B")
        self.publish()
        update = (await self.receive()).resourceVersions[0].entityChanges
        assert update is not None
        self.assertEqual((update.after, update.through), (1, 2))
        self.assertEqual(json.loads(update.documents[0].payload)["value"]["name"], "B")
        self.write("a", "private stale value", deleted=True)
        self.publish()
        deletion = (await self.receive()).resourceVersions[0].entityChanges
        assert deletion is not None
        self.assertTrue(deletion.documents[0].deleted)
        self.assertEqual(json.loads(deletion.documents[0].payload)["value"], {})
        reconnected = self.hub.subscribe([self.state], loop=asyncio.get_running_loop(), reconnect=True)
        self.assertIsNone(reconnected.initial.resourceVersions[0].entityChanges)
        reconnected.close()

    async def test_coalescing_replaces_same_identity_and_preserves_other_rows(self) -> None:
        second = self.hub.subscribe([self.state], loop=asyncio.get_running_loop())
        self.write("a")
        self.publish()
        self.assertIs(self.subscription._pending_entity_changes, second._pending_entity_changes)
        self.write("b", "B")
        self.publish()
        self.assertIs(self.subscription._pending_entity_changes, second._pending_entity_changes)
        self.write("a", "new A")
        self.publish()
        changes = (await self.receive()).resourceVersions[0].entityChanges
        assert changes is not None
        self.assertEqual((changes.after, changes.through), (0, 3))
        self.assertEqual([document.seq for document in changes.documents], [2, 3])
        self.assertEqual(json.loads(changes.documents[1].payload)["value"]["name"], "new A")
        second.close()

    async def test_pending_gap_reset_and_overflow_drop_data(self) -> None:
        self.hub.publish_entity_sequence(1, [1], entity_changes=batch(0, 1))
        self.hub.publish_entity_sequence(2, [2])
        self.hub.publish_entity_sequence(3, [3], entity_changes=batch(2, 3))
        self.assertIsNone((await self.receive()).resourceVersions[0].entityChanges)
        self.hub.publish_entity_sequence(4, [4], entity_changes=batch(2, 4))
        self.assertIsNone((await self.receive()).resourceVersions[0].entityChanges)
        self.hub.publish_entity_sequence(5, [5], entity_changes=batch(4, 5))
        self.hub.publish_entity_sequence(6, reset=True)
        reset = (await self.receive()).resourceVersions[0]
        self.assertTrue(reset.entitySequenceReset)
        self.assertIsNone(reset.entityChanges)
        self.hub.publish_entity_sequence(7, [7], entity_changes=batch(6, 7))
        self.hub.publish_overflow()
        self.assertIsNone((await self.receive()).resourceVersions[0].entityChanges)

    async def test_pending_coalescing_is_bounded_by_records_and_bytes(self) -> None:
        # Each publication fits. Their combined payload must fall back to a pull.
        self.hub.publish_entity_sequence(1, [1], entity_changes=batch(0, 1, "a", 140_000))
        self.hub.publish_entity_sequence(2, [2], entity_changes=batch(1, 2, "b", 140_000))
        self.assertIsNone((await self.receive()).resourceVersions[0].entityChanges)
        for sequence in range(3, MAX_ENTITY_CHANGE_DOCUMENTS + 4):
            self.hub.publish_entity_sequence(
                sequence, [sequence], entity_changes=batch(sequence - 1, sequence, str(sequence)),
            )
        version = (await self.receive()).resourceVersions[0]
        self.assertTrue(version.entitySequenceReset)
        self.assertIsNone(version.entityChanges)

    async def test_snapshot_ignores_a_concurrent_update_until_next_interval(self) -> None:
        self.write("a", "old")
        original = hub_module._read_entity_changes

        def concurrent_update(
            database: sqlite3.Connection, after: int, through: int,
            identities: list[tuple[int, int]],
        ) -> EntityChangeBatch | None:
            self.write("a", "new")
            return original(database, after, through, identities)

        with patch.object(hub_module, "_read_entity_changes", side_effect=concurrent_update):
            self.publish()
        first = (await self.receive()).resourceVersions[0].entityChanges
        assert first is not None
        self.assertEqual(first.through, 1)
        self.assertEqual(json.loads(first.documents[0].payload)["value"]["name"], "old")
        self.publish()
        second = (await self.receive()).resourceVersions[0].entityChanges
        assert second is not None
        self.assertEqual((second.after, second.through), (1, 2))
        self.assertEqual(json.loads(second.documents[0].payload)["value"]["name"], "new")

    async def test_pruned_tombstone_requires_reset_even_without_remaining_rows(self) -> None:
        self.write("a", deleted=True)
        with self.database:
            self.database.execute("DELETE FROM sync_entities")
            self.database.execute(
                "INSERT INTO sync_entity_meta(key,value) VALUES('entity_tombstone_floor','1')",
            )
        self.publish()
        version = (await self.receive()).resourceVersions[0]
        self.assertEqual(version.revision, 1)
        self.assertTrue(version.entitySequenceReset)
        self.assertIsNone(version.entityChanges)

    async def test_pruning_after_snapshot_still_delivers_tombstone(self) -> None:
        self.write("a", deleted=True)
        original = hub_module._read_entity_changes

        def concurrent_prune(
            database: sqlite3.Connection, after: int, through: int,
            identities: list[tuple[int, int]],
        ) -> EntityChangeBatch | None:
            with self.database:
                self.database.execute("DELETE FROM sync_entities")
                self.database.execute(
                    "INSERT INTO sync_entity_meta(key,value) VALUES('entity_tombstone_floor','1')",
                )
            return original(database, after, through, identities)

        with patch.object(hub_module, "_read_entity_changes", side_effect=concurrent_prune):
            self.publish()
        changes = (await self.receive()).resourceVersions[0].entityChanges
        assert changes is not None
        self.assertTrue(changes.documents[0].deleted)
        self.publish()
        self.assertIsNone(await self.subscription.next_event(timeout=0.02))

    async def test_large_payload_and_record_overflow_do_not_load_payloads(self) -> None:
        self.write("a")
        with self.database:
            self.database.execute("UPDATE sync_entities SET payload=?", ("x" * MAX_ENTITY_CHANGE_BYTES,))
        statements: list[str] = []
        original_connect = sqlite3.connect

        def trace_connection(
            database_uri: str, *, uri: bool, timeout: float,
        ) -> sqlite3.Connection:
            database = original_connect(database_uri, uri=uri, timeout=timeout)
            database.set_trace_callback(statements.append)
            return database

        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=trace_connection):
            self.publish()
        self.assertIsNone((await self.receive()).resourceVersions[0].entityChanges)
        self.assertFalse(any(statement.startswith("SELECT collection,id,seq,payload") for statement in statements))
        with self.database:
            self.database.executemany(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload) VALUES('project',?,?,?,?)",
                [(str(index), index + 2, "hash", "{}") for index in range(MAX_ENTITY_CHANGE_DOCUMENTS + 1)],
            )
        statements.clear()
        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=trace_connection):
            self.publish()
        version = (await self.receive()).resourceVersions[0]
        self.assertTrue(version.entitySequenceReset)
        self.assertIsNone(version.entityChanges)
        self.assertFalse(any(statement.startswith("SELECT collection,id,seq,payload") for statement in statements))
        self.assertFalse(any("LENGTH(CAST(payload AS BLOB))" in statement for statement in statements))

    async def test_size_work_stops_at_byte_budget_and_requires_a_state_reader(self) -> None:
        measured_sizes: list[int] = []
        statements: list[str] = []
        original_connect = sqlite3.connect

        class ProbeConnection(sqlite3.Connection):
            def execute(self, sql: str, parameters: Any = (), /) -> sqlite3.Cursor:
                return super().execute(
                    sql.replace("LENGTH(CAST(payload AS BLOB))", "probe_payload_bytes(payload)"),
                    parameters,
                )

        def measure_size(payload: str) -> int:
            size = len(payload.encode("utf-8"))
            measured_sizes.append(size)
            return size

        def probe_connection(
            database_uri: str, *, uri: bool, timeout: float,
        ) -> sqlite3.Connection:
            database = original_connect(database_uri, uri=uri, timeout=timeout, factory=ProbeConnection)
            database.create_function("probe_payload_bytes", 1, measure_size)
            database.set_trace_callback(statements.append)
            return database

        payload = "x" * (MAX_ENTITY_CHANGE_BYTES + 1)
        with self.database:
            self.database.executemany(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload) VALUES('project',?,?,?,?)",
                [(str(index), index + 1, "hash", payload) for index in range(32)],
            )
        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=probe_connection):
            self.publish()
        self.assertEqual(measured_sizes, [MAX_ENTITY_CHANGE_BYTES + 1])
        self.assertFalse(any(statement.startswith("SELECT collection,id,seq,payload") for statement in statements))
        self.assertIsNone((await self.receive()).resourceVersions[0].entityChanges)

        self.subscription.close()
        measured_sizes.clear()
        with self.database:
            self.database.execute("UPDATE sync_entities SET seq=seq+32")
        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=probe_connection):
            self.publish()
        self.assertEqual(measured_sizes, [])

        self.subscription = self.hub.subscribe([self.state], loop=asyncio.get_running_loop())
        # Each UTF-8 payload fits. The fourth crosses the cumulative byte budget.
        with self.database:
            self.database.execute("UPDATE sync_entities SET seq=seq+32,payload=?", ("ё" * 32_768,))
        measured_sizes.clear()
        statements.clear()
        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=probe_connection):
            self.publish()
        self.assertEqual(measured_sizes, [65_536] * 4)
        self.assertFalse(any(statement.startswith("SELECT collection,id,seq,payload") for statement in statements))
        self.assertIsNone((await self.receive()).resourceVersions[0].entityChanges)

    async def test_unreadable_row_and_database_failure_preserve_invalidation(self) -> None:
        self.write("a")
        with self.database:
            self.database.execute("UPDATE sync_entities SET payload='{' WHERE id='a'")
        self.publish()
        version = (await self.receive()).resourceVersions[0]
        self.assertEqual(version.entitySequences, [1])
        self.assertIsNone(version.entityChanges)
        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=sqlite3.OperationalError("locked")):
            self.publish(2, {2})
        version = (await self.receive()).resourceVersions[0]
        self.assertEqual(version.entitySequences, [2])
        self.assertIsNone(version.entityChanges)
        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=sqlite3.OperationalError("locked")):
            with self.assertRaises(sqlite3.OperationalError):
                self.publish()

    async def test_no_state_reader_skips_payload_work(self) -> None:
        self.subscription.close()
        self.write("a")
        with patch.object(hub_module, "_read_entity_changes") as read:
            self.publish()
        read.assert_not_called()
        self.assertEqual(self.hub._published_entity_sequence, 1)

    async def test_explicit_sequence_beyond_snapshot_cannot_claim_complete_data(self) -> None:
        self.write("a")
        self.publish(2, {1, 2})
        version = (await self.receive()).resourceVersions[0]
        self.assertEqual(version.revision, 2)
        self.assertIsNone(version.entityChanges)

    async def test_failed_scan_with_known_revision_still_requests_reconciliation(self) -> None:
        with patch("studio_api.sync.resources.hub.sqlite3.connect", side_effect=sqlite3.OperationalError("locked")):
            self.publish(1)
        version = (await self.receive()).resourceVersions[0]
        self.assertEqual(version.revision, 1)
        self.assertTrue(version.entitySequenceReset)
        self.assertIsNone(version.entityChanges)

    async def test_direct_payload_uses_http_rules_for_extras_mismatch_and_legacy_alias(self) -> None:
        payloads = [
            ("project", "a", '{"collection":"project","id":"a","value":{"id":"a","private":"omit"}}'),
            ("project", "b", '{"collection":"project","id":"b","value":{"id":"b","name":42}}'),
            ("agent", "chat", '{"collection":"agent","id":"chat","value":{"id":"chat","kind":"chat","name":"old"}}'),
        ]
        with self.database:
            self.database.executemany(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload) VALUES(?,?,?,'hash',?)",
                [(collection, identity, sequence, payload)
                 for sequence, (collection, identity, payload) in enumerate(payloads, 1)],
            )
        with self.assertLogs("studio_api.sync.router", level="WARNING"):
            self.publish()
        changes = (await self.receive()).resourceVersions[0].entityChanges
        assert changes is not None
        self.assertNotIn("private", changes.documents[0].payload)
        self.assertEqual(json.loads(changes.documents[1].payload)["value"]["name"], 42)
        self.assertTrue(changes.documents[2].deleted)


if __name__ == "__main__":
    unittest.main()
