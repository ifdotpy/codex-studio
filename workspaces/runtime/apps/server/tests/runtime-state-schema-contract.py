#!/usr/bin/env python3
"""Guard the fresh runtime schema and prove a plain SQLite reader can open it."""

from __future__ import annotations

import difflib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import unittest

SERVER_SOURCE = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SERVER_SOURCE))

from codex_runtime import Runtime  # noqa: E402


SNAPSHOT = Path(__file__).with_name("fixtures") / "runtime-state-schema.json"


class QuietRuntime(Runtime):
    """Create the regular runtime schema without starting its scheduler loop."""

    def schedule(self) -> None:
        return


def schema_snapshot(runtime: Runtime) -> dict[str, object]:
    def snapshot_database(db: sqlite3.Connection, schema: str) -> dict[str, object]:
        objects = [
            {"type": kind, "name": name, "table": table, "sql": sql.strip() if sql else None}
            for kind, name, table, sql in db.execute(
                f"SELECT type,name,tbl_name,sql FROM {schema}.sqlite_master "
                "WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name"
            )
        ]
        tables = []
        for _kind, name in db.execute(
            f"SELECT type,name FROM {schema}.sqlite_master "
            "WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY name"
        ):
            quoted = '"' + name.replace('"', '""') + '"'
            columns = [
                {"cid": cid, "name": column, "type": declaration, "notnull": required,
                 "default": default, "primary_key_position": primary_key}
                for cid, column, declaration, required, default, primary_key
                in db.execute(f"PRAGMA {schema}.table_info({quoted})")
            ]
            tables.append({"name": name, "columns": columns})
        user_version = db.execute(f"PRAGMA {schema}.user_version").fetchone()[0]
        migration_table = db.execute(
            f"SELECT 1 FROM {schema}.sqlite_master WHERE type='table' AND name='runtime_payload_migrations'"
        ).fetchone()
        payload_migrations = []
        if migration_table:
            payload_migrations = [
                {"name": name, "cursor": cursor, "complete": complete, "status": status}
                for name, cursor, complete, status in db.execute(
                    f"SELECT name,cursor,complete,status FROM {schema}.runtime_payload_migrations ORDER BY name"
                )
            ]
        migration_table = db.execute(
            f"SELECT 1 FROM {schema}.sqlite_master WHERE type='table' AND name='analytics_meta'"
        ).fetchone()
        file_migration = []
        if migration_table:
            row = db.execute(
                f"SELECT value FROM {schema}.analytics_meta WHERE key='fileMigrationV1'"
            ).fetchone()
            if row:
                file_migration = [json.loads(row[0])]
        return {
            "pragma_user_version": user_version,
            "payload_migrations": payload_migrations,
            "file_migration_v1": file_migration,
            "tables_and_columns": tables,
            "sqlite_master": objects,
        }

    with runtime.db(analytics=True) as db:
        return {
            "canvas.sqlite3": snapshot_database(db, "main"),
            "analytics.sqlite3": snapshot_database(db, "analytics"),
            "pragmas": {
                schema: {
                    "journal_mode": db.execute(f"PRAGMA {schema}.journal_mode").fetchone()[0],
                    "synchronous": db.execute(f"PRAGMA {schema}.synchronous").fetchone()[0],
                }
                for schema in ("main", "analytics")
            },
        }


class RuntimeStateSchemaContract(unittest.TestCase):
    def setUp(self) -> None:
        temporary_root = os.environ.get("RUNTIME_STATE_FORMAT_TMPDIR")
        if temporary_root:
            Path(temporary_root).mkdir(parents=True, exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(
            prefix="runtime-state-format-", dir=temporary_root
        )
        self.root = Path(self.temporary.name)
        self.runtime = QuietRuntime(self.root, lambda *_args, **_kwargs: None)

    def tearDown(self) -> None:
        self.runtime.close()
        self.temporary.cleanup()

    def test_fresh_runtime_schema_matches_checked_snapshot(self) -> None:
        actual = schema_snapshot(self.runtime)
        encoded = json.dumps(actual, indent=2, sort_keys=True) + "\n"
        if os.environ.get("UPDATE_RUNTIME_STATE_SCHEMA_SNAPSHOT") == "1":
            SNAPSHOT.write_text(encoded, encoding="utf-8")
            return

        expected = SNAPSHOT.read_text(encoding="utf-8") if SNAPSHOT.exists() else ""
        if encoded != expected:
            diff = "".join(difflib.unified_diff(
                expected.splitlines(keepends=True), encoded.splitlines(keepends=True),
                fromfile=str(SNAPSHOT), tofile="fresh runtime schema",
            ))
            self.fail(
                "Fresh runtime SQLite schema changed. Review the diff; if intentional, "
                "run `UPDATE_RUNTIME_STATE_SCHEMA_SNAPSHOT=1 just test server "
                "workspaces/runtime/apps/server/tests/runtime-state-schema-contract.py`.\n"
                + diff
            )

    def test_independent_sqlite_reader_opens_server_written_state_in_place(self) -> None:
        agent = self.runtime.create({
            "name": "State format proof",
            "cwd": str(self.root),
            "prompt": "Disposable record",
        }, defer=True)
        task = {
            "id": "state-format-task",
            "agent": agent["id"],
            "status": "accepted",
            "created": 1720000000.25,
            "fixtureFlag": True,
        }
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "tasks", task)
            self.runtime.enqueue(db, agent, "work_review", json.dumps({"fixture": True}),
                                 key="state-format-event")

        # This process imports only Python's standard-library sqlite3/json.
        reader = r'''import json, sqlite3, sys
db = sqlite3.connect(sys.argv[1])
agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (sys.argv[2],)).fetchone()[0])
task = json.loads(db.execute("SELECT record FROM runtime_tasks WHERE id='state-format-task'").fetchone()[0])
event = db.execute("SELECT kind,text,status,created,epoch FROM runtime_events WHERE id='state-format-event'").fetchone()
print(json.dumps({"agent": agent, "task": task,
                  "event": {"kind": event[0], "text": json.loads(event[1]),
                            "status": event[2], "created": event[3], "epoch": event[4]}},
                 sort_keys=True))
db.close()
'''
        completed = subprocess.run(
            [sys.executable, "-c", reader, str(self.runtime.db_path), agent["id"]],
            check=True, capture_output=True, text=True,
        )
        result = json.loads(completed.stdout)
        self.assertEqual(result["agent"]["name"], "State format proof")
        self.assertIs(result["task"]["fixtureFlag"], True)
        self.assertEqual(result["event"]["text"], {"fixture": True})
        self.assertEqual(result["event"]["kind"], "work_review")
        self.assertEqual(result["event"]["status"], "cancelled")
        self.assertIsInstance(result["event"]["created"], float)
        self.assertIsInstance(result["event"]["epoch"], int)


if __name__ == "__main__":
    unittest.main()
