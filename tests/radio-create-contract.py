#!/usr/bin/env python3
"""Direct shared chat creation uses validated accounts and one atomic commit."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location('radio_create_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_radio import manage, tick, _valid


class DirectCreate(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown

    def request(self, **changes):
        return dict(action='radio', radio_action='create', request_id=str(uuid.uuid4()),
                    path=str(self.project), name='Shared discussion', participants=[
                        {'account_key': 'default', 'model': 'gpt-6-astra'},
                        {'account_key': 'default', 'model': 'gpt-6-sol', 'effort': 'high'}]) | changes

    def count(self, table):
        with self.runtime.db() as db:
            return db.execute('SELECT count(*) FROM runtime_' + table).fetchone()[0]

    def test_creates_only_one_room_two_hidden_roots_without_starting(self):
        result = manage(self.runtime, self.request())
        room = result['room']
        self.assertTrue(room['radio']['direct'])
        self.assertEqual(self.count('rooms'), 1)
        self.assertEqual(self.count('agents'), 2)
        self.assertEqual(self.count('events'), 0)
        with self.runtime.db() as db:
            self.assertIsNone(_valid(self.runtime, db, room, ready=True))
            for key in room['members']:
                a = self.runtime.agent(key, db)
                self.assertEqual(a['sharedRoomId'], room['id'])
                self.assertEqual(a['rootId'], key)
                self.assertIsNone(a['threadId'])
                self.assertTrue(a['autoWake'])
            project = json.loads(db.execute('SELECT record FROM runtime_projects').fetchone()[0])
            self.assertFalse(project.get('peerTeams'))
        self.runtime.dispatch()
        self.assertFalse(any(method == 'turn/start' for method, _ in self.runtime.server.calls))

    def test_replay_works_without_catalog_and_changed_body_rejected(self):
        request = self.request()
        first = manage(self.runtime, request)
        with patch.object(self.runtime, 'catalog', side_effect=AssertionError('Must not read catalogs on replay')):
            self.assertEqual(manage(self.runtime, request), first)
            with self.assertRaisesRegex(ValueError, 'different content'):
                manage(self.runtime, request | {'name': 'Changed'})
        self.assertEqual(self.count('agents'), 2)

    def test_invalid_second_model_leaves_no_records(self):
        request = self.request()
        request['participants'][1]['model'] = 'not-a-model'
        with self.assertRaises(ValueError):
            manage(self.runtime, request)
        for table in ('agents', 'rooms', 'events', 'projects', 'operation_receipts'):
            self.assertEqual(self.count(table), 0)

    def test_mid_create_account_change_rolls_back_all_records(self):
        original = self.runtime.create
        count = 0
        def prepare(*args, **kwargs):
            nonlocal count
            result = original(*args, **kwargs)
            count += 1
            if count == 2:
                original_get = self.runtime.accounts.get
                self.runtime.accounts.get = lambda key: original_get(key) | {'disconnected': True}
            return result
        with patch.object(self.runtime, 'create', side_effect=prepare):
            with self.assertRaisesRegex(ValueError, 'account changed'):
                manage(self.runtime, self.request())
        self.assertEqual(self.count('agents'), 0)
        self.assertEqual(self.count('rooms'), 0)

    def test_direct_send_starts_and_membership_change_blocks(self):
        room = manage(self.runtime, self.request())['room']
        manage(self.runtime, dict(action='radio', radio_action='send', path=str(self.project),
            team_id=room['radio']['teamId'], request_id=str(uuid.uuid4()), expected_revision=0, text='Discuss this'))
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(room['members'][0]).get('turnId'))
        with self.runtime.lock, self.runtime.db() as db:
            a = self.runtime.agent(room['members'][1], db)
            a['sharedRoomId'] = 'another-room'
            self.runtime.put(db, 'agents', a)
            self.assertIsNotNone(_valid(self.runtime, db, room))
            tick(self.runtime, db)
            saved = json.loads(db.execute('SELECT record FROM runtime_rooms WHERE id=?', (room['id'],)).fetchone()[0])
            self.assertEqual(saved['radio']['status'], 'blocked')


if __name__ == '__main__':
    unittest.main()
