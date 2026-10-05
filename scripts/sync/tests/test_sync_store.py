"""Contracts for the production SQLite-backed renderer SyncStore."""
import contextlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPTS))
from codex_sync import SyncStore


class SyncStoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sync-store-")
        self.path = Path(self.temp.name) / "state.sqlite3"
        with self.connect() as db:
            db.executescript("""
                CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);
                CREATE TABLE groups(id TEXT PRIMARY KEY, name TEXT NOT NULL, members TEXT NOT NULL);
                CREATE TABLE analytics_usage(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            """)
        self.builds = 0
        self.transcripts = {"chat": {"agent": {"id": "chat"}, "items": [{"id": "one", "text": "first"}]}}

        def snapshot():
            self.builds += 1
            with self.connect() as db:
                groups = db.execute("SELECT count(*) FROM groups").fetchone()[0]
            return {"groups": groups, "large": "x" * 1000}

        def transcript(key):
            if key not in self.transcripts:
                raise ValueError("Gone")
            return self.transcripts[key]

        self.store = SyncStore(self.connect, snapshot, transcript)

    def tearDown(self):
        self.temp.cleanup()

    @contextlib.contextmanager
    def connect(self):
        db = sqlite3.connect(self.path, timeout=5)
        try:
            with db:
                yield db
        finally:
            db.close()

    def test_identity_and_generation_state_match_production_api(self):
        identity = self.store.identity()
        self.assertEqual(identity["syncProtocol"], 2)
        self.assertIn("workspaceId", identity)
        self.assertNotIn("chatState", identity)
        state = self.store.generation_state()
        self.assertEqual(state["generations"]["state"], state["generations"]["transcripts"])
        self.assertIn("transcriptRevisions", state)

    def test_state_pull_uses_production_snapshot_and_database_invalidation(self):
        first = self.store.pull("state")
        self.assertEqual(self.builds, 1)
        self.assertEqual(json.loads(first["documents"][0]["payload"])["groups"], 0)
        checkpoint = first["checkpoint"]["seq"]

        # Production observes committed database changes through data_version,
        # including unrelated writes, and rebuilds the cached projection.
        with self.connect() as db:
            db.execute("INSERT INTO analytics_usage VALUES ('usage','{}')")
        unrelated = self.store.pull("state", checkpoint)
        self.assertEqual(unrelated["documents"], [])
        self.assertEqual(self.builds, 2)

        with self.connect() as db:
            db.execute("INSERT INTO groups VALUES ('g','team','[]')")
        updated = self.store.pull("state", checkpoint)
        self.assertEqual(json.loads(updated["documents"][0]["payload"])["groups"], 1)
        self.assertGreater(updated["checkpoint"]["seq"], checkpoint)
        self.assertEqual(self.builds, 3)

    def test_transcript_pull_produces_full_then_sparse_delta(self):
        first = self.store.pull("transcript:chat")
        first_checkpoint = first["checkpoint"]["seq"]
        self.assertEqual(json.loads(first["documents"][0]["payload"]), self.transcripts["chat"])

        with self.connect() as db:
            db.execute("INSERT INTO runtime_agents VALUES ('chat','{}')")
            db.execute("INSERT INTO runtime_items VALUES ('item','chat','{}',1)")
        self.transcripts["chat"] = {
            "agent": {"id": "chat"},
            "items": [{"id": "one", "text": "first"}, {"id": "two", "text": "second"}],
        }
        delta = self.store.pull("transcript:chat", first_checkpoint)
        payload = json.loads(delta["documents"][0]["payload"])
        self.assertTrue(payload["delta"])
        self.assertEqual([item["id"] for item in payload["items"]], ["two"])
        self.assertGreater(delta["checkpoint"]["seq"], first_checkpoint)

    def test_missing_transcript_is_a_tombstone(self):
        result = self.store.pull("transcript:gone")
        document = result["documents"][0]
        self.assertTrue(document["_deleted"])
        self.assertEqual(document["payload"], "{}")

    def test_draft_tabs_keep_distinct_branches_and_conflicts_are_returned(self):
        # Mirror RxDB's initial pull, which performs the lazy schema setup
        # before its push handler can begin a write transaction.
        self.store.pull("drafts")
        value = {"device": "phone", "session": "chat", "text": "draft"}
        key = "phone:tab-a:chat"
        row = {"newDocumentState": {"id": key, "payload": json.dumps(value)}}
        self.assertEqual(self.store.push_drafts([row]), [])
        existing = self.store.pull("drafts")["documents"][0]

        changed = {**value, "text": "changed"}
        conflict = self.store.push_drafts([{
            "newDocumentState": {"id": key, "payload": json.dumps(changed)},
        }])
        self.assertEqual(conflict[0]["payload"], existing["payload"])
        accepted = self.store.push_drafts([{
            "newDocumentState": {"id": key, "payload": json.dumps(changed)},
            "assumedMasterState": existing,
        }])
        self.assertEqual(accepted, [])
        self.assertEqual(json.loads(self.store.pull("drafts", existing["seq"])["documents"][0]["payload"]), changed)

    def test_draft_key_rejects_nested_tab_writer_and_mismatched_session(self):
        for key, value in (
            ("phone:tab:a:chat", {"device": "phone", "session": "chat", "text": "bad"}),
            ("phone:chat", {"device": "phone", "session": "other", "text": "bad"}),
        ):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "identity"):
                self.store.push_drafts([{
                    "newDocumentState": {"id": key, "payload": json.dumps(value)},
                }])


if __name__ == "__main__":
    unittest.main()
