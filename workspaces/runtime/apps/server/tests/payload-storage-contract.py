#!/usr/bin/env python3
"""Synthetic contract checks for blob storage, reference safety and retention."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))

from codex_payload_migrate import migrate_batch
import codex_payload_migrate
from codex_payloads import (
    EXTERNALIZE_THRESHOLD,
    TASK_PREVIEW_BYTES,
    collect_unreferenced,
    ensure_payload_schema,
    externalize_record,
    externalize_result,
    load_bytes,
    release_db_writer_lock,
    resolve_record,
    resolve_result,
    store_bytes,
)
from studio_api.sync.resources.models import TaskResource, TasksResource
from studio_api.sync.resources.relay.client import NotifyCommittedWriteError


class PayloadStorageContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.db = sqlite3.connect(self.root / "canvas.sqlite3", timeout=5)
        self.db.row_factory = sqlite3.Row
        ensure_payload_schema(self.db)
        self.db.execute("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
        self.db.execute("CREATE INDEX runtime_task_agent_created_id ON runtime_tasks "
                        "(json_extract(record,'$.agent'),json_extract(record,'$.created') DESC,id DESC)")
        self.db.execute("CREATE TABLE runtime_payload_migrations "
                        "(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,"
                        "status TEXT NOT NULL DEFAULT 'pending',updated REAL,error TEXT)")
        self.db.execute("CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY,value TEXT NOT NULL)")
        self.db.commit()

    def tearDown(self):
        release_db_writer_lock(self.db)
        self.db.close()
        self.temp.cleanup()

    def test_checkpoint_and_tool_result_round_trip_and_reference_index(self):
        checkpoint = {"id": "cp", "items": ["history-entry-" + "x" * 1024 for _ in range(90)]}
        stored = externalize_record(self.root, self.db, "checkpoints", checkpoint)
        self.db.execute("INSERT INTO runtime_checkpoints VALUES (?,?)", ("cp", json.dumps(stored)))
        self.db.commit()
        release_db_writer_lock(self.db)
        raw = self.db.execute("SELECT record FROM runtime_checkpoints WHERE id='cp'").fetchone()[0]
        self.assertLess(len(raw.encode()), len(json.dumps(checkpoint).encode()) // 10)
        self.assertEqual(resolve_record(self.root, json.loads(raw)), checkpoint)
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_payload_refs WHERE sha256=?",
                                         (stored["_payloadBlobs"]["items"]["sha256"],)).fetchone()[0], 1)

        result = {"success": True, "contentItems": [{"type": "inputText", "text": "z" * 1024} for _ in range(100)]}
        stored_result = externalize_result(self.root, self.db, result)
        self.db.execute("INSERT INTO runtime_tool_results VALUES (?,?)", ("tool", json.dumps(stored_result)))
        self.db.commit()
        release_db_writer_lock(self.db)
        encoded = self.db.execute("SELECT result FROM runtime_tool_results WHERE id='tool'").fetchone()[0]
        self.assertEqual(resolve_result(self.root, encoded), result)
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_payload_refs WHERE table_name='runtime_tool_results'").fetchone()[0], 1)
        self.assertLess(len(json.dumps(stored_result["contentItems"], ensure_ascii=False).encode()), 4096)
        self.assertGreater(EXTERNALIZE_THRESHOLD, 0)

    def test_migration_is_resumable_and_retains_only_old_nonrecent_tasks(self):
        now = 1_800_000_000
        for i in range(130):
            record = {"id": f"a:{i:03}", "agent": "a", "created": now - 10 * 86400 + i,
                      "status": "completed", "tail": "output-" + "q" * 2048}
            self.db.execute("INSERT INTO runtime_tasks VALUES (?,?)", (record["id"], json.dumps(record)))
        self.db.commit()
        pages = 0
        while True:
            result = migrate_batch(self.root, self.db, "tasks", batch_rows=13, now=now)
            pages += 1
            if result["done"]:
                break
        rows = {row["id"]: json.loads(row["record"]) for row in
                self.db.execute("SELECT id,record FROM runtime_tasks")}
        self.assertEqual(pages, 11)
        self.assertEqual(sum(len(item["tail"].encode()) <= TASK_PREVIEW_BYTES for item in rows.values()), 30)
        self.assertEqual(sum(item.get("outputPreviewOnly", False) for item in rows.values()), 30)
        self.assertEqual(rows["a:129"]["tail"], "output-" + "q" * 2048)
        saved = migrate_batch(self.root, self.db, "tasks", batch_rows=13, now=now)
        self.assertTrue(saved["done"])
        self.assertEqual(saved["rows"], 0)

    def test_task_retention_notifies_only_after_committed_entity_changes(self):
        now = 1_800_000_000
        for i in range(101):
            record = {"id": f"a:{i:03}", "agent": "agent-a", "created": now - 10 * 86400 + i,
                      "status": "completed", "tail": "output-" + "q" * 2048}
            self.db.execute("INSERT INTO runtime_tasks VALUES (?,?)", (record["id"], json.dumps(record)))
        self.db.commit()
        notified: list[tuple[str, list[object]]] = []

        def notify(request_id, resources):
            self.assertFalse(self.db.in_transaction)
            row = self.db.execute("SELECT record FROM runtime_tasks WHERE id='a:000'").fetchone()
            self.assertTrue(json.loads(row["record"]).get("outputPreviewOnly"))
            notified.append((request_id, resources))

        result = migrate_batch(self.root, self.db, "tasks", batch_rows=101, now=now, notify=notify)
        self.assertFalse(result["done"])
        self.assertTrue(migrate_batch(self.root, self.db, "tasks", batch_rows=101, now=now)["done"])
        self.assertEqual(len(notified), 1)
        self.assertIsInstance(notified[0][1][0].root, TasksResource)
        self.assertIsInstance(notified[0][1][1].root, TaskResource)
        self.assertEqual(notified[0][1][0].root.agentId, "agent-a")
        self.assertEqual(notified[0][1][1].root.taskId, "a:000")

    def test_failed_task_notification_reports_committed_batch_without_reapplying_it(self):
        now = 1_800_000_000
        for i in range(101):
            record = {"id": f"a:{i:03}", "agent": "agent-a", "created": now - 10 * 86400 + i,
                      "status": "completed", "tail": "output-" + "q" * 2048}
            self.db.execute("INSERT INTO runtime_tasks VALUES (?,?)", (record["id"], json.dumps(record)))
        self.db.commit()

        def failed_notify(_request_id, _resources):
            raise NotifyCommittedWriteError("server unavailable")

        with self.assertRaisesRegex(NotifyCommittedWriteError, "Payload batch committed"):
            migrate_batch(self.root, self.db, "tasks", batch_rows=101, now=now, notify=failed_notify)
        migrated = json.loads(self.db.execute(
            "SELECT record FROM runtime_tasks WHERE id='a:000'").fetchone()["record"])
        self.assertTrue(migrated.get("outputPreviewOnly"))
        resumed = migrate_batch(self.root, self.db, "tasks", batch_rows=101, now=now)
        self.assertTrue(resumed["done"])
        self.assertEqual(resumed["rows"], 0)

    def test_gc_preserves_references_and_waits_for_grace(self):
        ref = store_bytes(self.root, b"referenced", db=self.db)
        record = {"id": "cp", "items": [], "_payloadBlobs": {"items": ref}}
        self.db.execute("INSERT INTO runtime_checkpoints VALUES (?,?)", ("cp", json.dumps(record)))
        old = store_bytes(self.root, b"orphan")
        old_path = self.root / "blobs" / old["sha256"][:2] / old["sha256"][2:4] / old["sha256"]
        os.utime(old_path, (1, 1))
        self.db.commit()
        release_db_writer_lock(self.db)
        result = collect_unreferenced(self.root, self.db, now=time.time())
        self.assertEqual(result, {"deleted": 1, "bytes": len(b"orphan")})
        self.assertTrue((self.root / "blobs" / ref["sha256"][:2] / ref["sha256"][2:4] / ref["sha256"]).exists())
        self.assertEqual(load_bytes(self.root, ref), b"referenced")

    def test_low_space_persists_waiting_status_without_changing_payload_rows(self):
        record = {"id": "cp", "items": ["large-" + "x" * 1000]}
        self.db.execute("INSERT INTO runtime_checkpoints VALUES (?,?)", ("cp", json.dumps(record)))
        self.db.commit()

        class Usage:
            free = codex_payload_migrate.FREE_SPACE_RESERVE - 1

        with patch.object(codex_payload_migrate.shutil, "disk_usage", return_value=Usage()):
            result = migrate_batch(self.root, self.db, "checkpoints")
        self.assertFalse(result["done"])
        self.assertTrue(result["waitingForSpace"])
        self.assertEqual(json.loads(self.db.execute(
            "SELECT record FROM runtime_checkpoints WHERE id='cp'").fetchone()[0]), record)
        status = self.db.execute("SELECT status,cursor FROM runtime_payload_migrations "
                                 "WHERE name='payload-v1:checkpoints'").fetchone()
        self.assertEqual(tuple(status), ("waitingForSpace", 0))


if __name__ == "__main__":
    unittest.main(verbosity=2)
