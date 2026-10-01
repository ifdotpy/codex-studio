import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

from transcript_storage.storage import backfill_addresses, backfill_items, backfill_partials, body, drain, ensure_indexed, has_partial, has_pending, index_item, initialize, persist, remove


class TranscriptStorageTests(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        self.db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
        initialize(self.db)

    def tearDown(self):
        self.db.close()

    def item(self, identity, text, *, streaming=False, now=1):
        record = {"id": identity, "title": "assistant", "text": text[:20], "truncated": len(text) > 20}
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                        (identity, "agent", json.dumps(record), now))
        persist(self.db, identity, "agent", "assistant", text, streaming=streaming, now=now)

    def test_body_is_durable_before_search_index_and_latest_version_wins(self):
        self.item("a:1", "first complete body", streaming=True, now=10)
        self.item("a:1", "final complete body", streaming=True, now=11.9)
        self.item("a:1", "latest streamed body", streaming=True, now=13)
        self.assertEqual(body(self.db, "a:1"), "latest streamed body")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search").fetchone()[0], 0)
        self.assertEqual(drain(self.db, now=11), 0)
        self.assertEqual(drain(self.db, now=12), 1)
        self.assertEqual(self.db.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH 'latest'").fetchone()[0],
                         "latest streamed body")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_pending").fetchone()[0], 0)

    def test_final_native_body_replaces_stream_prefix_and_indexes_now(self):
        self.item("a:final-replace", "stream prefix", streaming=True, now=20)
        self.item("a:final-replace", "authoritative completed body", now=21)
        self.assertEqual(body(self.db, "a:final-replace"), "authoritative completed body")
        self.assertEqual(self.db.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH 'authoritative'").fetchone()[0],
                         "authoritative completed body")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_pending").fetchone()[0], 0)

    def test_final_body_is_indexed_synchronously(self):
        self.item("a:final", "immediately searchable")
        self.assertEqual(self.db.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH 'searchable'").fetchone()[0],
                         "immediately searchable")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_pending").fetchone()[0], 0)

    def test_ensure_indexed_uses_address_metadata_without_duplicate_rows(self):
        self.item("a:ensure", "already indexed")
        self.assertFalse(ensure_indexed(self.db, "a:ensure", "agent", "assistant", "wrong duplicate"))
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search WHERE id='a:ensure'").fetchone()[0], 1)

    def test_restart_uses_committed_body_and_pending_work(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "restart.sqlite3"
            db = sqlite3.connect(path)
            db.row_factory = sqlite3.Row
            db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
            initialize(db)
            text = "long " + "x" * 50
            db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("a:2", "agent", json.dumps({"title": "assistant"}), 20))
            persist(db, "a:2", "agent", "assistant", text, streaming=True, now=20)
            db.commit()
            db.close()
            reopened = sqlite3.connect(path)
            reopened.row_factory = sqlite3.Row
            initialize(reopened)
            self.assertEqual(body(reopened, "a:2"), text)
            self.assertEqual(drain(reopened, now=22), 1)
            self.assertEqual(body(reopened, "a:2"), text)
            reopened.close()

    def test_legacy_fts_remains_readable_without_a_body_row(self):
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                        ("old:1", "old", json.dumps({"text": "excerpt"}), 1))
        index_item(self.db, "old:1", "old", "assistant", "legacy full body")
        self.assertEqual(body(self.db, "old:1", agent="old"), "legacy full body")

    def test_body_only_legacy_fts_uses_the_existing_row_address(self):
        legacy = sqlite3.connect(":memory:")
        legacy.row_factory = sqlite3.Row
        legacy.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
        legacy.execute("CREATE VIRTUAL TABLE runtime_search USING fts5(body)")
        legacy.execute("CREATE TABLE runtime_search_rows(id TEXT PRIMARY KEY,search_rowid INTEGER NOT NULL UNIQUE)")
        legacy.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("old:body-only", "old", json.dumps({}), 1))
        legacy.execute("INSERT INTO runtime_search(body) VALUES (?)", ("body-only legacy text",))
        legacy.execute("INSERT INTO runtime_search_rows VALUES (?,?)", ("old:body-only", 1))
        initialize(legacy)
        self.assertEqual(body(legacy, "old:body-only", agent="old"), "body-only legacy text")
        legacy.close()

    def test_initialize_does_not_scan_or_rewrite_existing_legacy_history(self):
        legacy = sqlite3.connect(":memory:")
        legacy.row_factory = sqlite3.Row
        legacy.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
        legacy.execute("CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body)")
        legacy.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("old:1", "old", json.dumps({"text": "excerpt"}), 1))
        legacy.execute("INSERT INTO runtime_search VALUES (?,?,?,?)", ("old:1", "old", "assistant", "legacy full body"))
        initialize(legacy)
        self.assertEqual(legacy.execute("SELECT count(*) FROM runtime_search_rows").fetchone()[0], 0)
        self.assertEqual(legacy.execute("SELECT count(*) FROM runtime_item_bodies").fetchone()[0], 0)
        self.assertEqual(legacy.execute("SELECT count(*) FROM runtime_search_pending").fetchone()[0], 0)
        self.assertEqual(legacy.execute("SELECT done FROM runtime_search_address_cursor").fetchone()[0], 0)
        self.assertEqual(tuple(legacy.execute("SELECT rowid,done FROM runtime_search_partial_cursor").fetchone()), (0, 0))
        self.assertEqual(backfill_addresses(legacy, limit=32), 1)
        self.assertEqual(body(legacy, "old:1", agent="old"), "legacy full body")
        legacy.close()

    def test_unmapped_legacy_body_is_readable_before_bounded_migration(self):
        legacy = sqlite3.connect(":memory:")
        legacy.row_factory = sqlite3.Row
        legacy.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
        legacy.execute("CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body)")
        legacy.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                       ("old:unmapped", "old", json.dumps({"text": "excerpt", "truncated": True}), 1))
        legacy.execute("INSERT INTO runtime_search VALUES (?,?,?,?)",
                       ("old:unmapped", "old", "assistant", "legacy full body"))
        initialize(legacy)
        self.assertEqual(body(legacy, "old:unmapped", agent="old"), "legacy full body")
        legacy.close()

    def test_existing_address_map_skips_legacy_migration(self):
        legacy = sqlite3.connect(":memory:")
        legacy.executescript("""
        CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body);
        CREATE TABLE runtime_search_rows(id TEXT PRIMARY KEY,search_rowid INTEGER NOT NULL UNIQUE);
        """)
        initialize(legacy)
        self.assertEqual(legacy.execute("SELECT done FROM runtime_search_address_cursor").fetchone()[0], 1)
        self.assertEqual(backfill_addresses(legacy), 0)
        legacy.close()

    def test_missing_legacy_index_rows_backfill_in_bounded_pages(self):
        for index in range(3):
            self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                            (f"old:{index}", "agent", json.dumps({"title": "assistant", "text": f"legacy{index}"}), index))
        self.assertEqual(self.db.execute("SELECT done FROM runtime_search_item_cursor").fetchone()[0], 0)
        self.assertEqual(backfill_addresses(self.db), 0)
        self.assertEqual(backfill_items(self.db, limit=2), 2)
        self.assertTrue(has_pending(self.db, ["agent"]))
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_indexed").fetchone()[0], 2)
        self.assertEqual(backfill_items(self.db, limit=2), 1)
        self.assertEqual(backfill_partials(self.db), 3)
        self.assertFalse(has_pending(self.db, ["agent"]))
        self.assertEqual(self.db.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH 'legacy2'").fetchone()[0], "legacy2")

    def test_item_migration_cursor_resumes_after_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "legacy.sqlite3"
            db = sqlite3.connect(path)
            db.row_factory = sqlite3.Row
            db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
            initialize(db)
            for index in range(3):
                db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                           (f"legacy:{index}", "agent", json.dumps({"text": f"body{index}"}), index))
            backfill_addresses(db)
            self.assertEqual(backfill_items(db, limit=2), 2)
            db.commit()
            db.close()
            reopened = sqlite3.connect(path)
            reopened.row_factory = sqlite3.Row
            initialize(reopened)
            self.assertEqual(backfill_items(reopened, limit=2), 1)
            self.assertEqual(reopened.execute("SELECT done FROM runtime_search_item_cursor").fetchone()[0], 1)
            self.assertEqual(reopened.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH 'body2'").fetchone()[0], "body2")
            reopened.close()

    def test_missing_truncated_legacy_body_is_marked_partial_not_promoted(self):
        record = {"title": "assistant", "text": "excerpt", "truncated": True}
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("old:partial", "agent", json.dumps(record), 1))
        backfill_addresses(self.db)
        backfill_items(self.db)
        self.assertIsNone(body(self.db, "old:partial", agent="agent"))
        self.assertTrue(has_partial(self.db, ["agent"]))
        self.assertFalse(has_partial(self.db, ["other-agent"]))
        self.db.execute("UPDATE runtime_items SET record=? WHERE id=?",
                        (json.dumps({"title": "assistant", "text": "complete"}), "old:partial"))
        persist(self.db, "old:partial", "agent", "assistant", "recovered complete body")
        self.assertFalse(has_partial(self.db, ["agent"]))

    def test_remove_clears_partial_provenance(self):
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                        ("old:partial", "agent", json.dumps({"text": "excerpt", "truncated": True}), 1))
        backfill_addresses(self.db)
        backfill_items(self.db)
        self.assertTrue(has_partial(self.db, ["agent"]))
        remove(self.db, "old:partial")
        self.db.execute("DELETE FROM runtime_items WHERE id='old:partial'")
        self.assertFalse(has_partial(self.db, ["agent"]))

    def test_partial_migration_classifies_rows_after_completed_item_cursor(self):
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                        ("old:partial", "agent", json.dumps({"text": "excerpt", "truncated": True}), 1))
        index_item(self.db, "old:partial", "agent", "assistant", "excerpt")
        self.db.execute("UPDATE runtime_search_address_cursor SET done=1 WHERE singleton=1")
        self.db.execute("UPDATE runtime_search_item_cursor SET done=1 WHERE singleton=1")
        self.assertEqual(backfill_items(self.db), 0)
        self.assertEqual(backfill_partials(self.db, limit=1), 1)
        self.assertTrue(has_pending(self.db, ["agent"]))
        self.assertEqual(backfill_partials(self.db, limit=1), 0)
        self.assertFalse(has_pending(self.db, ["agent"]))
        self.assertTrue(has_partial(self.db, ["agent"]))

    def test_partial_migration_preserves_recoverable_legacy_full_text(self):
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                        ("old:complete", "agent", json.dumps({"text": "excerpt", "truncated": True}), 1))
        index_item(self.db, "old:complete", "agent", "assistant", "recoverable full legacy body")
        self.db.execute("UPDATE runtime_search_address_cursor SET done=1 WHERE singleton=1")
        self.db.execute("UPDATE runtime_search_item_cursor SET done=1 WHERE singleton=1")
        backfill_partials(self.db)
        self.assertFalse(has_partial(self.db, ["agent"]))
        self.assertEqual(body(self.db, "old:complete", agent="agent"), "recoverable full legacy body")

    def test_partial_migration_resumes_after_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "partials.sqlite3"
            db = sqlite3.connect(path)
            db.row_factory = sqlite3.Row
            db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
            initialize(db)
            db.execute("UPDATE runtime_search_address_cursor SET done=1 WHERE singleton=1")
            db.execute("UPDATE runtime_search_item_cursor SET done=1 WHERE singleton=1")
            for index in range(3):
                db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                           (f"old:{index}", "agent", json.dumps({"text": "excerpt", "truncated": True}), index))
            self.assertEqual(backfill_partials(db, limit=2), 2)
            db.commit()
            db.close()
            reopened = sqlite3.connect(path)
            reopened.row_factory = sqlite3.Row
            initialize(reopened)
            self.assertEqual(backfill_partials(reopened, limit=2), 1)
            self.assertEqual(reopened.execute("SELECT done FROM runtime_search_partial_cursor").fetchone()[0], 1)
            self.assertEqual(reopened.execute("SELECT count(*) FROM runtime_search_partial").fetchone()[0], 3)
            reopened.close()

    def test_failed_index_does_not_erase_committed_body_or_pending_row(self):
        self.item("a:3", "durable body", streaming=True, now=30)
        self.db.execute("DROP TABLE runtime_search")
        with self.assertRaises(sqlite3.OperationalError):
            drain(self.db, now=32)
        self.assertEqual(body(self.db, "a:3"), "durable body")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_pending").fetchone()[0], 1)

    def test_failed_synchronous_final_index_keeps_body_durable(self):
        self.db.execute("DROP TABLE runtime_search")
        self.item("a:failed-final", "body survives index error", now=31)
        self.assertEqual(body(self.db, "a:failed-final"), "body survives index error")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_pending").fetchone()[0], 1)

    def test_remove_clears_body_and_pending_index(self):
        self.item("a:4", "gone", streaming=True, now=40)
        remove(self.db, "a:4")
        self.db.execute("DELETE FROM runtime_items WHERE id='a:4'")
        self.assertIsNone(body(self.db, "a:4"))
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_pending").fetchone()[0], 0)

    def test_remove_clears_legacy_fts_without_address(self):
        self.db.execute("INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
                        ("legacy:gone", "legacy", "assistant", "private legacy text"))
        remove(self.db, "legacy:gone")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_deletions").fetchone()[0], 1)
        self.assertEqual(backfill_addresses(self.db, limit=32), 1)
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search WHERE id='legacy:gone'").fetchone()[0], 0)
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_deletions").fetchone()[0], 0)

    def test_recreated_identity_cancels_legacy_tombstone(self):
        self.db.execute("INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
                        ("legacy:reused", "agent", "assistant", "old body"))
        remove(self.db, "legacy:reused")
        self.item("legacy:reused", "new body")
        self.assertEqual(backfill_addresses(self.db, limit=32), 2)
        row = self.db.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH 'new'").fetchone()
        self.assertEqual(row[0], "new body")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search").fetchone()[0], 1)

    def test_pending_freshness_includes_debounced_streams(self):
        self.item("a:5", "recently streamed", streaming=True, now=50)
        backfill_addresses(self.db)
        backfill_items(self.db)
        backfill_partials(self.db)
        self.assertTrue(has_pending(self.db, ["agent"]))
        self.assertFalse(has_pending(self.db, ["other-agent"]))
        self.assertEqual(drain(self.db, now=50, force=True), 1)
        self.assertEqual(backfill_addresses(self.db), 0)
        self.assertEqual(backfill_items(self.db), 0)
        self.assertFalse(has_pending(self.db, ["agent"]))

    def test_body_and_new_index_updates_use_address_lookups(self):
        self.db.executemany("INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
                            ((str(i), "agent", "assistant", "large legacy history") for i in range(3000)))
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("1500", "agent", json.dumps({}), 1))
        while backfill_addresses(self.db, limit=128):
            pass
        def steps(action):
            count = [0]
            self.db.set_progress_handler(lambda: count.__setitem__(0, count[0] + 1) or 0, 1)
            try:
                action()
            finally:
                self.db.set_progress_handler(None, 0)
            return count[0]
        full_scan = steps(lambda: self.db.execute("SELECT body FROM runtime_search WHERE id='1500'").fetchone())
        mapped_body = steps(lambda: body(self.db, "1500", agent="agent"))
        new_item_update = steps(lambda: index_item(self.db, "new:item", "agent", "assistant", "new text"))
        self.assertLess(mapped_body * 10, full_scan)
        self.assertLess(new_item_update * 10, full_scan)

    def test_removing_unmapped_item_does_not_scan_legacy_fts(self):
        self.db.executemany("INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
                            ((str(i), "agent", "assistant", "large legacy history") for i in range(3000)))
        def steps(action):
            count = [0]
            self.db.set_progress_handler(lambda: count.__setitem__(0, count[0] + 1) or 0, 1)
            try:
                action()
            finally:
                self.db.set_progress_handler(None, 0)
            return count[0]
        full_scan = steps(lambda: self.db.execute("SELECT rowid FROM runtime_search WHERE id='1500'").fetchone())
        removal = steps(lambda: remove(self.db, "1500"))
        self.assertLess(removal * 10, full_scan)
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_deletions WHERE id='1500'").fetchone()[0], 1)

    def test_address_backfill_is_bounded_and_finishes_once(self):
        self.db.executemany("INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
                            ((str(i), "agent", "assistant", "history") for i in range(100)))
        self.assertEqual(backfill_addresses(self.db, limit=17), 17)
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_rows").fetchone()[0], 17)
        while backfill_addresses(self.db, limit=17):
            pass
        self.assertEqual(self.db.execute("SELECT done FROM runtime_search_address_cursor").fetchone()[0], 1)
        self.assertEqual(backfill_addresses(self.db, limit=17), 0)

    def test_body_rejects_mismatched_agent_before_legacy_fallback(self):
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("owned:1", "owner", json.dumps({}), 1))
        index_item(self.db, "owned:1", "owner", "assistant", "private legacy body")
        with self.assertRaisesRegex(ValueError, "requested agent"):
            body(self.db, "owned:1", "fallback", agent="intruder")


if __name__ == "__main__":
    unittest.main()
