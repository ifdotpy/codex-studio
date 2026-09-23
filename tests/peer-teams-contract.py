#!/usr/bin/env python3
"""Peer teams use project records and grant only same-project lead membership."""

from contextlib import contextmanager

import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import unittest
import uuid

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_accounts import AccountStore
from codex_peer_teams import manage, peer_pair_allowed, peers_for, snapshot
from codex_work import WorkMixin
from codex_workspace import WorkspaceMixin


class Store(WorkMixin, WorkspaceMixin):
    def __init__(self, root):
        self.root = root
        self.lock = threading.RLock()
        self.accounts = AccountStore(root)
        with self.db() as db:
            for table in ('agents', 'projects', 'events', 'tasks', 'messages'):
                db.execute(f'CREATE TABLE runtime_{table} (id TEXT PRIMARY KEY, record TEXT NOT NULL)')
            db.execute('CREATE TABLE runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT, result TEXT)')

    @contextmanager
    def db(self):
        db = sqlite3.connect(self.root / 'test.sqlite3')
        db.row_factory = sqlite3.Row
        try:
            with db:
                yield db
        finally:
            db.close()

    @staticmethod
    def put(db, table, record):
        db.execute(f'INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)',
                   (record['id'], json.dumps(record)))

    @staticmethod
    def records(db, table):
        return [json.loads(row[0]) for row in db.execute(f'SELECT record FROM runtime_{table}')]


