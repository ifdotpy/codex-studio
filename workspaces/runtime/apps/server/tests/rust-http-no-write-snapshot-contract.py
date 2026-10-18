#!/usr/bin/env python3
"""Prove the shared HTTP no-write snapshot filters only named owner fields."""

from __future__ import annotations

from test_isolation import isolate_supervisor_environment

import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from contextlib import closing

isolate_supervisor_environment()

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from http_backend_harness import (
    BACKGROUND_OWNED_FIELDS,
    BACKGROUND_OWNED_ROW_JSON_FIELDS,
    no_write_snapshot,
)


class NoWriteSnapshotContract(unittest.TestCase):
    def test_agent_projection_sequence_ignores_only_scheduler_start_attempt(self) -> None:
        with tempfile.TemporaryDirectory(prefix="no-write-agent-projection-") as directory:
            state = Path(directory)
            database_path = state / "canvas.sqlite3"
            owned_field = "payload.value.startAttempt"
            self.assertIn(owned_field, BACKGROUND_OWNED_FIELDS["sync_entities"])
            payload = {
                "collection": "agent",
                "id": "agent-a",
                "value": {
                    "id": "agent-a",
                    "name": "worker",
                    "status": "paused",
                    "startAttempt": {"id": "attempt-before"},
                },
            }
            with closing(sqlite3.connect(database_path)) as database, database:
                database.execute(
                    "CREATE TABLE sync_entities ("
                    "collection TEXT, id TEXT, seq INTEGER, hash TEXT, "
                    "payload TEXT, deleted INTEGER)"
                )
                database.execute(
                    "INSERT INTO sync_entities VALUES(?,?,?,?,?,?)",
                    (
                        "agent",
                        "agent-a",
                        20,
                        "hash-before",
                        json.dumps(payload, sort_keys=True, separators=(",", ":")),
                        0,
                    ),
                )
            before = no_write_snapshot(state)

            payload["value"]["startAttempt"] = {"id": "attempt-after"}
            with closing(sqlite3.connect(database_path)) as database, database:
                database.execute(
                    "UPDATE sync_entities SET seq=?, hash=?, payload=? "
                    "WHERE collection='agent' AND id='agent-a'",
                    (
                        21,
                        "hash-after-start-attempt",
                        json.dumps(payload, sort_keys=True, separators=(",", ":")),
                    ),
                )
            self.assertEqual(
                no_write_snapshot(state, baseline_snapshot=before), before
            )

            payload["value"]["status"] = "running"
            with closing(sqlite3.connect(database_path)) as database, database:
                database.execute(
                    "UPDATE sync_entities SET seq=?, hash=?, payload=? "
                    "WHERE collection='agent' AND id='agent-a'",
                    (
                        22,
                        "hash-after-status",
                        json.dumps(payload, sort_keys=True, separators=(",", ":")),
                    ),
                )
            self.assertNotEqual(
                no_write_snapshot(state, baseline_snapshot=before), before
            )
            status_snapshot = no_write_snapshot(
                state, baseline_snapshot=before
            )

            payload["id"] = "agent-b"
            payload["value"]["id"] = "agent-b"
            with closing(sqlite3.connect(database_path)) as database, database:
                database.execute(
                    "UPDATE sync_entities SET id=?, seq=?, hash=?, payload=? "
                    "WHERE collection='agent' AND id='agent-a'",
                    (
                        "agent-b",
                        23,
                        "hash-after-identity",
                        json.dumps(payload, sort_keys=True, separators=(",", ":")),
                    ),
                )
            self.assertNotEqual(
                no_write_snapshot(state, baseline_snapshot=status_snapshot),
                status_snapshot,
            )

    def test_ignores_only_accepted_archive_tick_receipt_metadata(self) -> None:
        with tempfile.TemporaryDirectory(prefix="no-write-archive-receipt-") as directory:
            state = Path(directory)
            with closing(sqlite3.connect(state / "canvas.sqlite3")) as database, database:
                database.execute(
                    "CREATE TABLE runtime_operation_receipts (id TEXT, signature TEXT, result TEXT)"
                )
                fields = BACKGROUND_OWNED_ROW_JSON_FIELDS[
                    "runtime_operation_receipts"
                ]["work-archive:"]["result"]
                result = {field: "before" for field in fields} | {"status": "pending"}
                database.execute(
                    "INSERT INTO runtime_operation_receipts VALUES(?,?,?)",
                    ("work-archive:task:result", "accepted-archive-v1", json.dumps(result)),
                )
            before = no_write_snapshot(state)
            with closing(sqlite3.connect(state / "canvas.sqlite3")) as database, database:
                row = database.execute(
                    "SELECT result FROM runtime_operation_receipts WHERE id=?",
                    ("work-archive:task:result",),
                ).fetchone()
                result = json.loads(row[0])
                for field in fields:
                    result[field] = "after"
                database.execute(
                    "UPDATE runtime_operation_receipts SET result=? WHERE id=?",
                    (json.dumps(result), "work-archive:task:result"),
                )
            self.assertEqual(no_write_snapshot(state), before)

            with closing(sqlite3.connect(state / "canvas.sqlite3")) as database, database:
                result["status"] = "completed"
                database.execute(
                    "UPDATE runtime_operation_receipts SET result=? WHERE id=?",
                    (json.dumps(result), "work-archive:task:result"),
                )
            self.assertNotEqual(no_write_snapshot(state), before)

    def test_ignores_each_background_field_but_detects_business_changes(self) -> None:
        with tempfile.TemporaryDirectory(prefix="no-write-snapshot-") as directory:
            state = Path(directory)
            database_path = state / "canvas.sqlite3"
            with closing(sqlite3.connect(database_path)) as database, database:
                for table, fields in BACKGROUND_OWNED_FIELDS.items():
                    columns = sorted({field for field in fields if "." not in field})
                    schema = ["id TEXT", "record TEXT", "payload TEXT", "business TEXT"] + [
                        f'"{column}" TEXT' for column in columns
                    ]
                    database.execute(f'CREATE TABLE "{table}" ({", ".join(schema)})')
                    record: dict[str, object] = {"business": "unchanged"}
                    payload: dict[str, object] = {"value": {"business": "unchanged"}}
                    for field in fields:
                        if "." not in field:
                            continue
                        if field.startswith("payload."):
                            parts = field[len("payload."):].split(".")
                            current = payload
                        else:
                            parts = field.removeprefix("record.").split(".")
                            current = record
                        for part in parts[:-1]:
                            current = current.setdefault(part, {})  # type: ignore[assignment]
                        is_parent = any(
                            other.startswith(f"{field}.") for other in fields
                        )
                        if is_parent:
                            current.setdefault(parts[-1], {})
                        else:
                            current[parts[-1]] = "before"
                    values = [table, json.dumps(record), json.dumps(payload), "unchanged"] + ["before"] * len(columns)
                    database.execute(
                        f'INSERT INTO "{table}" VALUES ({", ".join("?" for _ in values)})',
                        values,
                    )
            with closing(sqlite3.connect(state / "verification.sqlite3")) as database, database:
                database.execute("CREATE TABLE analytics_items (id TEXT, item TEXT)")
                database.execute(
                    "INSERT INTO analytics_items VALUES('item', 'unchanged')"
                )
            before = no_write_snapshot(state)
            with closing(sqlite3.connect(database_path)) as database, database:
                for table, fields in BACKGROUND_OWNED_FIELDS.items():
                    row = database.execute(f'SELECT record,payload FROM "{table}"').fetchone()
                    record = json.loads(row[0])
                    payload = json.loads(row[1])
                    for field in fields:
                        if "." not in field:
                            database.execute(
                                f'UPDATE "{table}" SET "{field}"=?', ("after",)
                            )
                            continue
                        if any(other.startswith(f"{field}.") for other in fields):
                            continue
                        if field.startswith("payload."):
                            parts = field[len("payload."):].split(".")
                            current = payload
                        else:
                            parts = field.removeprefix("record.").split(".")
                            current = record
                        for part in parts[:-1]:
                            current = current[part]
                        current[parts[-1]] = "after"
                    database.execute(
                        f'UPDATE "{table}" SET record=?,payload=?', (json.dumps(record), json.dumps(payload))
                    )
            self.assertEqual(no_write_snapshot(state), before)

            lazy_schema = state / "verification.sqlite3"
            with closing(sqlite3.connect(lazy_schema)) as database, database:
                database.execute("CREATE TABLE runtime_execution_effects (id TEXT, record TEXT)")
            self.assertEqual(no_write_snapshot(state), before)

            with closing(sqlite3.connect(lazy_schema)) as database, database:
                database.execute(
                    "INSERT INTO runtime_execution_effects VALUES('effect', 'new row')"
                )
            self.assertNotEqual(no_write_snapshot(state), before)
            with_row = no_write_snapshot(state)

            with closing(sqlite3.connect(state / "verification.sqlite3")) as database, database:
                database.execute("UPDATE analytics_items SET item='changed'")
            self.assertNotEqual(no_write_snapshot(state), with_row)
            with_analytics_change = no_write_snapshot(state)

            with closing(sqlite3.connect(state / "analytics.sqlite3")) as database, database:
                database.execute("CREATE TABLE analytics_items (id TEXT, item TEXT)")
                database.execute("INSERT INTO analytics_items VALUES('collector', 'before')")
            analytics_baseline = no_write_snapshot(state)
            with closing(sqlite3.connect(state / "analytics.sqlite3")) as database, database:
                database.execute("UPDATE analytics_items SET item='after'")
            self.assertEqual(no_write_snapshot(state), analytics_baseline)

            with closing(sqlite3.connect(state / "analytics-copy.sqlite3")) as database, database:
                database.execute("CREATE TABLE analytics_items (id TEXT, item TEXT)")
                database.execute("INSERT INTO analytics_items VALUES('copy', 'before')")
            other_analytics_baseline = no_write_snapshot(state)
            with closing(sqlite3.connect(state / "analytics-copy.sqlite3")) as database, database:
                database.execute("UPDATE analytics_items SET item='after'")
            self.assertNotEqual(no_write_snapshot(state), other_analytics_baseline)

            with closing(sqlite3.connect(database_path)) as database, database:
                database.execute(
                    f'UPDATE "{next(iter(BACKGROUND_OWNED_FIELDS))}" '
                    "SET business='changed'"
                )
            self.assertNotEqual(no_write_snapshot(state), with_analytics_change)


if __name__ == "__main__":
    unittest.main()
