#!/usr/bin/env python3
"""Idle peer conversion through the user action, with real runtime sync writes."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
import uuid

sys.dont_write_bytecode = True
root = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_peer_teams import manage, peer_pair_allowed
from codex_runtime import Runtime
spec = importlib.util.spec_from_file_location('runtime_fixture', SERVER_TESTS_ROOT / 'runtime-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.1)
            self.changed.clear()


class Conversion(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = str(Path(self.tmp.name).resolve())
        self.rt = QuietRuntime(Path(self.path), fixture.FakeServer)
        self.addCleanup(self.rt.close)
        self.a = self.rt.create({'name': 'Source', 'cwd': self.path, 'prompt': 'Source task'}, defer=True)
        self.b = self.rt.create({'name': 'Peer', 'cwd': self.path, 'prompt': 'Peer task'}, defer=True)
        self.c = self.rt.create({'name': 'Destination', 'cwd': self.path, 'prompt': 'Target task'}, defer=True)
        self.team = str(uuid.uuid4())
        manage(self.rt, {'action': 'save', 'path': self.path, 'team_id': self.team, 'members': [self.a['id'], self.b['id']],
                         'name': 'Peers', 'expected_revision': 0, 'request_id': str(uuid.uuid4())})
        self.body = {'action': 'convert', 'path': self.path, 'member': self.a['id'], 'target': self.c['id'],
                     'expected_revision': 1, 'request_id': str(uuid.uuid4())}

    def change(self, key, **fields):
        with self.rt.lock, self.rt.db() as db:
            a = self.rt.agent(key, db)
            a.update(fields)
            self.rt.put(db, 'agents', a)

    def set_concurrency(self, key, limit):
        current = self.rt.agent(key)
        return self.rt.conversation_settings(key, {'subagent_concurrency': limit,
            'expected_mode_revision': current['agentModeRevision'], 'request_id': str(uuid.uuid4())})

    def test_move_preserves_threads_settings_descendants_board_rooms_and_progress(self):
        self.change(self.a['id'], autoWake=True)
        self.change(self.c['id'], autoWake=True)
        worker = self.rt.create({'name': 'Child', 'prompt': 'Child task', 'role': 'reviewer'}, parent=self.a['id'], defer=True)
        self.change(worker['id'], autoWake=True)
        grandchild = self.rt.create({'name': 'Grandchild', 'prompt': 'Grandchild task', 'role': 'reviewer'}, parent=worker['id'], defer=True)
        for key in (self.a['id'], worker['id'], grandchild['id']):
            self.change(key, threadId='native-' + key)
        progress = self.rt.progress_file(self.a)
        progress.write_text('The saved progress.\n')
        with self.rt.lock, self.rt.db() as db:
            self.rt.item(db, self.a['id'], 'history', 'assistant', 'The original report', 'Lead')
            before = {a['id']: a for a in self.rt.records(db, 'agents')}
            work = {'id': 'board', 'rootId': self.a['id'], 'owner': worker['id'], 'status': 'review',
                    'title': 'Original board', 'dependencies': ['dependency'], 'version': 4, 'results': [{'text': 'Saved evidence'}]}
            self.rt.put(db, 'work', work)
            self.rt.put(db, 'rooms', {'id': 'broadcast:' + self.a['id'], 'rootId': self.a['id'], 'kind': 'broadcast', 'updated': 1, 'members': []})
            self.rt.put(db, 'rooms', {'id': 'peer-history', 'kind': 'private', 'updated': 1, 'members': [self.a['id'], self.b['id']]})
            self.rt.put(db, 'monitors', {'id': 'monitor', 'agent': worker['id'], 'status': 'completed', 'created': 1})
        result = manage(self.rt, self.body)
        self.assertEqual(set(result['movedAgents']), {self.a['id'], worker['id'], grandchild['id']})
        self.assertEqual(progress.read_text(), 'The saved progress.\n')
        with self.rt.db() as db:
            a = self.rt.agent(self.a['id'], db)
            self.assertFalse(a['isLead'])
            self.assertFalse(a['autoWake'])
            self.assertEqual((a['role'], a['parentId'], a['rootId']), ('implementer', self.c['id'], self.c['id']))
            for key in (self.a['id'], worker['id'], grandchild['id']):
                after = self.rt.agent(key, db)
                self.assertEqual(after['rootId'], self.c['id'])
                for field in ('threadId', 'accountKey', 'model', 'effort', 'cwd'):
                    self.assertEqual(after.get(field), before[key].get(field))
                if key != self.a['id']:
                    self.assertEqual(after['parentId'], before[key]['parentId'])
                payload = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?", (key,)).fetchone()[0])['value']
                self.assertEqual(payload['rootId'], self.c['id'])
            moved_work = self.rt.work_records(db, self.c['id'])[0]
            self.assertEqual(moved_work, work | {'rootId': self.c['id']})
            self.assertFalse(peer_pair_allowed(db, self.a['id'], self.b['id']))
            self.assertEqual(db.execute("SELECT deleted FROM sync_entities WHERE collection='peerTeam' AND id=?", (self.team,)).fetchone()[0], 1)
            rooms = {r['id']: r for r in self.rt.chat_rooms(db)}
            self.assertIn('peer-history', rooms)
            self.assertEqual(self.rt.chat_rooms(db, viewer=self.a['id'], room_id='peer-history'), [])
            broadcast = next(r for r in rooms.values() if r['id'] == 'broadcast:' + self.a['id'])
            self.assertEqual(broadcast['kind'], 'private')
            self.assertEqual(set(broadcast['members']), set(result['movedAgents']))
            self.assertEqual(self.rt.records(db, 'monitors')[0]['agent'], worker['id'])
            self.assertEqual(json.loads(db.execute("SELECT record FROM runtime_items WHERE agent=? AND json_extract(record,'$.text')=?", (self.a["id"], "The original report")).fetchone()[0])["text"], 'The original report')

    def test_conversion_tombstones_private_room_with_deleted_member(self):
        deleted = self.rt.create({'name': 'Deleted member', 'cwd': self.path, 'prompt': 'Unused'}, defer=True)
        with self.rt.lock, self.rt.db() as db:
            self.rt.put(db, 'rooms', {'id': 'unavailable-private', 'rootId': self.a['id'],
                'kind': 'private', 'updated': 1, 'members': [deleted['id']]})
        self.change(deleted['id'], deletedAt=1)
        manage(self.rt, self.body)
        with self.rt.db() as db:
            row = db.execute(
                "SELECT deleted FROM sync_entities WHERE collection='room' AND id='unavailable-private'"
            ).fetchone()
            self.assertIsNotNone(row)
            self.assertEqual(row[0], 1)

    def test_busy_descendants_commands_permissions_watches_and_transfer_records(self):
        self.change(self.a['id'], autoWake=True)
        worker = self.rt.create({'name': 'Child', 'prompt': 'Child', 'role': 'reviewer'}, parent=self.a['id'], defer=True)
        self.change(worker['id'], inFlight=True)
        with self.assertRaisesRegex(ValueError, 'Child.*turn'):
            manage(self.rt, self.body)
        self.change(worker['id'], inFlight=False)
        rows = [
            ('tasks', {'id': 'busy', 'agent': worker['id'], 'status': 'running', 'created': 1}, 'commands'),
            ('monitors', {'id': 'busy', 'agent': worker['id'], 'status': 'running', 'created': 1}, 'monitors'),
            ('requests', {'id': 'busy', 'agent': self.c['id'], 'status': 'pending'}, 'permissions'),
            ('rules', {'id': 'busy', 'agent': worker['id'], 'inFlight': True}, 'watch'),
            ('tool_requests', {'id': 'busy', 'agent': self.a['id'], 'outcome': 'unknown', 'readOnly': False}, 'tool request'),
        ]
        for table, record, reason in rows:
            with self.subTest(table=table):
                with self.rt.db() as db:
                    db.execute('INSERT INTO runtime_' + table + ' VALUES (?,?)', ('busy', json.dumps(record)))
                with self.assertRaisesRegex(ValueError, reason):
                    manage(self.rt, self.body)
                with self.rt.db() as db:
                    db.execute('DELETE FROM runtime_' + table + " WHERE id='busy'")
        with self.rt.db() as db:
            db.execute('CREATE TABLE IF NOT EXISTS runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
            db.execute('INSERT INTO runtime_account_transfers VALUES (?,?)', ('busy', json.dumps({
                'id': 'busy', 'status': 'pending', 'leadId': self.c['id'], 'members': {}})))
        with self.assertRaisesRegex(ValueError, 'account transfer'):
            manage(self.rt, self.body)

    def test_three_member_team_survives_and_watches_pause(self):
        manage(self.rt, {'action': 'save', 'path': self.path, 'team_id': self.team,
            'members': [self.a['id'], self.b['id'], self.c['id']], 'name': 'Peers',
            'expected_revision': 1, 'request_id': str(uuid.uuid4())})
        self.body['expected_revision'] = 2
        with self.rt.db() as db:
            self.rt.put(db, 'rules', {'id': 'watch', 'agent': self.a['id'], 'rootId': self.a['id'],
                                    'epoch': self.a['epoch'], 'status': 'active', 'kind': 'event'})
            self.rt.put(db, 'plans', {'id': self.a['id'], 'rootId': self.a['id'], 'text': 'Saved plan'})
            self.rt.put(db, 'complaints', {'id': 'question', 'leadId': self.a['id'], 'author': self.a['id']})
        manage(self.rt, self.body)
        with self.rt.db() as db:
            project = self.rt.projects(db=db)['items'][0]
            self.assertEqual(project['peerTeams'][0]['members'], [self.b['id'], self.c['id']])
            self.assertTrue(peer_pair_allowed(db, self.b['id'], self.c['id']))
            self.assertFalse(peer_pair_allowed(db, self.a['id'], self.b['id']))
            watch = self.rt.records(db, 'rules')[0]
            self.assertEqual((watch['rootId'], watch['status'], watch['epoch']),
                             (self.c['id'], 'paused', self.a['epoch'] + 1))
            self.assertEqual(self.rt.records(db, 'plans')[0]['text'], 'Saved plan')
            self.assertEqual(self.rt.records(db, 'plans')[0]['rootId'], self.c['id'])
            self.assertEqual(self.rt.records(db, 'complaints')[0]['leadId'], self.c['id'])
            payload = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='peerTeam' AND id=?", (self.team,)).fetchone()[0])['value']
            self.assertEqual(payload['members'], [self.b['id'], self.c['id']])
            self.assertEqual(payload['revision'], 3)


    def test_retry_saved_identity_after_lost_response_and_changed_body(self):
        first = manage(self.rt, self.body)
        self.assertEqual(manage(self.rt, dict(self.body)), first)
        self.assertEqual(manage(self.rt, dict(self.body)), first)
        for field, value in [('target', self.b['id']), ('member', self.b['id']), ('expected_revision', 2)]:
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, 'different content'):
                manage(self.rt, self.body | {field: value})
        with self.rt.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'peer-convert:%'").fetchone()[0], 1)
            self.assertEqual(self.rt.agent(self.a['id'], db)['epoch'], self.a['epoch'] + 1)

    def test_saved_room_with_source_in_third_position_is_refreshed(self):
        with self.rt.db() as db:
            self.rt.put(db, 'rooms', {'id': 'long-room', 'kind': 'private', 'updated': 1,
                'members': [self.b['id'], self.c['id'], self.a['id']], 'radio': {
                    'teamId': 'radio-team', 'revision': 0, 'status': 'idle', 'speaker': None,
                    'next': [], 'active': {'identity': [None, None, 1], 'eventId': 'radio-event',
                        'agentId': self.b['id'], 'epoch': 0, 'threadId': 'thread', 'through': 0},
                    'error': None}})
        with self.assertRaisesRegex(ValueError, 'shared chat exchange'):
            manage(self.rt, self.body)
        with self.rt.db() as db:
            db.execute("UPDATE runtime_rooms SET record=json_remove(record,'$.radio') WHERE id='long-room'")
        manage(self.rt, self.body)
        with self.rt.db() as db:
            payload = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='room' AND id='long-room'").fetchone()[0])['value']
            self.assertEqual(payload['members'], [self.b['id'], self.c['id'], self.a['id']])

    def test_permission_and_membership_refusals(self):
        for body in (self.body | {'member': self.c['id'], 'target': self.a['id']},
                     self.body | {'member': self.a['id'], 'target': self.a['id']},
                     self.body | {'actor': self.b['id']}, self.body | {'expected_revision': 0}):
            with self.subTest(body=body), self.assertRaises(ValueError):
                manage(self.rt, body)
        self.set_concurrency(self.c['id'], 0)
        with self.assertRaisesRegex(ValueError, 'Single agent'):
            manage(self.rt, self.body)
        self.set_concurrency(self.c['id'], 32)
        self.change(self.c['id'], deletedAt=1)
        with self.assertRaisesRegex(ValueError, 'deleted'):
            manage(self.rt, self.body)
        self.change(self.c['id'], deletedAt=None, cwd=self.path + '/other')
        with self.assertRaisesRegex(ValueError, 'this project'):
            manage(self.rt, self.body)

    def test_busy_refuses_both_trees_and_rolls_back(self):
        for key in (self.a['id'], self.c['id']):
            for fields, reason in (({'inFlight': True}, 'turn'), ({'turnId': 'active'}, 'turn'),
                                   ({'workspaceOperation': 'pending'}, 'workspace'),
                                   ({'accountTransferId': 'transfer'}, 'transfer'),
                                   ({'accountTransfer': {'status': 'pending'}}, 'transfer')):
                with self.subTest(key=key, fields=fields):
                    self.change(key, **fields)
                    with self.assertRaisesRegex(ValueError, reason):
                        manage(self.rt, self.body)
                    self.change(key, **{f: None for f in fields})
        with self.rt.db() as db:
            self.assertTrue(self.rt.agent(self.a['id'], db)['isLead'])
            self.assertEqual(self.rt.projects(db=db)['items'][0]['peerTeamsRevision'], 1)
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'peer-convert:%'").fetchone()[0], 0)

    def test_pending_operation_and_input_refuse(self):
        with self.rt.lock, self.rt.db() as db:
            self.rt._put_workspace_operation(db, {'id': 'pending', 'agent': self.c['id'], 'phase': 'capture_pending'})
        with self.assertRaisesRegex(ValueError, 'workspace'):
            manage(self.rt, self.body)
        self.change(self.a['id'], autoWake=True)
        with self.rt.db() as db:
            db.execute('DELETE FROM runtime_workspace_operations')
            self.rt.enqueue(db, self.rt.agent(self.a['id'], db), 'user', 'Pending input', 'pending-input')
        with self.assertRaisesRegex(ValueError, 'active or queued turn'):
            manage(self.rt, self.body)


if __name__ == '__main__':
    unittest.main()
