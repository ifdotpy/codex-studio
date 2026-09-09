#!/usr/bin/env python3
"""Inputs survive workspace reservations; accepted decisions keep their history."""
import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('complaint_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class CoordinationComplaints(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    agent_update = f.WorkspaceContract.agent_update
    worker = f.WorkspaceContract.worker
    work = f.WorkspaceContract.work
    action = f.WorkspaceContract.action

    def test_each_delivery_waits_for_reservation_without_duplicate_input(self):
        for delivery in ('queue', 'steer', 'after_tool'):
            with self.subTest(delivery=delivery):
                a = self.lead(delivery)
                a = self.agent_update(a, workspaceOperation='checkpoint')
                key = 'message-' + delivery
                result = self.runtime.send(a['id'], 'Continue exact task', key, delivery=delivery)
                self.assertEqual(result['status'], 'queued')
                self.assertEqual(result['waitingFor'], [{'agentId': a['id'], 'operation': 'checkpoint'}])
                self.runtime.dispatch()
                self.assertFalse(self.runtime.agent(a['id'])['inFlight'])
                self.runtime.send(a['id'], 'Continue exact task', key, delivery=delivery)
                with self.runtime.db() as db:
                    self.assertEqual(db.execute('SELECT COUNT(*) FROM runtime_events WHERE id=?', (key,)).fetchone()[0], 1)
                with self.assertRaisesRegex(ValueError, 'different content'):
                    self.runtime.send(a['id'], 'Different task', key, delivery=delivery)
                self.agent_update(a, workspaceOperation=None)
                self.runtime.dispatch()
                f.eventually(lambda: self.runtime.agent(a['id']).get('turnId'))
                def delivered():
                    with self.runtime.db() as db:
                        return db.execute('SELECT status FROM runtime_events WHERE id=?', (key,)).fetchone()[0] == 'delivered'
                f.eventually(delivered)

    def test_checkpoint_created_during_preparation_requeues_unsubmitted_batch(self):
        a = self.lead()
        self.runtime.send(a['id'], 'Original input', 'race-input')
        original = self.runtime.prepare
        def reserve(agent):
            prepared = original(agent)
            self.agent_update(prepared, workspaceOperation='checkpoint')
            return prepared
        with patch.object(self.runtime, 'prepare', side_effect=reserve):
            self.runtime.dispatch()
            f.eventually(lambda: self.runtime.agent(a['id']).get('workspaceOperation') == 'checkpoint')
            f.eventually(lambda: not self.runtime.agent(a['id']).get('inFlight'))
        current = self.runtime.agent(a['id'])
        self.assertEqual(current['status'], 'queued')
        self.assertNotIn('startAttempt', current)
        self.assertFalse(any(method == 'turn/start' for method, _ in self.runtime.server.calls))
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='race-input'").fetchone()[0], 'pending')
        self.agent_update(current, workspaceOperation=None)
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a['id']).get('turnId'))
        self.assertEqual(sum(method == 'turn/start' for method, _ in self.runtime.server.calls), 1)

    def test_unknown_or_submitted_start_is_never_requeued(self):
        from codex_workspace_delivery import defer_workspace_start
        a = self.lead()
        error = ValueError("A workspace operation is active in this directory")
        for submitted, unknown in ((True, False), (False, True)):
            self.agent_update(a, status='starting', inFlight=True,
                              startAttempt={'id': 'attempt', 'submitted': submitted,
                                            'epoch': a['epoch'], 'events': []})
            self.assertFalse(defer_workspace_start(self.runtime, a['id'], 'attempt', error, unknown=unknown))
            self.assertTrue(self.runtime.agent(a['id'])['inFlight'])
        self.agent_update(a, autoWake=False)
        self.assertFalse(defer_workspace_start(self.runtime, a['id'], 'attempt', error))

    def test_restore_reservation_does_not_accept_input_for_an_obsolete_thread(self):
        a = self.lead()
        self.agent_update(a, workspaceOperation='restore')
        with self.assertRaisesRegex(ValueError, 'workspace operation'):
            self.runtime.send(a['id'], 'Continue', 'restore-input')
        with self.runtime.db() as db:
            self.assertIsNone(db.execute("SELECT id FROM runtime_events WHERE id='restore-input'").fetchone())

    def test_accepted_task_guidance_preserves_result_and_decision(self):
        a = self.lead()
        worker = self.worker(a)
        task = self.work(a, 'Original task', owner=worker['id'])
        task = self.action(worker, task, 'submit', result='Done', checks='Fixture passed', revision='abc')
        task = self.action(a, task, 'accept', result='Reviewed')
        for action in ('update', 'reject', 'accept'):
            with self.assertRaisesRegex(ValueError, 'follow-up task'):
                self.action(a, task, action, result='Late evidence', status='ready')
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?', (task['id'],)).fetchone()[0])
        self.assertEqual(saved['status'], 'accepted')
        self.assertEqual(len(saved['decisions']), 1)
        followup = self.work(a, 'Follow-up', description='New evidence for ' + task['id'])
        self.assertEqual(followup['status'], 'ready')


if __name__ == '__main__':
    unittest.main()
