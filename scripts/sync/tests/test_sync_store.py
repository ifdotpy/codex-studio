"""Component contracts for scoped generations, checkpoints and draft writes."""
import contextlib
import json
from pathlib import Path
import re
import sqlite3
import tempfile
import threading
import time
import unittest
import sys

SCRIPTS = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPTS))
from sync.sync_store import SyncStore, STATE_TABLES, TRANSCRIPT_TABLES


class SyncStoreTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / "state.sqlite3"
        with self.connect() as db:
            db.executescript("""
                CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);
                CREATE INDEX runtime_item_agent ON runtime_items(agent, created);
                CREATE TABLE analytics_usage(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE VIRTUAL TABLE message_search USING fts5(text);
                CREATE TABLE groups(id TEXT PRIMARY KEY, name TEXT NOT NULL, members TEXT NOT NULL);
            """)
        self.builds = 0
        def snapshot():
            self.builds += 1
            with self.connect() as db:
                groups = db.execute("SELECT count(*) FROM groups").fetchone()[0]
            return {"nodes": [], "groups": groups, "large": "x" * 1000}
        def transcript(key):
            if key == "gone":
                raise ValueError("Gone")
            return {"items": [key]}
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

    def traced_connect(self, statements):
        @contextlib.contextmanager
        def connect():
            db = sqlite3.connect(self.path, timeout=5)
            db.set_trace_callback(statements.append)
            try:
                with db:
                    yield db
            finally:
                db.close()
        return connect

    @staticmethod
    def trigger_ddl(statements):
        return [sql.strip() for sql in statements
                if sql.lstrip().upper().startswith(("CREATE TRIGGER", "DROP TRIGGER"))]

    def test_existing_database_migration_preserves_broad_and_adds_scoped_triggers(self):
        before = self.store.generations()
        with self.connect() as db:
            db.execute("INSERT INTO analytics_usage VALUES ('a','{}')")
            db.execute("INSERT INTO message_search VALUES ('word')")
        self.assertEqual(before, self.store.generations())
        broad = self.store.generation()
        with self.connect() as db:
            db.execute("INSERT INTO runtime_agents VALUES ('a','{}')")
        self.assertGreater(self.store.generation(), broad)
        self.assertGreater(self.store.generations()["state"], before["state"])
        self.assertGreater(self.store.generations()["transcripts"], before["transcripts"])
        upgraded = SyncStore(self.connect, lambda: {}, lambda key: {})
        self.assertEqual(upgraded.generations(), self.store.generations())

    def test_restart_keeps_correct_trigger_definitions_without_ddl(self):
        statements = []
        upgraded = SyncStore(self.traced_connect(statements), lambda: {}, lambda key: {})
        ddl = self.trigger_ddl(statements)
        self.assertEqual(ddl, [])
        self.assertEqual(upgraded.generations(), self.store.generations())

    def test_startup_installs_the_expected_trigger_map(self):
        with self.connect() as db:
            tables = {row[1] for row in db.execute("PRAGMA table_list")
                      if row[0] == "main" and row[2] == "table"}
            actual = {row[0] for row in db.execute(
                "SELECT name FROM sqlite_master WHERE type='trigger' "
                "AND (name LIKE 'sync_watch_%' OR name LIKE 'sync_scope_%')"
            )}
        base_tables = {table for table in tables
                       if not table.startswith(("sync_", "sqlite_")) and
                       re.fullmatch(r"[a-zA-Z0-9_]+", table)}
        expected = {f"sync_watch_{table}_{op}" for table in base_tables
                    for op in ("INSERT", "UPDATE", "DELETE")}
        expected.update(f"sync_scope_{scope}_{table}_{op}"
                        for scope, owned in (("state", STATE_TABLES),
                                             ("transcripts", TRANSCRIPT_TABLES))
                        for table in owned.intersection(tables)
                        for op in ("INSERT", "UPDATE", "DELETE"))
        if "runtime_items" in tables:
            expected.update(("sync_scope_state_runtime_items_first",
                             "sync_scope_state_runtime_items_last"))
        self.assertEqual(actual, expected)

    def test_unrelated_schema_drift_adds_only_new_broad_clock_triggers(self):
        statements = []
        self.store.connect = self.traced_connect(statements)
        with self.connect() as db:
            db.execute('''CREATE TRIGGER app_owned_runtime_agents_insert
                AFTER INSERT ON runtime_agents BEGIN SELECT 1; END''')
            db.execute("CREATE TABLE unrelated_late_table(id TEXT PRIMARY KEY)")
        statements.clear()

        self.store.generations()

        ddl = self.trigger_ddl(statements)
        self.assertEqual(len(ddl), 3, ddl)
        self.assertTrue(all(sql.lstrip().upper().startswith("CREATE TRIGGER") for sql in ddl), ddl)
        self.assertEqual(
            {f'sync_watch_unrelated_late_table_{op}' for op in ("INSERT", "UPDATE", "DELETE")},
            {sql.split('"', 2)[1] for sql in ddl},
        )
        with self.connect() as db:
            self.assertIsNotNone(db.execute(
                "SELECT 1 FROM sqlite_master WHERE type='trigger' "
                "AND name='app_owned_runtime_agents_insert'"
            ).fetchone())

    def test_changed_owned_trigger_body_is_migrated_alone(self):
        name = "sync_scope_state_runtime_agents_INSERT"
        with self.connect() as db:
            db.execute(f'DROP TRIGGER "{name}"')
            db.execute(f'''CREATE TRIGGER "{name}" AFTER INSERT ON "runtime_agents" BEGIN
                UPDATE sync_generation SET value=value+2 WHERE id=1; END''')
        statements = []

        SyncStore(self.traced_connect(statements), lambda: {}, lambda key: {})

        ddl = self.trigger_ddl(statements)
        self.assertEqual(len(ddl), 2, ddl)
        self.assertEqual(ddl[0], f'DROP TRIGGER "{name}"')
        self.assertIn(f'CREATE TRIGGER "{name}"', ddl[1])

    def test_analytics_churn_reuses_snapshot_but_ui_write_rebuilds(self):
        first = self.store.pull("state")
        checkpoint = first["checkpoint"]["seq"]
        generation = self.store.generations()["state"]
        for i in range(20):
            with self.connect() as db:
                db.execute("INSERT INTO analytics_usage VALUES (?, '{}')", (str(i),))
                db.execute("INSERT INTO message_search VALUES (?)", (str(i),))
            same = self.store.pull("state", checkpoint)
            self.assertFalse(same["documents"])
            self.assertEqual(same["generation"], generation)
        self.assertEqual(self.builds, 1)
        with self.connect() as db:
            db.execute("INSERT INTO groups VALUES ('g','team','[]')")
        updated = self.store.pull("state", checkpoint)
        self.assertTrue(updated["documents"])
        self.assertEqual(self.builds, 2)

    def test_transcript_tombstone_and_draft_conflict_semantics(self):
        gone = self.store.pull("transcript:gone")
        self.assertTrue(gone["documents"][0]["_deleted"])
        value = {"device": "phone", "session": "chat", "text": "draft"}
        doc = {"id": "phone:chat", "payload": json.dumps(value)}
        self.assertEqual(self.store.push_drafts([{"newDocumentState": doc}]), [])
        existing = self.store.pull("drafts")["documents"][0]
        changed = {**value, "text": "conflict"}
        conflicts = self.store.push_drafts([{"newDocumentState": {"id": doc["id"], "payload": json.dumps(changed)}}])
        self.assertEqual(conflicts[0]["payload"], existing["payload"])
        accepted = self.store.push_drafts([{"newDocumentState": {"id": doc["id"], "payload": json.dumps(changed)},
                                           "assumedMasterState": existing}])
        self.assertEqual(accepted, [])

    def test_identity_negotiates_scoped_protocol_and_old_clock_stays_readable(self):
        identity = self.store.identity()
        self.assertEqual(identity["syncProtocol"], 2)
        self.assertTrue(identity["chatState"])
        self.assertIn("state", self.store.generations())
        self.assertIsInstance(self.store.generation(), int)

    def test_schema_created_after_startup_gets_broad_and_scoped_triggers(self):
        with self.connect() as db:
            db.executescript("CREATE TABLE runtime_chat_messages(id TEXT PRIMARY KEY, room TEXT); CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY, record TEXT); CREATE TABLE runtime_item_bodies(id TEXT PRIMARY KEY, body TEXT, version INTEGER); CREATE TABLE runtime_search_pending(id TEXT PRIMARY KEY, version INTEGER, due_at REAL);")
        before = self.store.generations()
        with self.connect() as db:
            db.execute("INSERT INTO runtime_chat_messages VALUES ('m','room')")
            db.execute("INSERT INTO runtime_event_meta VALUES ('e','{}')")
            db.execute("INSERT INTO runtime_item_bodies VALUES ('i','body',1)")
            db.execute("INSERT INTO runtime_search_pending VALUES ('i',1,1)")
        after = self.store.generations()
        self.assertGreater(after["state"], before["state"])
        self.assertGreater(after["transcripts"], before["transcripts"])

    def test_populated_late_created_state_table_invalidates_existing_snapshot(self):
        # Use a straightforward snapshot connection while keeping the late table
        # absent until after the first cached projection has been built.
        def notices_snapshot():
            with self.connect() as db:
                exists = db.execute(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_native_notices'"
                ).fetchone()
                if not exists:
                    return {"notices": []}
                return {"notices": [row[0] for row in db.execute(
                    "SELECT id FROM runtime_native_notices ORDER BY id"
                )]}
        store = SyncStore(self.connect, notices_snapshot, lambda key: {})
        first = store.pull("state")
        first_generation = first["generation"]
        with self.connect() as db:
            db.execute("CREATE TABLE runtime_native_notices(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            db.execute("INSERT INTO runtime_native_notices VALUES ('already-here','{}')")
        statements = []
        store.connect = self.traced_connect(statements)
        current = store.generations()["state"]
        ddl = self.trigger_ddl(statements)
        self.assertEqual(len(ddl), 6, ddl)
        self.assertTrue(all(sql.lstrip().upper().startswith("CREATE TRIGGER") for sql in ddl), ddl)
        self.assertGreater(current, first_generation)
        updated = store.pull("state", first["checkpoint"]["seq"])
        self.assertEqual(json.loads(updated["documents"][0]["payload"])["notices"], ["already-here"])

    def test_startup_invalidates_populated_sources_added_while_stopped(self):
        before = self.store.generations()
        with self.connect() as db:
            db.execute("CREATE TABLE runtime_native_notices(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
            db.execute("INSERT INTO runtime_native_notices VALUES ('offline-notice','{}')")
            db.execute("CREATE TABLE runtime_item_bodies(id TEXT PRIMARY KEY, body TEXT, version INTEGER)")
            db.execute("INSERT INTO runtime_item_bodies VALUES ('offline-item','full text',1)")

        restarted = SyncStore(self.connect, lambda: {}, lambda _agent: {})

        after = restarted.generations()
        self.assertGreater(after["state"], before["state"])
        self.assertGreater(after["transcripts"], before["transcripts"])

    def test_search_pending_is_index_only(self):
        with self.connect() as db:
            db.execute("CREATE TABLE runtime_search_pending(id TEXT PRIMARY KEY, version INTEGER, due_at REAL)")
            for table in (
                "runtime_search_partial", "runtime_search_item_cursor",
                "runtime_search_indexed", "runtime_search_rows",
                "runtime_search_address_cursor", "runtime_search_deletions",
            ):
                db.execute(f'CREATE TABLE {table}(id TEXT PRIMARY KEY, record TEXT)')
        before = self.store.generations()
        with self.connect() as db:
            db.execute("INSERT INTO runtime_search_pending VALUES ('item',1,1)")
            for table in (
                "runtime_search_partial", "runtime_search_item_cursor",
                "runtime_search_indexed", "runtime_search_rows",
                "runtime_search_address_cursor", "runtime_search_deletions",
            ):
                db.execute(f"INSERT INTO {table} VALUES ('item','{{}}')")
        self.assertEqual(self.store.generations(), before)

    def test_transcript_item_churn_only_invalidates_state_when_empty_lead_changes(self):
        with self.connect() as db:
            db.execute("INSERT INTO runtime_items VALUES ('one','agent','{}',1)")
        first = self.store.generations()
        with self.connect() as db:
            db.execute("INSERT INTO runtime_items VALUES ('two','agent','{}',2)")
            db.execute("UPDATE runtime_items SET record='changed' WHERE id='two'")
            db.execute("DELETE FROM runtime_items WHERE id='one'")
        middle = self.store.generations()
        self.assertEqual(middle["state"], first["state"])
        self.assertGreater(middle["transcripts"], first["transcripts"])
        with self.connect() as db:
            db.execute("DELETE FROM runtime_items WHERE id='two'")
        self.assertGreater(self.store.generations()["state"], middle["state"])

    def test_runtime_connection_signature_invalidates_only_ui_state(self):
        online = [False]
        store = SyncStore(self.connect, lambda: {"connected": online[0]}, lambda key: {},
                          state_signature=lambda: online[0])
        before = store.generations()
        broad = store.generation()
        legacy = store.legacy_generation()
        online[0] = True
        after = store.generations()
        self.assertGreater(after["state"], before["state"])
        self.assertEqual(after["transcripts"], before["transcripts"])
        self.assertEqual(store.generation(), broad)
        self.assertGreater(store.legacy_generation(), legacy)

    def test_all_visible_volatile_values_invalidate_without_database_writes(self):
        signature = [{
            "connected": True,
            "rateLimits": {"data": {"value": 1}},
            "rateLimitsByAccount": {"other": {"data": {"value": 1}}},
            "connectionIds": {"default": "first"},
        }]
        store = SyncStore(self.connect, lambda: signature[0], lambda key: {},
                          state_signature=lambda: json.dumps(signature[0], sort_keys=True))
        generations = store.generations()
        broad = store.generation()
        for key, value in (
            ("rateLimits", {"data": {"value": 2}}),
            ("rateLimitsByAccount", {"other": {"data": {"value": 2}}}),
            ("connectionIds", {"default": "second"}),
        ):
            prior = generations["state"]
            signature[0][key] = value
            generations = store.generations()
            self.assertGreater(generations["state"], prior)
            self.assertEqual(store.generation(), broad)
        self.assertEqual(generations["transcripts"], 0)

    def test_busy_volatile_signature_is_deferred_without_blocking_generation_read(self):
        lock = threading.Lock()
        value = [0]

        def signature():
            if not lock.acquire(blocking=False):
                return None
            try:
                return value[0]
            finally:
                lock.release()

        store = SyncStore(self.connect, lambda: {}, lambda key: {}, state_signature=signature)
        before = store.generations()["state"]
        lock.acquire()
        value[0] = 1
        try:
            started = time.monotonic()
            busy = store.generations()["state"]
            self.assertLess(time.monotonic() - started, 0.2)
            self.assertEqual(busy, before)
        finally:
            lock.release()
        self.assertGreater(store.generations()["state"], before)

    def test_runtime_item_first_row_trigger_uses_agent_index(self):
        with self.connect() as db:
            db.executemany("INSERT INTO runtime_items VALUES (?,?,?,?)", (
                (f"history-{number}", "busy", "{}", number) for number in range(2000)
            ))
            plan = [row[3] for row in db.execute(
                "EXPLAIN QUERY PLAN SELECT 1 FROM runtime_items WHERE agent=? AND id<>? LIMIT 1",
                ("busy", "new"),
            )]
            self.assertTrue(any("runtime_item_agent" in detail for detail in plan), plan)
        before = self.store.generations()["state"]
        with self.connect() as db:
            db.execute("INSERT INTO runtime_items VALUES ('new','busy','{}',3000)")
        self.assertEqual(self.store.generations()["state"], before)


if __name__ == "__main__":
    unittest.main()
