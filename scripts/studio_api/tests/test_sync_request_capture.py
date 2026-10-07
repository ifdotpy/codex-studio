"""Request mutation watermarks are captured at the entity writer boundary."""
from __future__ import annotations

import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from codex_sync_entities import (
    begin_sync_request_capture,
    end_sync_request_capture,
    put,
    sync_request_checkpoint,
)


class SyncRequestCaptureTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = Path(self.tmp.name) / "sync.sqlite3"
        with sqlite3.connect(self.path) as db:
            db.executescript(
                """
                CREATE TABLE sync_entities (
                    collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
                    hash TEXT NOT NULL, payload TEXT, deleted INTEGER NOT NULL DEFAULT 0,
                    PRIMARY KEY(collection,id)
                );
                CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                CREATE TABLE sync_documents (seq INTEGER PRIMARY KEY);
                CREATE TABLE sync_versions (seq INTEGER PRIMARY KEY);
                INSERT INTO sync_entities VALUES ('workspace','current',1,'h','{}',0);
                INSERT INTO sync_entity_meta VALUES ('entity_tombstone_floor','0');
                """
            )
        self.writer = sqlite3.connect(self.path, isolation_level=None)
        self.other = sqlite3.connect(self.path, isolation_level=None)
        self.addCleanup(self.writer.close)
        self.addCleanup(self.other.close)

    def write_entity(self, db: sqlite3.Connection, key: str) -> None:
        with patch("codex_sync_entities.project", return_value={"id": key}):
            put(db, "workspace", key, {"id": key})

    def test_request_writer_states_the_previous_non_transcript_high(self) -> None:
        token = begin_sync_request_capture(1)
        try:
            self.writer.execute("BEGIN IMMEDIATE")
            self.write_entity(self.writer, "request")
            self.assertEqual(sync_request_checkpoint(), 1)
            self.writer.commit()
        finally:
            end_sync_request_capture(token)

    def test_interleaved_connection_commit_makes_the_watermark_unknown(self) -> None:
        token = begin_sync_request_capture(1)
        try:
            self.other.execute("BEGIN IMMEDIATE")
            self.other.execute(
                "INSERT INTO sync_entities VALUES ('workspace','other',2,'h','{}',0)"
            )
            self.other.commit()
            self.writer.execute("BEGIN IMMEDIATE")
            self.write_entity(self.writer, "request")
            self.assertIsNone(sync_request_checkpoint())
            self.writer.commit()
        finally:
            end_sync_request_capture(token)

    def test_transcript_commit_does_not_change_the_entity_scope_high(self) -> None:
        token = begin_sync_request_capture(1)
        try:
            self.other.execute("BEGIN IMMEDIATE")
            self.other.execute(
                "INSERT INTO sync_entities VALUES ('transcript:agent','item',2,'h','{}',0)"
            )
            self.other.commit()
            self.writer.execute("BEGIN IMMEDIATE")
            self.write_entity(self.writer, "request")
            self.assertEqual(sync_request_checkpoint(), 1)
            self.writer.commit()
        finally:
            end_sync_request_capture(token)

    def test_request_without_entity_rows_has_no_checkpoint(self) -> None:
        token = begin_sync_request_capture(1)
        try:
            self.assertIsNone(sync_request_checkpoint())
        finally:
            end_sync_request_capture(token)


if __name__ == "__main__":
    unittest.main()
