#!/usr/bin/env python3
"""Committed entity reads never require SQLite's sole WAL writer."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_sync import SyncStore
from codex_sync_entities import encoded, install_bypass_triggers, max_seq, put, register_functions


class EntityReadLockContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='studio-entity-read-lock-')
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'state.sqlite3'
        self.anchor = sqlite3.connect(self.path)
        self.addCleanup(self.anchor.close)
        self.anchor.executescript('''PRAGMA journal_mode=WAL;
            CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT);
            CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);
            CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,status TEXT,
                                        created REAL,error TEXT);''')
        self.agent = {'id': 'owner', 'kind': 'agent', 'rootId': 'owner', 'name': 'Owner', 'status': 'paused', 'deletedAt': None}
        self.monitor = {'id': 'monitor', 'agent': 'owner', 'status': 'running', 'created': 1, 'tail': 'First output'}
        self.anchor.execute('INSERT INTO runtime_agents VALUES (?,?)', ('owner', json.dumps(self.agent)))
        self.anchor.execute('INSERT INTO runtime_monitors VALUES (?,?)', ('monitor', json.dumps(self.monitor)))
        self.anchor.commit()
        self.statements = []
        class RuntimeView:
            def agent_entity_view(_self, _db, record):
                return {**record, 'kind': 'agent'}

            def complaint_entity_view(_self, _db, record):
                return record

            def room_entity_view(_self, _db, room_id):
                return None

            def workspace_entity_view(_self, _db, base=None):
                return {**(base or {}), 'connected': False, 'projectOrganizationVersion': 1,
                        'peerTeamsVersion': 1, 'tasksHistoryLimit': 100}

        class CanvasView:
            root = Path(self.path).parent

            def threads(_self, runtime_agents=None, **_unused):
                return []

            def chats(_self, db=None):
                return []

            def edges(_self, agents, db=None):
                return []

        self.runtime_view = RuntimeView()
        self.canvas_view = CanvasView()
        self.store = SyncStore(self.connect, lambda _key: {},
                               runtime=self.runtime_view, canvas=self.canvas_view)
        with self.connect() as db:
            install_bypass_triggers(db)
        self.initial = self.store.pull('state:entities:v1')
        self.statements.clear()

    @contextmanager
    def connect(self):
        db = sqlite3.connect(self.path, timeout=.05)
        register_functions(db)
        db.set_trace_callback(self.statements.append)
        try:
            with db:
                yield db
        finally:
            db.close()

    @contextmanager
    def writer(self):
        db = sqlite3.connect(self.path)
        db.execute('BEGIN IMMEDIATE')
        try:
            yield db
        finally:
            db.rollback()
            db.close()

    def writes(self):
        return [sql for sql in self.statements if sql.lstrip().upper().startswith(
            ('BEGIN IMMEDIATE', 'INSERT', 'UPDATE', 'DELETE', 'REPLACE', 'CREATE', 'DROP', 'ALTER'))]

    def values(self, response):
        return {row['id']: json.loads(row['payload'])['value'] for row in response['documents']}

    def test_unchanged_pull_and_fresh_entities_remain_readable_with_an_unrelated_writer(self):
        with patch.object(self.store, '_schedule_entity_pruning') as prune, self.writer():
            delta = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
            fresh = self.store.pull('state:entities:v1', fresh=True)
        self.assertEqual(delta['documents'], [])
        self.assertEqual(delta['checkpoint'], self.initial['checkpoint'])
        self.assertEqual(fresh['documents'], self.initial['documents'])
        self.assertEqual(fresh['maxSeq'], self.initial['maxSeq'])
        self.assertEqual(fresh['initialHigh'], self.initial['maxSeq'])
        self.assertEqual(self.writes(), [])
        prune.assert_not_called()

    def test_startup_repairs_changed_monitor_before_readers_run(self):
        changed = {**self.monitor, 'status': 'completed', 'tail': 'The final output'}
        with self.connect() as db:
            db.execute('UPDATE runtime_monitors SET record=? WHERE id=?', (json.dumps(changed), 'monitor'))
        # A newly constructed store represents a restart. Its first pull must
        # reconcile state that could have been left by an older server build.
        self.store = SyncStore(self.connect, lambda _key: {},
                               runtime=self.runtime_view, canvas=self.canvas_view)
        self.statements.clear()
        with self.writer():
            refreshed = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
            fresh = self.store.pull('state:entities:v1', fresh=True)
        self.assertEqual(self.writes(), [])
        self.assertEqual(self.values(fresh)['entity:monitor:monitor']['tail'], 'The final output')
        self.assertEqual(self.values(refreshed)['entity:monitor:monitor']['tail'], 'The final output')
        self.assertEqual(self.values(refreshed)['entity:monitor:monitor']['status'], 'completed')
        self.statements.clear()
        with self.writer():
            stable = self.store.pull('state:entities:v1', refreshed['checkpoint']['seq'])
        self.assertEqual(stable['documents'], [])
        self.assertEqual(self.writes(), [])

    def test_ordinary_pull_does_not_scan_the_monitor_window(self):
        import codex_entity_contracts
        import codex_sync_entities
        with patch.object(codex_entity_contracts, 'monitor_records', wraps=codex_entity_contracts.monitor_records) as scan_records, \
             patch.object(codex_sync_entities, '_recent_monitor_records', wraps=codex_sync_entities._recent_monitor_records) as project_window:
            unchanged = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertEqual(unchanged['documents'], [])
        scan_records.assert_not_called()
        project_window.assert_not_called()

    def test_restart_repairs_stale_monitor_state_before_first_pull_returns(self):
        changed = {**self.monitor, 'tail': 'Stale across restart'}
        with self.connect() as db:
            db.execute('UPDATE runtime_monitors SET record=? WHERE id=?',
                       (json.dumps(changed), 'monitor'))
        restarted = SyncStore(self.connect, lambda _key: {},
                              runtime=self.runtime_view, canvas=self.canvas_view)
        response = restarted.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        monitor = self.values(response)['entity:monitor:monitor']
        self.assertEqual(monitor['tail'], 'Stale across restart')
        self.assertGreater(response['checkpoint']['seq'], self.initial['checkpoint']['seq'])

    def test_changed_agent_deletion_retires_its_monitor_through_normal_maintenance(self):
        with self.connect() as db:
            db.execute('UPDATE runtime_agents SET record=? WHERE id=?',
                       (json.dumps({**self.agent, 'deletedAt': 2}), 'owner'))
        self.store = SyncStore(self.connect, lambda _key: {},
                               runtime=self.runtime_view, canvas=self.canvas_view)
        response = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        removed = next(row for row in response['documents'] if row['id'] == 'entity:monitor:monitor')
        self.assertTrue(removed['_deleted'])
        self.assertIn('BEGIN IMMEDIATE', self.writes())

    def test_event_window_retires_stale_rows_and_uses_the_global_sequence(self):
        with self.connect() as db:
            db.execute("INSERT INTO sync_documents(seq,scope,id,payload,deleted) VALUES(5000,'drafts','draft','{}',0)")
            db.execute("INSERT INTO runtime_events VALUES ('stale','owner','user','delivered',9,NULL)")
            db.execute("DELETE FROM runtime_events WHERE id='stale'")
            db.execute("INSERT INTO runtime_events VALUES ('new-event','owner','user','pending',10,NULL)")
        response = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        rows = {row['id']: row for row in response['documents']}
        self.assertTrue(rows['entity:event:stale']['_deleted'])
        self.assertFalse(rows['entity:event:new-event']['_deleted'])
        self.assertGreater(response['checkpoint']['seq'], 5000)
        self.statements.clear()
        with self.writer():
            self.assertEqual(self.store.pull('state:entities:v1', response['checkpoint']['seq'])['documents'], [])
        self.assertEqual(self.writes(), [])

    def test_event_window_writer_projects_only_the_changed_or_backfilled_rows(self):
        import codex_entity_contracts
        import codex_sync_entities
        projected = []
        original_project = codex_sync_entities.project
        def counted_project(collection, record):
            if collection == 'event':
                projected.append(record['id'])
            return original_project(collection, record)
        def add(key, created):
            with self.connect() as db:
                db.execute("INSERT INTO runtime_events VALUES (?,'owner','user','pending',?,NULL)",
                           (key, created))
        with patch.object(codex_entity_contracts, 'EVENT_WINDOW', 3):
            with self.connect() as db:
                install_bypass_triggers(db)
            for index in range(3):
                add(f'event-{index}', index)
            first = self.store.pull('state:entities:v1', fresh=True)
            with patch.object(codex_sync_entities, 'project', counted_project):
                add('event-3', 3)
            # The existing trigger hashes and stores its source payload. Window
            # reconciliation must not reproject any already-live row.
            self.assertEqual(projected, ['event-3', 'event-3'])
            self.statements.clear()
            with self.writer():
                moved = self.store.pull('state:entities:v1', first['checkpoint']['seq'])
                fresh = self.store.pull('state:entities:v1', fresh=True)
            self.assertEqual(self.writes(), [])
            self.assertEqual({key for key in self.values(fresh) if key.startswith('entity:event:')},
                             {'entity:event:event-1', 'entity:event:event-2', 'entity:event:event-3'})
            rows = {row['id']: row for row in moved['documents']}
            self.assertTrue(rows['entity:event:event-0']['_deleted'])
            projected.clear()
            with patch.object(codex_sync_entities, 'project', counted_project), self.connect() as db:
                db.execute("DELETE FROM runtime_events WHERE id='event-3'")
            self.assertEqual(projected, ['event-0', 'event-0'])
            with self.connect() as db:
                expected = {record['id']: codex_sync_entities.project('event', record)
                            for record in codex_entity_contracts.event_records(db)}
                actual = {key: json.loads(payload)['value'] for key, payload in db.execute(
                    "SELECT id,payload FROM sync_entities WHERE collection='event' AND deleted=0")}
            self.assertEqual(actual, expected)

    def test_generated_rust_event_trigger_sql_matches_its_python_owner(self):
        from studio_api.generate_sync_schema import SCHEMA_PATH, generated_schema
        source = SCHEMA_PATH.read_text()
        self.assertEqual(source, generated_schema(source),
                         'codex_sync_entities.event_trigger_sql owns generated Rust event trigger SQL')

    def test_noop_and_startup_repair_do_not_advance_event_sequences(self):
        with self.connect() as db:
            db.execute("INSERT INTO runtime_events VALUES ('event','owner','user','pending',1,NULL)")
        before = self.store.pull('state:entities:v1', fresh=True)
        with self.connect() as db:
            db.execute("UPDATE runtime_events SET status=status WHERE id='event'")
        self.store.prepare_entities()
        after = self.store.pull('state:entities:v1', before['checkpoint']['seq'])
        self.assertEqual(after['documents'], [])
        self.assertEqual(after['checkpoint'], before['checkpoint'])

    def test_boot_markers_are_repaired_before_pulls(self):
        for marker in ('seeded', 'agent_organization_fields', 'task_window_migrated', 'event_window_seq'):
            with self.subTest(marker=marker):
                with self.connect() as db:
                    db.execute('DELETE FROM sync_entity_meta WHERE key=?', (marker,))
                self.store = SyncStore(self.connect, lambda _key: {},
                                       runtime=self.runtime_view, canvas=self.canvas_view)
                self.statements.clear()
                with self.writer():
                    response = self.store.pull('state:entities:v1', fresh=True)
                self.assertTrue(response['documents'])
                self.assertEqual(self.writes(), [])
                with self.connect() as db:
                    self.assertIsNotNone(db.execute('SELECT value FROM sync_entity_meta WHERE key=?', (marker,)).fetchone())

    def test_attach_prepares_revision_triggers_for_new_runtime_tables(self):
        path = Path(self.temp.name) / 'brand-new.sqlite3'
        @contextmanager
        def connect():
            db = sqlite3.connect(path)
            register_functions(db)
            try:
                with db:
                    yield db
            finally:
                db.close()
        store = SyncStore(connect, lambda _key: {})
        with connect() as db:
            db.executescript("""CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
                CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT);
                CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);""")
        store.runtime = self.runtime_view
        store.prepare_entities()
        with connect() as db:
            db.execute('INSERT INTO runtime_agents VALUES (?,?)', ('owner', json.dumps(self.agent)))
        self.assertEqual(store.transcript_revision('owner'), 1)

    def test_boot_drains_all_legacy_task_window_batches(self):
        with self.connect() as db:
            db.execute("DELETE FROM sync_entity_meta WHERE key='task_window_migrated'")
            for index in range(205):
                put(db, 'task', f'legacy-{index}', {'id': f'legacy-{index}', 'agent': 'owner',
                                                  'status': 'completed', 'created': index})
        store = SyncStore(self.connect, lambda _key: {},
                          runtime=self.runtime_view, canvas=self.canvas_view)
        self.statements.clear()
        with self.writer():
            fresh = store.pull('state:entities:v1', fresh=True)
        self.assertFalse(any(key.startswith('entity:task:') for key in self.values(fresh)))
        self.assertEqual(self.writes(), [])
        with self.connect() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM sync_entities WHERE collection='task' AND deleted=1").fetchone()[0], 205)

    def test_current_markers_allow_pull_without_runtime_or_writer_lock(self):
        self.store.runtime = None
        self.statements.clear()
        with self.writer():
            unchanged = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertEqual(self.writes(), [])
        self.assertEqual(unchanged['documents'], [])

    def test_legacy_chat_agent_alias_is_read_without_writer_lock(self):
        payload, digest, _ = encoded('agent', 'historical-chat', {
            'id': 'historical-chat', 'kind': 'chat', 'name': 'Old chat', 'tail': 'reply',
        })
        with self.connect() as db:
            seq = max_seq(db) + 1
            db.execute('''INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
                          VALUES ('agent','historical-chat',?,?,?,0)''', (seq, digest, payload))
        self.statements.clear()
        with self.writer():
            response = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        rows = {row['id']: row for row in response['documents']}
        self.assertEqual(rows['entity:agent:historical-chat']['seq'], seq)
        self.assertFalse(rows['entity:agent:historical-chat']['_deleted'])
        self.assertEqual(rows['entity:agent:historical-chat']['payload'], payload)
        self.assertEqual(self.writes(), [])

    def test_concurrent_commit_cannot_move_a_read_cursor_past_unseen_changes(self):
        import codex_sync_entities
        original = codex_sync_entities.max_seq
        def high_watermark(db):
            high = original(db)
            with self.connect() as writer:
                writer.execute('BEGIN IMMEDIATE')
                put(writer, 'agent', 'owner', {}, deleted=True)
            return high
        with patch.object(codex_sync_entities, 'max_seq', side_effect=high_watermark):
            before = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertEqual(before['documents'], [])
        self.assertEqual(before['checkpoint'], self.initial['checkpoint'])
        after = self.store.pull('state:entities:v1', before['checkpoint']['seq'])
        self.assertTrue(next(row for row in after['documents'] if row['id'] == 'entity:agent:owner')['_deleted'])
        self.assertGreater(after['checkpoint']['seq'], before['checkpoint']['seq'])
        fresh = self.store.pull('state:entities:v1', fresh=True, initial_high=before['maxSeq'])
        self.assertTrue(next(row for row in fresh['documents'] if row['id'] == 'entity:agent:owner')['_deleted'])

    def test_event_window_moves_atomically_for_multiple_changes_and_rollback(self):
        import codex_entity_contracts
        import codex_sync_entities
        with patch.object(codex_entity_contracts, 'EVENT_WINDOW', 3):
            with self.connect() as db:
                install_bypass_triggers(db)
                db.executemany("INSERT INTO runtime_events VALUES (?,'owner','user','pending',?,NULL)",
                               [(f'event-{index}', index) for index in range(7)])
                expected = {record['id'] for record in codex_entity_contracts.event_records(db)}
                actual = {row[0] for row in db.execute(
                    "SELECT id FROM sync_entities WHERE collection='event' AND deleted=0")}
                self.assertEqual(actual, expected)
            self.statements.clear()
            with self.writer() as writer:
                register_functions(writer)
                writer.execute("INSERT INTO runtime_events VALUES ('rolled-back','owner','user','pending',99,NULL)")
                delta = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
                fresh = self.store.pull('state:entities:v1', fresh=True)
            rows = {row['id']: row for row in delta['documents']}
            self.assertTrue(all(rows[f'entity:event:event-{index}']['_deleted'] for index in range(4)))
            self.assertEqual({key for key in self.values(fresh) if key.startswith('entity:event:')},
                             {'entity:event:event-4', 'entity:event:event-5', 'entity:event:event-6'})
            self.assertNotIn('entity:event:rolled-back', rows)
            # The fixture writer is intentionally outside the observed provider.
            self.assertEqual(self.writes(), [])
            with self.connect() as db:
                self.assertIsNone(db.execute("SELECT 1 FROM runtime_events WHERE id='rolled-back'").fetchone())

    def test_pruner_runs_only_when_its_durable_count_requires_a_batch(self):
        # Each new tombstone at the limit must not start a one-row prune.
        for count, due in (('10001', False), ('10500', False), ('10501', True)):
            with self.subTest(count=count):
                with self.connect() as db:
                    db.execute("UPDATE sync_entity_meta SET value=? WHERE key='entity_tombstone_count'", (count,))
                self.statements.clear()
                with patch.object(self.store, '_schedule_entity_pruning') as prune, self.writer():
                    self.store.maintain_entities()
                    response = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
                self.assertEqual(response['documents'], [])
                self.assertEqual(self.writes(), [])
                self.assertEqual(prune.call_count, int(due))


if __name__ == '__main__':
    unittest.main()
