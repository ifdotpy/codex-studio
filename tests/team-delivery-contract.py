#!/usr/bin/env python3
"""Team checks for durable events, without a model or shared runtime state."""

import json
from pathlib import Path
import sqlite3
import sys
import threading
from types import SimpleNamespace
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_team_isolation import assert_events, cancel_pending, validate_event


class TeamDeliveryContract(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.db.row_factory = sqlite3.Row
        self.addCleanup(self.db.close)
        self.runtime = SimpleNamespace(changed=threading.Event())
        self.db.executescript('''
            CREATE TABLE runtime_agents (id TEXT PRIMARY KEY,record TEXT);
            CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY,record TEXT);
            CREATE TABLE runtime_complaints (id TEXT PRIMARY KEY,record TEXT);
            CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY,record TEXT);
            CREATE TABLE runtime_events (id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,error TEXT);
            CREATE INDEX runtime_event_queue ON runtime_events(status,agent);
            CREATE TABLE runtime_chat_messages (id TEXT PRIMARY KEY,room TEXT,sender TEXT,deliveries TEXT);
        ''')
        for agent, root in [('lead', 'lead'), ('worker', 'lead'), ('peer', 'lead'), ('other', 'other')]:
            self.record('agents', agent, {'id': agent, 'rootId': root})
        self.record('rooms', 'private', {'kind': 'private', 'members': ['lead', 'worker']})
        self.record('rooms', 'cross', {'kind': 'private', 'members': ['other', 'worker']})
        self.record('rooms', 'broadcast:lead', {'kind': 'broadcast', 'rootId': 'lead'})
        self.record('rooms', 'broadcast:all', {'kind': 'broadcast', 'rootId': 'all'})

    def record(self, table, key, value):
        self.db.execute(f'INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)', (key, json.dumps(value)))

    def message(self, sender='lead', room='private', key='message'):
        return {'id': 'chat:' + key + ':worker', 'kind': 'agent_message',
                'text': json.dumps({'sender': sender, 'room': room, 'message_id': key, 'text': 'Keep this text'})}

    def reason(self, event, recipient=None):
        return validate_event(self.runtime, self.db, recipient or {'id': 'worker'}, event)

    def test_same_team_direct_and_broadcast_need_no_message_row_at_enqueue(self):
        self.assertIsNone(self.reason(self.message()))
        self.assertIsNone(self.reason(self.message(room='broadcast:lead')))
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_chat_messages').fetchone()[0], 0)

    def test_foreign_sender_and_global_channel_are_denied(self):
        for event in [self.message('other', 'cross'), self.message(room='broadcast:all'),
                      self.message('other', 'broadcast:lead')]:
            with self.subTest(event=event):
                self.assertIsNotNone(self.reason(event))
        # A stale or forged recipient object cannot override the stored root.
        self.assertIsNotNone(self.reason(self.message('other', 'cross'), {'id': 'worker', 'rootId': 'other'}))

    def test_room_cannot_hide_another_team_or_missing_member(self):
        for members in [['lead', 'worker', 'other'], ['lead', 'worker', 'missing'], ['lead', 'peer']]:
            self.record('rooms', 'private', {'kind': 'private', 'members': members})
            self.assertIsNotNone(self.reason(self.message()))

    def test_missing_or_malformed_sensitive_provenance_is_denied(self):
        for text in ['bad json', '[]', '{}', '{"sender": "lead", "room": "missing"}']:
            self.assertIsNotNone(self.reason({'id': 'bad', 'kind': 'agent_message', 'text': text}))
        self.assertIsNotNone(self.reason(self.message('missing')))
        self.assertIsNotNone(self.reason(self.message(), {'id': 'missing'}))
        self.assertIsNotNone(self.reason({'id': 'bad', 'kind': 'complaint_response',
                                         'text': '{"complaint_id": []}'}))

    def test_old_review_ids_resolve_real_team_participants(self):
        for target, recipient, allowed in [('lead', 'worker', True), ('other', 'worker', False),
                                           ('lead', 'peer', False), ('missing', 'worker', False)]:
            event = {'id': f'review:{target}:{recipient}:1:100.000000', 'kind': 'chat_review', 'text': 'Snapshot'}
            self.assertEqual(self.reason(event) is None, allowed)
        self.assertIsNotNone(self.reason({'id': 'review:bad', 'kind': 'chat_review', 'text': 'Snapshot'}))

    def test_followup_checks_new_sender_provenance_and_preserves_legacy(self):
        event = {'id': 'followup', 'kind': 'followup', 'text': 'Task'}
        self.assertIsNone(self.reason(event))
        for sender, allowed in [('lead', True), ('other', False), ('missing', False), (None, False)]:
            self.record('event_meta', event['id'], {'senderId': sender})
            self.assertEqual(self.reason(event) is None, allowed)

    def test_complaint_responses_allow_user_and_same_team_only(self):
        for sender, allowed in [('lead', True), ('other', False), ('user', True)]:
            self.record('complaints', 'complaint', {'author': 'worker', 'leadId': sender})
            payload = {'complaint_id': 'complaint', 'response': {'author': sender, 'text': 'Action'}}
            payload['responder' if sender == 'user' else 'lead'] = sender
            event = {'id': 'response', 'kind': 'complaint_response', 'text': json.dumps(payload)}
            self.assertEqual(self.reason(event) is None, allowed)
        payload['response']['author'] = 'other'
        self.assertIsNotNone(self.reason({**event, 'text': json.dumps(payload)}))
        self.record('complaints', 'complaint', {'author': 'peer', 'leadId': 'user'})
        self.assertIsNotNone(self.reason(event))

    def test_migration_cancels_pending_only_and_preserves_history(self):
        for status in ['pending', 'reserved', 'dispatching', 'uncertain', 'delivered', 'stored_only', 'cancelled']:
            event = self.message('other', 'cross', key=status)
            self.db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?)',
                            (event['id'], 'worker', event['kind'], event['text'], status, 'Original error'))
            self.db.execute('INSERT INTO runtime_chat_messages VALUES (?,?,?,?)',
                            (status, 'cross', 'other', json.dumps({'worker': 'queued', 'other': 'delivered'})))
        allowed = self.message(key='same-team')
        self.db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?)',
                        (allowed['id'], 'worker', allowed['kind'], allowed['text'], 'pending', None))
        before = {row['id']: dict(row) for row in self.db.execute('SELECT * FROM runtime_events')}
        self.assertEqual(cancel_pending(self.runtime, self.db), 1)
        self.assertTrue(self.runtime.changed.is_set())
        after = {row['id']: dict(row) for row in self.db.execute('SELECT * FROM runtime_events')}
        changed = after.pop('chat:pending:worker')
        original = before.pop('chat:pending:worker')
        self.assertEqual(after, before)
        self.assertEqual(changed['status'], 'cancelled')
        self.assertIn('one team', changed['error'])
        self.assertEqual(changed['text'], original['text'])
        for row in self.db.execute('SELECT * FROM runtime_chat_messages'):
            deliveries = json.loads(row['deliveries'])
            self.assertEqual(deliveries['worker'], 'cancelled' if row['id'] == 'pending' else 'queued')
            self.assertEqual(deliveries['other'], 'delivered')
        self.assertEqual(cancel_pending(self.runtime, self.db), 0)

    def test_batch_rechecks_after_team_change(self):
        events = [self.message(), {'id': 'user', 'kind': 'user', 'text': 'Continue'}]
        assert_events(self.runtime, self.db, {'id': 'worker'}, events)
        self.record('agents', 'lead', {'id': 'lead', 'rootId': 'other'})
        with self.assertRaisesRegex(ValueError, 'team'):
            assert_events(self.runtime, self.db, {'id': 'worker'}, events)
        self.assertIsNone(self.reason(events[1]))

    def test_malformed_message_is_cancelled_without_receipt_write(self):
        self.db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?)',
                        ('malformed', 'worker', 'agent_message', '{"message_id": []}', 'pending', None))
        self.assertEqual(cancel_pending(self.runtime, self.db), 1)
        self.assertEqual(self.db.execute('SELECT status FROM runtime_events').fetchone()[0], 'cancelled')


if __name__ == '__main__':
    unittest.main()
