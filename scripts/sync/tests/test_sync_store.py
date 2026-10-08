"""Contracts for the production SQLite-backed renderer SyncStore."""
import contextlib
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
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

        def transcript(key):
            if key not in self.transcripts:
                raise ValueError("Gone")
            return self.transcripts[key]

        self.store = SyncStore(self.connect, transcript)

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

    def test_identity_has_no_legacy_chat_state_flag(self):
        identity = self.store.identity()
        self.assertEqual(identity["syncProtocol"], 2)
        self.assertIn("workspaceId", identity)
        self.assertNotIn("chatState", identity)

    def test_legacy_state_scopes_are_rejected(self):
        for scope in ("state", "state:chat"):
            with self.subTest(scope=scope), self.assertRaisesRegex(ValueError, "Invalid sync scope"):
                self.store.pull(scope)

    def test_entity_pull_never_returns_snapshot_private_agent_or_rule_fields(self):
        agent = {
            "id": "worker-a", "kind": "agent", "workerBaseBehindMain": 182,
            "nativeNameSynced": {"accountKey": "default", "threadId": "thread-a",
                                 "name": "Worker", "privateMarker": "omit"},
            "accountHistory": [{"threadId": "old-thread"}],
            "deliveredMode": {"epoch": ["thread-a", 136]},
            "nativeRelease": {"phase": "released", "targetEpoch": 0, "targetRootId": "lead-a"},
            "startAttempt": {"id": "attempt-a", "claudeInputRequest": {"input": "private"}},
        }
        rule = {
            "id": "rule-a", "agent": "worker-a", "name": "Housekeeping",
            "enabled": True, "description": "Scheduled maintenance",
            "kind": "interval",
            "intervalSeconds": 30, "nextAt": 100.0, "at": 90.0,
            "path": "/repo", "event": "file-change", "command": "check",
            "stallTimeoutSeconds": 1800, "livenessCommand": "alive",
            "text": "Rule summary", "status": "completed", "checks": 4,
            "wakes": 2, "minimumWorkers": 3, "durationMinutes": 5,
            "error": "last check warning", "lastExitCode": 0,
            "lastOutput": "public rule output",
            "restartHoldNotified": {"epoch": 1, "reason": "restart"},
            "lastFinished": 2.0,
            "activeWorkers": 1, "lastStallExitCode": 0, "stallProbe": False,
            "eventText": "private event",
        }
        work = {"id": "work-a", "archive": {"status": "kept"},
                "archiveIntent": {"status": "pending"}, "releases": [{"agent": "worker-a"}]}
        with self.connect() as db:
            db.executescript("""
                CREATE TABLE runtime_rules(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            """)
            db.execute("INSERT INTO runtime_agents VALUES (?,?)", (agent["id"], json.dumps(agent)))
            db.execute("INSERT INTO runtime_rules VALUES (?,?)", (rule["id"], json.dumps(rule)))
            db.execute("INSERT INTO runtime_work VALUES (?,?)", (work["id"], json.dumps(work)))

        pulled = self.store.pull("state:entities:v1", fresh=True, limit=500)
        entities = {
            document["id"]: json.loads(document["payload"])["value"]
            for document in pulled["documents"]
        }
        self.assertEqual(entities["entity:rule:rule-a"], {
            "id": "rule-a", "agent": "worker-a", "name": "Housekeeping",
            "enabled": True, "description": "Scheduled maintenance",
        })
        self.assertEqual(entities["entity:work:work-a"]["archive"], {"status": "kept"})

        private_keys = {
            "accountHistory", "deliveredMode", "targetEpoch", "targetRootId",
            "claudeInputRequest", "privateMarker", "restartHoldNotified",
            "nativeNameSynced", "workerBaseBehindMain",
            "kind", "intervalSeconds", "nextAt", "at", "path", "event",
            "command", "stallTimeoutSeconds", "livenessCommand", "text",
            "status", "checks", "wakes", "minimumWorkers", "durationMinutes",
            "error", "lastExitCode", "lastOutput",
            "lastFinished", "activeWorkers", "lastStallExitCode",
            "stallProbe", "eventText", "archiveIntent", "releases",
        }
        def keys(value):
            if isinstance(value, dict):
                yield from value.keys()
                for nested in value.values():
                    yield from keys(nested)
            elif isinstance(value, list):
                for nested in value:
                    yield from keys(nested)

        self.assertFalse(private_keys.intersection(keys(entities)) - {"kind", "status"})
        for field in private_keys:
            if field not in {"kind", "status"}:
                self.assertNotIn(field, entities["entity:rule:rule-a"])
        with self.connect() as db:
            stored_agent = json.loads(db.execute(
                "SELECT record FROM runtime_agents WHERE id='worker-a'"
            ).fetchone()[0])
            stored_rule = json.loads(db.execute(
                "SELECT record FROM runtime_rules WHERE id='rule-a'"
            ).fetchone()[0])
        self.assertIn("accountHistory", stored_agent)
        self.assertIn("restartHoldNotified", stored_rule)

    def test_lazy_version_schema_initialization_is_idempotent(self):
        self.store._ensure_versions()
        with self.connect() as db:
            first = {row[0] for row in db.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger' "
                "AND name LIKE 'sync_transcript_revision_%'"
            )}
            self.assertEqual(len(first), 6)
            self.assertIsNotNone(db.execute(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_versions'"
            ).fetchone())

        self.store._ensure_versions()

        with self.connect() as db:
            second = {row[0] for row in db.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger' "
                "AND name LIKE 'sync_transcript_revision_%'"
            )}
        self.assertEqual(second, first)

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

    def test_fresh_store_first_draft_push_reproduces_lazy_schema_lock(self):
        value = {"device": "phone", "session": "chat", "text": "draft"}
        self.assertEqual(self.store.push_drafts([{
            "newDocumentState": {"id": "phone:chat", "payload": json.dumps(value)},
        }]), [])
        self.assertTrue(self.store._versions_ready)
        with self.connect() as db:
            trigger_count = db.execute(
                "SELECT count(*) FROM sqlite_master WHERE type='trigger' "
                "AND name LIKE 'sync_transcript_revision_%'"
            ).fetchone()[0]
        self.assertEqual(trigger_count, 6)

        self.store._ensure_versions()
        with self.connect() as db:
            self.assertEqual(db.execute(
                "SELECT count(*) FROM sqlite_master WHERE type='trigger' "
                "AND name LIKE 'sync_transcript_revision_%'"
            ).fetchone()[0], trigger_count)


if __name__ == "__main__":
    unittest.main()
