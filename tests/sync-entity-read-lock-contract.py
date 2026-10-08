#!/usr/bin/env python3
"""Unchanged entity reads do not require SQLite's sole WAL writer."""
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

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
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

            def threads(_self, runtime_agents=None):
                return []

            def chats(_self, db=None):
                return []

            def edges(_self, agents, db=None):
                return []

        self.runtime_view = RuntimeView()
        self.store = SyncStore(self.connect, lambda _key: {},
                               runtime=self.runtime_view, canvas=CanvasView())
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

    def test_changed_monitor_requires_maintenance_and_publishes_the_exact_source(self):
        changed = {**self.monitor, 'status': 'completed', 'tail': 'The final output'}
        with self.connect() as db:
            db.execute('UPDATE runtime_monitors SET record=? WHERE id=?', (json.dumps(changed), 'monitor'))
        self.statements.clear()
        with self.writer(), self.assertRaisesRegex(sqlite3.OperationalError, 'database is locked'):
            self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertIn('BEGIN IMMEDIATE', self.writes())
        refreshed = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertEqual(self.values(refreshed)['entity:monitor:monitor']['tail'], 'The final output')
        self.assertEqual(self.values(refreshed)['entity:monitor:monitor']['status'], 'completed')
        self.statements.clear()
        with self.writer():
            stable = self.store.pull('state:entities:v1', refreshed['checkpoint']['seq'])
        self.assertEqual(stable['documents'], [])
        self.assertEqual(self.writes(), [])

    def test_changed_agent_deletion_retires_its_monitor_through_normal_maintenance(self):
        with self.connect() as db:
            db.execute('UPDATE runtime_agents SET record=? WHERE id=?',
                       (json.dumps({**self.agent, 'deletedAt': 2}), 'owner'))
        response = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        removed = next(row for row in response['documents'] if row['id'] == 'entity:monitor:monitor')
        self.assertTrue(removed['_deleted'])
        self.assertIn('BEGIN IMMEDIATE', self.writes())

    def test_event_window_retires_stale_rows_and_uses_the_global_sequence(self):
        with self.connect() as db:
            db.execute("INSERT INTO sync_documents(seq,scope,id,payload,deleted) VALUES(5000,'drafts','draft','{}',0)")
            put(db, 'event', 'stale', {'id': 'stale', 'agent': 'owner', 'kind': 'user',
                                       'status': 'delivered', 'created': 9})
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

    def test_boot_markers_still_require_seed_and_window_repairs(self):
        for marker in ('seeded', 'agent_organization_fields', 'task_window_migrated', 'event_window_seq'):
            with self.subTest(marker=marker):
                self.store.runtime = self.runtime_view
                with self.connect() as db:
                    db.execute('DELETE FROM sync_entity_meta WHERE key=?', (marker,))
                with self.writer(), self.assertRaisesRegex(sqlite3.OperationalError, 'database is locked'):
                    self.store.pull('state:entities:v1', fresh=True)
                response = self.store.pull('state:entities:v1', fresh=True)
                self.assertTrue(response['documents'])
                with self.connect() as db:
                    self.assertIsNotNone(db.execute('SELECT value FROM sync_entity_meta WHERE key=?', (marker,)).fetchone())

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
        original = self.store.entity_maintenance_needed
        def check(db):
            needed = original(db)
            self.assertFalse(needed)
            with self.connect() as writer:
                writer.execute('BEGIN IMMEDIATE')
                put(writer, 'agent', 'owner', {}, deleted=True)
            return needed
        with patch.object(self.store, 'entity_maintenance_needed', side_effect=check):
            before = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertEqual(before['documents'], [])
        self.assertEqual(before['checkpoint'], self.initial['checkpoint'])
        after = self.store.pull('state:entities:v1', before['checkpoint']['seq'])
        self.assertTrue(next(row for row in after['documents'] if row['id'] == 'entity:agent:owner')['_deleted'])
        self.assertGreater(after['checkpoint']['seq'], before['checkpoint']['seq'])
        fresh = self.store.pull('state:entities:v1', fresh=True, initial_high=before['maxSeq'])
        self.assertTrue(next(row for row in fresh['documents'] if row['id'] == 'entity:agent:owner')['_deleted'])

    def test_maintenance_reads_again_after_the_writer_is_acquired(self):
        with self.connect() as db:
            db.execute('UPDATE runtime_monitors SET record=? WHERE id=?',
                       (json.dumps({**self.monitor, 'tail': 'Earlier output'}), 'monitor'))
        original = self.store.entity_maintenance_needed
        def check(db):
            needed = original(db)
            self.assertTrue(needed)
            with self.connect() as writer:
                writer.execute('UPDATE runtime_monitors SET record=? WHERE id=?',
                               (json.dumps({**self.monitor, 'tail': 'Newer output'}), 'monitor'))
            return needed
        with patch.object(self.store, 'entity_maintenance_needed', side_effect=check):
            response = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertEqual(self.values(response)['entity:monitor:monitor']['tail'], 'Newer output')

    def test_pruner_runs_only_when_its_durable_count_requires_a_batch(self):
        with self.connect() as db:
            db.execute("UPDATE sync_entity_meta SET value='10001' WHERE key='entity_tombstone_count'")
        self.statements.clear()
        with patch.object(self.store, '_schedule_entity_pruning') as prune, self.writer():
            response = self.store.pull('state:entities:v1', self.initial['checkpoint']['seq'])
        self.assertEqual(response['documents'], [])
        self.assertEqual(self.writes(), [])
        prune.assert_called_once_with()


if __name__ == '__main__':
    unittest.main()
