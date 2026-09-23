#!/usr/bin/env python3
"""Peer teams grant explicit chat access without sharing work or descendants."""

import importlib.util
from pathlib import Path
import unittest
import uuid

spec = importlib.util.spec_from_file_location('peer_message_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_peer_teams import manage
from codex_team_isolation import cancel_pending, validate_event


class PeerTeamMessages(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update
    events = f.WorkspaceContract.events
    work = f.WorkspaceContract.work

    def group(self, *members):
        self.team_id = str(uuid.uuid4())
        return manage(self.runtime, {'action': 'save', 'path': str(self.project),
            'team_id': self.team_id, 'name': 'Peers', 'members': [a['id'] for a in members],
            'expected_revision': 0, 'request_id': str(uuid.uuid4())})

    def ungroup(self):
        return manage(self.runtime, {'action': 'delete', 'path': str(self.project),
            'team_id': self.team_id, 'expected_revision': 1, 'request_id': str(uuid.uuid4())})

    def pair(self):
        left = self.agent_update(self.lead('Left'), draft=False, prompt='Independent left task', status='idle')
        right = self.agent_update(self.lead('Right'), draft=False, prompt='Independent right task', status='idle')
        self.group(left, right)
        return left, right

    def test_grouping_preserves_work_and_does_not_deliver_events(self):
        left, right = self.lead('Left'), self.lead('Right')
        child = self.worker(left)
        before = {a['id']: self.runtime.agent(a['id']) for a in (left, right, child)}
        events = {key: self.events(a) for key, a in before.items()}
        self.group(left, right)
        for key, agent in before.items():
            self.assertEqual(self.runtime.agent(key), agent)
            self.assertEqual(self.events(agent), events[key])

    def test_explicit_peer_message_is_readable_and_authorized_for_delivery(self):
        left, right = self.pair()
        receipt = self.runtime.chat_message(left['id'], right['id'], 'Can you check this interface?', 'peer-message')
        self.assertEqual(receipt['deliveries'], {right['id']: 'queued'})
        for viewer in (left, right):
            page = self.runtime.chat_read(receipt['room'], viewer['id'])
            self.assertEqual(page['messages'][0]['text'], 'Can you check this interface?')
        with self.runtime.db() as db:
            event = db.execute('SELECT * FROM runtime_events WHERE id=?', ('chat:peer-message:' + right['id'],)).fetchone()
            self.assertIsNotNone(event)
            self.assertIsNone(validate_event(self.runtime, db, right, event))
            room = next(r for r in self.runtime.chat_rooms(db, left['id']) if r['id'] == receipt['room'])
            self.assertEqual(room['peerTeamId'], self.team_id)
            self.assertEqual(room['peerTeamName'], 'Peers')

    def test_discovery_exposes_peer_leads_but_not_their_children(self):
        left, right = self.pair()
        own, foreign = self.worker(left, 'Own'), self.worker(right, 'Foreign')
        expected = {left['id'], right['id'], own['id']}
        self.assertEqual({a['id'] for a in self.runtime.peers(left['id'])['peers']}, expected)
        directory = self.runtime.model_directory(left['id'], 'orchestration_peers', {})
        self.assertEqual({a['id'] for a in directory['items']}, expected)
        self.assertNotIn(left['id'], {a['id'] for a in self.runtime.peers(foreign['id'])['peers']})
        for sender, target in ((left, foreign), (own, right)):
            with self.assertRaises(ValueError):
                self.runtime.chat_message(sender['id'], target['id'], 'Forbidden', str(uuid.uuid4()))

    def test_revocation_blocks_reads_sends_and_pending_delivery(self):
        left, right = self.pair()
        receipt = self.runtime.chat_message(left['id'], right['id'], 'Pending question', 'revoke')
        self.ungroup()
        with self.assertRaises(ValueError):
            self.runtime.chat_read(receipt['room'], right['id'])
        with self.assertRaises(ValueError):
            self.runtime.chat_message(left['id'], right['id'], 'No longer allowed', 'revoked-send')
        with self.runtime.lock, self.runtime.db() as db:
            cancel_pending(self.runtime, db)
            event = db.execute('SELECT status FROM runtime_events WHERE id=?', ('chat:revoke:' + right['id'],)).fetchone()
            self.assertEqual(event['status'], 'cancelled')
        page = self.runtime.chat_read(receipt['room'])
        self.assertEqual(page['messages'][0]['text'], 'Pending question')
        self.assertEqual(page['messages'][0]['deliveries'][right['id']], 'cancelled')

    def test_project_move_revokes_peer_permission(self):
        left, right = self.pair()
        receipt = self.runtime.chat_message(left['id'], right['id'], 'Before move', 'move')
        elsewhere = self.root / 'elsewhere'
        elsewhere.mkdir()
        self.agent_update(right, cwd=str(elsewhere))
        with self.assertRaises(ValueError):
            self.runtime.chat_read(receipt['room'], right['id'])
        with self.assertRaises(ValueError):
            self.runtime.chat_message(left['id'], right['id'], 'After move', 'after-move')
        with self.runtime.lock, self.runtime.db() as db:
            cancel_pending(self.runtime, db)
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?',
                ('chat:move:' + right['id'],)).fetchone()[0], 'cancelled')

    def test_peer_membership_does_not_grant_work_or_followup_authority(self):
        left, right = self.pair()
        task = self.work(right, 'Private work')
        with self.assertRaises(ValueError):
            self.runtime.work_action(right['id'], {'action': 'list'}, actor=left['id'])
        self.assertEqual(self.runtime.work_action(left['id'], {'action': 'list'}, actor=left['id'])['items'], [])
        with self.assertRaises(ValueError):
            self.runtime.work_action(left['id'], {'action': 'claim', 'task_id': task['id']}, actor=left['id'])
        with self.assertRaises(ValueError):
            self.runtime.send(right['id'], 'Replace your work', 'forbidden-followup', manual=False,
                resume=True, sender=left['id'], sender_epoch=left['epoch'])

    def test_broadcast_and_child_results_keep_original_scope(self):
        left, right = self.pair()
        own, foreign = self.worker(left, 'Own'), self.worker(right, 'Foreign')
        receipt = self.runtime.chat_message(left['id'], 'broadcast', 'Our own work', 'own-broadcast')
        self.assertEqual(set(receipt['deliveries']), {own['id']})
        with self.assertRaises(ValueError):
            self.runtime.chat_read(receipt['room'], right['id'])
        before = self.events(left)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.parent_event(db, foreign, 'done', 'Private child result')
        self.assertEqual(self.events(left), before)
        self.assertEqual(len(self.events(right, 'child_result')), 1)


if __name__ == '__main__':
    unittest.main()
