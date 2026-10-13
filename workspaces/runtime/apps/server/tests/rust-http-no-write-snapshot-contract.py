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
from http_backend_harness import BACKGROUND_OWNED_FIELDS, no_write_snapshot


class NoWriteSnapshotContract(unittest.TestCase):
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
                        if field.startswith("payload."):
                            parts = field[len("payload."):].split(".")
                            current = payload
                        else:
                            parts = field.removeprefix("record.").split(".")
                            current = record
                        for part in parts[:-1]:
                            current = current.setdefault(part, {})  # type: ignore[assignment]
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
                        if field.startswith("payload."):
                            parts = field[len("payload."):].split(".")
                            current = payload
                        else:
                            parts = field.removeprefix("record.").split(".")
                            current = record
                        for part in parts[:-1]:
                            current = current[part]
                        current[parts[-1]] = "after"
                        if "." not in field:
                            database.execute(
                                f'UPDATE "{table}" SET "{field}"=?', ("after",)
                            )
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