class PeerTeams(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.path = str(self.root / 'one' / 'project')
        self.other = str(self.root / 'two' / 'project')
        self.runtime = Store(self.root)
        self.team_id = str(uuid.uuid4())
        with self.runtime.db() as db:
            for key in ('a', 'b', 'c', 'd'):
                self.runtime.put(db, 'agents', {
                    'id': key, 'isLead': True, 'parentId': None, 'rootId': key,
                    'cwd': self.path, 'threadId': 'native-' + key,
                    'accountKey': 'default', 'history': ['original'], 'autoWake': True})
            self.runtime.ensure_project(self.path, 'default', db)

    def request(self, **changes):
        return dict(action='save', path=self.path, team_id=self.team_id,
                    expected_revision=0, request_id=str(uuid.uuid4()),
                    name='Team', members=['a', 'b']) | changes

    def mutate_agent(self, key, **changes):
        with self.runtime.db() as db:
            record = next(a for a in self.runtime.records(db, 'agents') if a['id'] == key)
            self.runtime.put(db, 'agents', record | changes)

    def assert_denied(self, left='a', right='b'):
        with self.runtime.db() as db:
            self.assertFalse(peer_pair_allowed(db, left, right))

    def test_save_preserves_chats_and_does_not_start_work(self):
        with self.runtime.db() as db:
            before = self.runtime.records(db, 'agents')
        result = manage(self.runtime, self.request())
        self.assertEqual(result['peerTeamsRevision'], 1)
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, 'agents'), before)
            for table in ('tasks', 'events', 'messages'):
                self.assertEqual(self.runtime.records(db, table), [])
            self.assertTrue(peer_pair_allowed(db, 'a', 'b'))
            self.assertTrue(peer_pair_allowed(db, 'b', 'a'))
            self.assertFalse(peer_pair_allowed(db, 'a', 'a'))
            self.assertFalse(peer_pair_allowed(db, 'a', 'c'))
            self.assertEqual([p['id'] for p in peers_for(self.runtime, db, 'a')], ['b'])
            self.assertEqual(snapshot(self.runtime, db), [dict(
                id=self.team_id, name='Team', projectPath=self.path, members=['a', 'b'], revision=1)])

    def test_receipt_replay_after_later_edit_and_changed_payload(self):
        request = self.request()
        result = manage(self.runtime, request)
        manage(self.runtime, self.request(expected_revision=1, name='Changed'))
        self.assertEqual(manage(self.runtime, request), result)
        for changed in (dict(name='Other'), dict(expected_revision=2), dict(members=['b', 'a'])):
            with self.assertRaisesRegex(ValueError, 'different content'):
                manage(self.runtime, request | changed)
        with self.runtime.db() as db:
            project = self.runtime.records(db, 'projects')[0]
            self.assertEqual(project['peerTeams'][0]['name'], 'Changed')
            self.assertEqual(project['peerTeamsRevision'], 2)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_operation_receipts').fetchone()[0], 2)

    def test_stale_revision_rolls_back(self):
        manage(self.runtime, self.request())
        with self.assertRaisesRegex(ValueError, 'Reload'):
            manage(self.runtime, self.request(name='Stale'))
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, 'projects')[0]['peerTeamsRevision'], 1)
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_operation_receipts').fetchone()[0], 1)

    def test_one_team_per_lead(self):
        manage(self.runtime, self.request())
        with self.assertRaisesRegex(ValueError, 'another peer team'):
            manage(self.runtime, self.request(team_id=str(uuid.uuid4()), members=['b', 'c'], expected_revision=1))
        manage(self.runtime, self.request(members=['a', 'c'], expected_revision=1))
        self.assert_denied()
        with self.runtime.db() as db:
            self.assertTrue(peer_pair_allowed(db, 'a', 'c'))

    def test_delete_dissolves_only_membership_and_replays(self):
        manage(self.runtime, self.request())
        with self.runtime.db() as db:
            before = self.runtime.records(db, 'agents')
        request = self.request(action='delete', expected_revision=1)
        result = manage(self.runtime, request)
        self.assertEqual(manage(self.runtime, request), result)
        self.assertEqual(result['peerTeams'], [])
        self.assert_denied()
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, 'agents'), before)
            self.assertEqual(snapshot(self.runtime, db), [])

    def test_reject_invalid_members_and_inputs(self):
        for members in ([], ['a'], ['a', 'a'], ['a', 'missing'], ['a', {}], 'a,b'):
            with self.subTest(members=members), self.assertRaises(ValueError):
                manage(self.runtime, self.request(members=members))
        for changes in (dict(name=''), dict(name='x' * 81), dict(expected_revision=True),
                        dict(expected_revision=-1), dict(team_id='bad'), dict(request_id=''),
                        dict(action='unknown')):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                manage(self.runtime, self.request(**changes))

    def test_children_deleted_and_other_project_cannot_join(self):
        for changes in (dict(isLead=False, parentId='a', rootId='a'),
                        dict(parentId='a'), dict(rootId='a'), dict(deletedAt=1),
                        dict(cwd=self.other)):
            with self.subTest(changes=changes):
                self.mutate_agent('b', isLead=True, parentId=None, rootId='b', deletedAt=None, cwd=self.path)
                self.mutate_agent('b', **changes)
                with self.assertRaisesRegex(ValueError, 'live lead chats'):
                    manage(self.runtime, self.request())
                self.assert_denied()

    def test_live_membership_rechecks_delete_move_and_child(self):
        manage(self.runtime, self.request(members=['a', 'b', 'c']))
        for changes in (dict(deletedAt=1), dict(cwd=self.other), dict(isLead=False, rootId='a', parentId='a')):
            with self.subTest(changes=changes):
                self.mutate_agent('b', isLead=True, parentId=None, rootId='b', deletedAt=None, cwd=self.path)
                self.mutate_agent('b', **changes)
                self.assert_denied()
                with self.runtime.db() as db:
                    self.assertEqual(snapshot(self.runtime, db)[0]['members'], ['a', 'c'])
        self.mutate_agent('c', deletedAt=1)
        with self.runtime.db() as db:
            self.assertEqual(snapshot(self.runtime, db), [])
            self.assertEqual(peers_for(self.runtime, db, 'a'), [])

    def test_inactive_team_does_not_block_regrouping_or_restore_membership(self):
        for change in (dict(deletedAt=1), dict(cwd=self.other)):
            with self.subTest(change=change):
                self.setUp()
                manage(self.runtime, self.request())
                self.mutate_agent('b', **change)
                new_id = str(uuid.uuid4())
                result = manage(self.runtime, self.request(
                    team_id=new_id, members=['a', 'c'], expected_revision=1))
                self.assertEqual([t['id'] for t in result['peerTeams']], [new_id])
                self.mutate_agent('b', deletedAt=None, cwd=self.path)
                self.assert_denied()
                with self.runtime.db() as db:
                    self.assertTrue(peer_pair_allowed(db, 'a', 'c'))
                    self.assertEqual(snapshot(self.runtime, db)[0]['members'], ['a', 'c'])

    def test_mutation_removes_invalid_member_from_surviving_team(self):
        manage(self.runtime, self.request(members=['a', 'b', 'c']))
        self.mutate_agent('b', cwd=self.other)
        result = manage(self.runtime, self.request(action='delete',
            team_id=str(uuid.uuid4()), expected_revision=1))
        self.assertEqual(result['peerTeams'][0]['members'], ['a', 'c'])
        self.mutate_agent('b', cwd=self.path)
        self.assert_denied()

    def test_drag_move_is_atomic_and_replay_safe(self):
        manage(self.runtime, self.request())
        other = str(uuid.uuid4())
        manage(self.runtime, self.request(team_id=other, members=['c', 'd'], expected_revision=1))
        move = self.request(action='move', member='a', team_id=other, expected_revision=2)
        result = manage(self.runtime, move)
        self.assertEqual(result['peerTeamsRevision'], 3)
        self.assertEqual(result['peerTeams'], [{'id': other, 'name': 'Team', 'members': ['c', 'd', 'a']}])
        self.assertEqual(manage(self.runtime, move), result)
        with self.assertRaises(ValueError):
            manage(self.runtime, move | {'member': 'b'})
        with self.runtime.db() as db:
            self.assertTrue(peer_pair_allowed(db, 'a', 'c'))
            self.assertFalse(peer_pair_allowed(db, 'a', 'b'))

    def test_drag_remove_preserves_remaining_team_and_revokes_access(self):
        manage(self.runtime, self.request(members=['a', 'b', 'c']))
        result = manage(self.runtime, self.request(action='move', member='a', team_id=None, expected_revision=1))
        self.assertEqual(result['peerTeams'][0]['members'], ['b', 'c'])
        self.assert_denied()
        with self.runtime.db() as db:
            self.assertTrue(peer_pair_allowed(db, 'b', 'c'))

    def test_drag_rejects_foreign_project_worker_stale_or_missing_target(self):
        manage(self.runtime, self.request())
        move = self.request(action='move', member='c', expected_revision=1)
        for change in ({'expected_revision': 0}, {'team_id': str(uuid.uuid4())}):
            with self.assertRaises(ValueError):
                manage(self.runtime, move | change)
        self.mutate_agent('c', cwd=self.other)
        with self.assertRaises(ValueError): manage(self.runtime, move)
        self.mutate_agent('c', cwd=self.path, isLead=False, parentId='a')
        with self.assertRaises(ValueError): manage(self.runtime, move)
        with self.runtime.db() as db:
            self.assertEqual(snapshot(self.runtime, db)[0]['members'], ['a', 'b'])

    def test_canonical_project_path(self):
        alias = self.root / 'alias'
        Path(self.path).mkdir(parents=True)
        alias.symlink_to(self.path, target_is_directory=True)
        self.mutate_agent('b', cwd=str(alias))
        result = manage(self.runtime, self.request(path=str(alias)))
        self.assertEqual(result['path'], self.path)
        with self.runtime.db() as db:
            self.assertTrue(peer_pair_allowed(db, 'a', 'b'))


if __name__ == '__main__':
    unittest.main()
