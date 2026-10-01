"""Online move from text-bearing FTS into a contentless index."""
import hashlib
import json
import sqlite3
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_search_text import search_text, search_texts
from codex_work import WorkMixin


class SearchIndex(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(":memory:")
        self.db.row_factory = sqlite3.Row
        self.db.executescript("""
        CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL);
        CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,text TEXT);
        CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT NOT NULL);
        CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize='unicode61');
        CREATE TABLE runtime_search_rows(id TEXT PRIMARY KEY,search_rowid INTEGER UNIQUE);
        CREATE TABLE runtime_search_indexed(id TEXT PRIMARY KEY);
        CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='',contentless_delete=1,tokenize='unicode61');
        CREATE TABLE runtime_search_next_meta(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,search_rowid INTEGER UNIQUE);
        CREATE TABLE runtime_search_rollout(id INTEGER PRIMARY KEY,phase TEXT,cursor INTEGER,updated REAL);
        INSERT INTO runtime_search_rollout VALUES(1,'building',0,0);
        """)
        self.work = WorkMixin()

    def tearDown(self):
        self.db.close()

    def seed(self, count):
        old_rows = []
        item_rows = []
        for n in range(count):
            key = f"agent:{n:04}"
            body = f"original evidence document {n} " + ("shared phrase " * (2000 if n % 10 == 0 else 4))
            truncated = n % 10 == 0
            record = {"id": key, "role": "assistant", "title": "assistant",
                      "text": body[:20000] if truncated else body, "truncated": truncated}
            item_rows.append((key, "agent", json.dumps(record), n))
            old_rows.append((key, "agent", "assistant", body))
        self.db.executemany("INSERT INTO runtime_items VALUES (?,?,?,?)", item_rows)
        for row in old_rows:
            cursor = self.db.execute("INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)", row)
            self.db.execute("INSERT INTO runtime_search_rows VALUES (?,?)", (row[0], cursor.lastrowid))
            self.db.execute("INSERT INTO runtime_search_indexed VALUES (?)", (row[0],))
        self.db.commit()

    def test_resumable_migration_preserves_truncated_bodies_and_old_search(self):
        self.seed(31)
        legacy_hits = self.db.execute("SELECT id FROM runtime_search WHERE runtime_search MATCH 'original' ORDER BY rank").fetchall()
        self.assertEqual(len(legacy_hits), 31)
        self.assertTrue(self.work._search_migration_batch(self.db, batch_size=7))
        self.db.commit()
        # A committed cursor resumes on the next batch; the old table remains queryable.
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search WHERE runtime_search MATCH 'original'").fetchone()[0], 31)
        while self.work._search_migration_batch(self.db, batch_size=7):
            self.db.commit()
        self.db.commit()
        self.work._search_migration_verify_and_switch(self.db)
        self.db.commit()
        self.assertEqual(self.db.execute("SELECT phase FROM runtime_search_rollout WHERE id=1").fetchone()[0], "dropping")
        legacy = {row["id"]: row["body"] for row in self.db.execute("SELECT id,body FROM runtime_search")}
        fulltext = {row["id"]: search_text(self.db, row["id"]) for row in self.db.execute(
            "SELECT id FROM runtime_items WHERE json_extract(record,'$.truncated')=1")}
        self.assertEqual(len(fulltext), 4)
        for key, body in fulltext.items():
            self.assertEqual(hashlib.sha256(body.encode()).digest(), hashlib.sha256(legacy[key].encode()).digest())
        new_hits = self.db.execute("SELECT m.id FROM runtime_search_next JOIN runtime_search_next_meta m ON m.search_rowid=runtime_search_next.rowid WHERE runtime_search_next MATCH 'original' ORDER BY rank").fetchall()
        self.assertEqual({r[0] for r in new_hits}, {r[0] for r in legacy_hits})
        self.assertEqual([r[0] for r in new_hits], [r[0] for r in self.db.execute(
            "SELECT id FROM runtime_search WHERE runtime_search MATCH 'original' ORDER BY rank")])
        self.work._search_cleanup_batch(self.db, batch_size=4)
        self.db.commit()
        while self.db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_search'").fetchone():
            self.work._search_cleanup_batch(self.db, batch_size=4)
            self.db.commit()
        self.assertEqual(self.db.execute("SELECT phase FROM runtime_search_rollout WHERE id=1").fetchone()[0], "complete")
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_next WHERE runtime_search_next MATCH 'original'").fetchone()[0], 31)
        self.assertEqual(search_text(self.db, "agent:0000"), legacy["agent:0000"])

    def test_worker_builds_index_after_the_row_check_table_is_gone(self):
        # Live state on 2026-10-01: the row check finished and dropped its table,
        # and runtime_search_next did not exist yet. The worker must still build.
        import tempfile, threading
        from contextlib import contextmanager
        from unittest.mock import patch
        self.seed(12)
        self.db.execute("DROP TABLE runtime_search_next")
        self.db.commit()
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "canvas.sqlite3"
            self.db.execute("VACUUM INTO ?", (str(path),))
            work = WorkMixin()
            work.db_path, work.closed, work.lock = path, False, threading.RLock()

            @contextmanager
            def db():
                connection = sqlite3.connect(path)
                connection.row_factory = sqlite3.Row
                try:
                    with connection:
                        yield connection
                finally:
                    connection.close()
            work.db = db
            steps = []
            def sleep(_seconds):
                steps.append(1)
                if len(steps) > 500:
                    work.closed = True
            with patch("codex_work.time.sleep", side_effect=sleep), \
                    patch("codex_work.shutil.disk_usage", return_value=type("U", (), {"free": 64 * 1024**3})()):
                work._search_migration_run()
            self.assertIsNone(getattr(work, "search_migration_error", None))
            check = sqlite3.connect(path)
            self.assertEqual(check.execute("SELECT phase FROM runtime_search_rollout").fetchone()[0], "complete")
            self.assertEqual(check.execute("SELECT count(*) FROM runtime_search_next WHERE runtime_search_next MATCH 'original'").fetchone()[0], 12)
            check.close()

    def test_live_updates_and_deletes_are_transactional(self):
        self.seed(2)
        self.work._search_migration_batch(self.db)
        self.db.commit()
        key = "agent:0001"
        record = json.loads(self.db.execute("SELECT record FROM runtime_items WHERE id=?", (key,)).fetchone()[0])
        record["text"] = "updated durable evidence"
        self.db.execute("UPDATE runtime_items SET record=? WHERE id=?", (json.dumps(record), key))
        self.work.index_item(self.db, key, "agent", "assistant")
        self.db.commit()
        self.assertEqual([r[0] for r in self.db.execute("SELECT id FROM runtime_search_next JOIN runtime_search_next_meta ON search_rowid=runtime_search_next.rowid WHERE runtime_search_next MATCH 'updated'")], [key])
        self.db.execute("BEGIN")
        self.work.delete_search_item(self.db, key)
        self.db.rollback()
        self.assertEqual(self.db.execute("SELECT count(*) FROM runtime_search_next WHERE runtime_search_next MATCH 'updated'").fetchone()[0], 1)

    def test_text_accessor_prefers_fulltext_and_resolves_input_events(self):
        body = "complete " * 3000
        record = {"id": "agent:long", "text": body[:20000], "truncated": True}
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("agent:long", "agent", json.dumps(record), 1))
        self.db.execute("INSERT INTO runtime_item_fulltext VALUES (?,?)", ("agent:long", body))
        self.assertEqual(search_text(self.db, "agent:long"), body)
        self.db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)", ("agent:batch", "agent", json.dumps({"id":"agent:batch","text":"","inputs":[{"id":"evt-1"}]}), 2))
        self.db.execute("INSERT INTO runtime_events VALUES (?,?,?)", ("evt-1", "agent", "event body"))
        self.assertEqual(search_texts(self.db, ["agent:batch"])["agent:batch"], "event body")

    def test_excerpt_window_matches_fts5_plain_snippet_format(self):
        body = " ".join(f"word{i}" for i in range(80))
        self.db.execute("CREATE VIRTUAL TABLE runtime_search_snippets USING fts5(body,tokenize='unicode61')")
        self.db.execute("INSERT INTO runtime_search_snippets(body) VALUES (?)", (body,))
        fts = self.db.execute("SELECT snippet(runtime_search_snippets,0,'','',' … ',30) FROM runtime_search_snippets WHERE runtime_search_snippets MATCH 'word40'").fetchone()[0]
        self.assertEqual(self.work._search_excerpt(body, "word40"), fts)
        accented = "start " + ("ordinary " * 40) + "café " + ("ordinary " * 40)
        self.assertIn("café", self.work._search_excerpt(accented, "cafe"))


if __name__ == "__main__":
    unittest.main()
