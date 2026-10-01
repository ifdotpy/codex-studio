#!/usr/bin/env python3
"""Named event waits and archive transitions through the real Runtime caller."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('lifecycle_fixture', Path(__file__).with_name('workspace-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_agent_management import manage_agent


class LifecycleContract(unittest.TestCase):
    setUp = fixture.WorkspaceContract.setUp
    tearDown = fixture.WorkspaceContract.tearDown
    lead = fixture.WorkspaceContract.lead
    worker = fixture.WorkspaceContract.worker
    start = fixture.WorkspaceContract.start
    events = fixture.WorkspaceContract.events
    agent_update = fixture.WorkspaceContract.agent_update
    git = fixture.WorkspaceContract.git
    work = fixture.WorkspaceContract.work
    action = fixture.WorkspaceContract.action
    tool = fixture.WorkspaceContract.tool

    def manage(self, actor, action, **data):
        return manage_agent(self.runtime, actor['id'], {'action': action, **data}, actor['epoch'])

    def test_park_wake_all_once_and_reject_late_wait(self):
        lead = self.lead()
        one = self.worker(lead, 'One')
        two = self.worker(lead, 'Two')
        for worker in (one, two):
            parked = self.manage(lead, 'park', agent_id=worker['id'], event='seal3-published')
            self.assertEqual(parked['agent']['parkedEvent'], 'seal3-published')
            self.assertEqual(self.runtime.agent(worker['id'])['status'], 'parked')
        rows = self.manage(lead, 'list_parked')['items']
        self.assertEqual({row['id'] for row in rows}, {one['id'], two['id']})
        status = self.runtime.model_directory(lead['id'], 'orchestration_status', {})
        parked = [row for row in status['changes'] if row['kind'] == 'agent'
                  and row['id'] in {one['id'], two['id']}]
        self.assertEqual({row['waitsForEvent'] for row in parked}, {'seal3-published'})
        result = self.manage(lead, 'emit_event', event='seal3-published', request_id='event-once')
        self.assertEqual(set(result['woken']), {one['id'], two['id']})
        self.assertEqual(self.manage(lead, 'emit_event', event='seal3-published',
                                     request_id='event-once'), result)
        self.assertTrue(self.manage(lead, 'emit_event', event='seal3-published',
                                    request_id='another-id')['replayed'])
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.manage(lead, 'emit_event', event='other-event', request_id='event-once')
        with self.assertRaisesRegex(ValueError, 'Only the lead'):
            self.manage(one, 'emit_event', event='other-event', request_id='worker-event')
        for worker in (one, two):
            self.assertEqual(len(self.events(worker, 'event_wake')), 1)
            self.assertEqual(self.runtime.agent(worker['id'])['status'], 'queued')
        with self.assertRaisesRegex(ValueError, 'already occurred'):
            self.manage(lead, 'park', agent_id=one['id'], event='seal3-published')

    def test_worker_self_park_after_turn_keeps_parent_result(self):
        lead = self.lead()
        worker = self.start(self.worker(lead), 'Do work')
        fixture.eventually(lambda: self.events(worker)[0]['status'] == 'delivered')
        self.tool(worker, 'orchestration_agent_manage',
                  {'action': 'park', 'agent_id': worker['id'], 'event': 'release'})
        self.assertTrue(self.runtime.agent(worker['id'])['parkAfterTurn'])
        self.runtime.server.complete(worker['threadId'], worker['turnId'], 'Result for lead')
        fixture.eventually(lambda: self.runtime.agent(worker['id'])['status'] == 'parked')
        self.assertEqual(len(self.events(lead, 'child_result')), 1)
        self.assertIn('Result for lead', self.events(lead, 'child_result')[0]['text'])
        self.assertEqual(self.runtime.agent(worker['id'])['parkedEvent'], 'release')
        self.manage(lead, 'cancel_park', agent_id=worker['id'])
        self.assertEqual(self.runtime.agent(worker['id'])['status'], 'queued')
        self.assertEqual(len(self.events(worker, 'event_wake')), 1)

    def test_reviewer_archives_only_after_result_delivery(self):
        lead = self.lead()
        reviewer = self.start(self.worker(lead), 'Review code')
        self.runtime.server.complete(reviewer['threadId'], reviewer['turnId'], 'Review complete')
        fixture.eventually(lambda: len(self.events(lead, 'child_result')) == 1)
        self.assertFalse(self.runtime.agent(reviewer['id']).get('agentArchive'))
        self.start(lead, 'Read review')
        fixture.eventually(lambda: bool(self.runtime.agent(reviewer['id']).get('agentArchive')))
        archived = self.runtime.agent(reviewer['id'])
        self.assertEqual(archived['agentArchive']['reason'], 'Reviewer result delivered to parent')
        self.assertFalse(archived.get('worktreeReady'))
        self.assertEqual(self.events(lead, 'child_result')[0]['status'], 'delivered')

    def test_accept_archives_clean_integrated_owner_and_keeps_dirty_owner(self):
        self.git(self.project, 'init', '-q', '-b', 'main')
        self.git(self.project, 'config', 'user.name', 'Fixture')
        self.git(self.project, 'config', 'user.email', 'fixture@invalid.local')
        (self.project / 'tracked.txt').write_text('base\n')
        self.git(self.project, 'add', 'tracked.txt')
        self.git(self.project, 'commit', '-qm', 'base')
        commit = self.git(self.project, 'rev-parse', 'HEAD')
        lead = self.lead()
        for dirty in (False, True):
            worker = self.worker(lead, 'Dirty' if dirty else 'Clean')
            path = self.project / '.worktrees' / 'codex-agents' / worker['id']
            path.parent.mkdir(parents=True, exist_ok=True)
            self.git(self.project, 'worktree', 'add', '-q', '-b', 'worker-' + worker['id'], str(path), 'main')
            self.agent_update(worker, role='implementer', cwd=str(path), worktree=True,
                              worktreeReady=True, status='completed')
            task = self.work(lead, 'Task', owner=worker['id'])
            self.action(worker, task, 'submit', result='Done', checks='Checked', revision=commit)
            if dirty:
                (path / 'untracked.txt').write_text('keep me\n')
            result = self.runtime.work_action(lead['id'], {'action': 'accept', 'task_id': task['id'],
                                                              'result': 'Reviewed'}, 'accept-' + worker['id'],
                                              actor=lead['id'])
            self.assertEqual(result['archive']['status'], 'kept' if dirty else 'archived')
            if dirty:
                self.assertIn('changes', result['archive']['reason'])
                self.assertTrue(path.exists())
                self.assertEqual(len(self.events(worker, 'work_decision')), 1)
            else:
                self.assertFalse(path.exists())
                self.assertEqual(self.events(worker, 'work_decision'), [])
                ref = self.git(self.project, 'rev-parse', 'refs/codex-agents/archive/' + worker['id'])
                self.assertEqual(ref, commit)
            replay = self.runtime.work_action(lead['id'], {'action': 'accept', 'task_id': task['id'],
                                                              'result': 'Reviewed'}, 'accept-' + worker['id'],
                                              actor=lead['id'])
            self.assertEqual(replay['archive'], result['archive'])

    def test_accept_keeps_owner_with_open_task_or_unmerged_revision(self):
        self.git(self.project, 'init', '-q', '-b', 'main')
        self.git(self.project, 'config', 'user.name', 'Fixture')
        self.git(self.project, 'config', 'user.email', 'fixture@invalid.local')
        (self.project / 'tracked.txt').write_text('base\n')
        self.git(self.project, 'add', 'tracked.txt')
        self.git(self.project, 'commit', '-qm', 'base')
        main = self.git(self.project, 'rev-parse', 'HEAD')
        lead = self.lead()
        for mode in ('open', 'unmerged'):
            worker = self.worker(lead, mode)
            path = self.project / '.worktrees' / 'codex-agents' / worker['id']
            path.parent.mkdir(parents=True, exist_ok=True)
            self.git(self.project, 'worktree', 'add', '-q', '-b', 'worker-' + worker['id'], str(path), 'main')
            self.agent_update(worker, role='implementer', cwd=str(path), worktree=True,
                              worktreeReady=True, status='completed')
            revision = main
            if mode == 'unmerged':
                (path / 'tracked.txt').write_text('branch result\n')
                self.git(path, 'add', 'tracked.txt')
                self.git(path, 'commit', '-qm', 'worker result')
                revision = self.git(path, 'rev-parse', 'HEAD')
            else:
                self.work(lead, 'Another open task', owner=worker['id'])
            task = self.work(lead, 'Task', owner=worker['id'])
            self.action(worker, task, 'submit', result='Done', checks='Checked', revision=revision)
            accepted = self.action(lead, task, 'accept', result='Reviewed')
            self.assertEqual(accepted['archive']['status'], 'kept')
            self.assertIn('another open task' if mode == 'open' else 'not reachable',
                          accepted['archive']['reason'])
            self.assertTrue(path.exists())


if __name__ == '__main__':
    unittest.main()
