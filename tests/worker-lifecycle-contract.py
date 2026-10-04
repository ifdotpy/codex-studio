#!/usr/bin/env python3
"""Named event waits and archive transitions through the real Runtime caller."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import copy
from pathlib import Path
import shutil
import unittest
from unittest.mock import patch

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

    def restart_backend(self):
        state = self.state
        runtime_type = type(self.runtime)
        self.runtime.close()
        self.runtime = runtime_type(state, fixture.WorkspaceServer)
        self.runtime.connect()

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

    def test_agent_entity_put_does_not_read_root_record_for_unused_wave_field(self):
        lead = self.lead(); worker = self.worker(lead)
        with self.runtime.db() as db:
            statements = []
            db.set_trace_callback(statements.append)
            view = self.runtime.agent_entity_view(db, worker)
            db.set_trace_callback(None)
        self.assertNotIn('wave', view)
        self.assertFalse(any('SELECT record FROM runtime_agents WHERE id=' in sql for sql in statements))

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

    def test_user_stop_notifies_parent_with_assignment_and_lead_stop_marker(self):
        lead = self.lead()
        self.agent_update(lead, status='completed', autoWake=True)
        worker = self.worker(lead)
        task = self.work(lead, 'Assigned task', owner=worker['id'])
        self.runtime.stop(worker['id'], descendants=False)
        rows = self.events(lead, 'child_result')
        self.assertEqual(len(rows), 1)
        payload = __import__('json').loads(rows[0]['text'])
        self.assertEqual(payload['agent_id'], worker['id'])
        self.assertEqual(payload['status'], 'paused')
        self.assertEqual(payload['reason'], 'Stopped by user')
        self.assertTrue(payload['last_activity'])
        self.assertEqual(payload['task_id'], task['id'])
        self.assertFalse(payload['result_submitted'])
        self.assertEqual(payload['next_step'], 'send')
        self.assertFalse(payload['requested_by_lead'])

    def test_lead_interrupt_is_marked_and_native_failure_notifies_once(self):
        lead = self.lead()
        self.agent_update(lead, status='completed', autoWake=True)
        worker = self.worker(lead)
        self.runtime.stop(worker['id'], descendants=False, sender=lead['id'], sender_epoch=lead['epoch'])
        payload = __import__('json').loads(self.events(lead, 'child_result')[0]['text'])
        self.assertTrue(payload['requested_by_lead'])
        worker = self.worker(lead, 'Failed worker')
        running = self.start(worker)
        self.runtime.server.notify({'method': 'turn/completed', 'params': {
            'threadId': running['threadId'], 'turn': {'id': running['turnId'], 'status': 'failed',
                'error': {'message': 'Native failure'}}}})
        rows = self.events(lead, 'child_result')
        self.assertEqual(len(rows), 2)
        failed = __import__('json').loads(rows[-1]['text'])
        self.assertEqual(failed['status'], 'failed')
        self.assertEqual(failed['reason']['message'], 'Native failure')
        self.assertEqual(failed['next_step'], 'recover')

    def test_stop_reports_submitted_assignment(self):
        lead = self.lead()
        self.agent_update(lead, status='completed', autoWake=True)
        worker = self.worker(lead)
        task = self.work(lead, 'Submitted task', owner=worker['id'])
        self.action(worker, task, 'submit', result='Done', checks='Checked', revision='abc123')
        self.runtime.stop(worker['id'], descendants=False)
        payload = __import__('json').loads(self.events(lead, 'child_result')[0]['text'])
        self.assertEqual(payload['task_id'], task['id'])
        self.assertTrue(payload['result_submitted'])

    def test_stop_reports_current_review_cycle_for_all_open_assignments(self):
        lead = self.lead()
        self.agent_update(lead, status='completed', autoWake=True)
        worker = self.worker(lead)
        rejected = self.work(lead, 'Rejected then rebased', owner=worker['id'])
        open_task = self.work(lead, 'Other open assignment', owner=worker['id'])
        self.action(worker, open_task, 'claim')
        self.git(self.project, 'init', '-q', '-b', 'main')
        self.git(self.project, 'config', 'user.name', 'Fixture')
        self.git(self.project, 'config', 'user.email', 'fixture@invalid.local')
        (self.project / 'base.txt').write_text('base\n')
        self.git(self.project, 'add', 'base.txt')
        self.git(self.project, 'commit', '-qm', 'base')
        branch = 'worker-' + worker['id']
        self.git(self.project, 'checkout', '-qb', branch)
        (self.project / 'worker.txt').write_text('worker change\n')
        self.git(self.project, 'add', 'worker.txt')
        self.git(self.project, 'commit', '-qm', 'worker change')
        revision = self.git(self.project, 'rev-parse', 'HEAD')
        submitted = self.action(worker, rejected, 'submit', result='Old result', checks='Passed', revision=revision)
        self.restart_backend()
        lead = self.runtime.agent(lead['id'])
        worker = self.runtime.agent(worker['id'])
        self.action(lead, rejected, 'reject', result='Rebase and submit again')
        self.restart_backend()
        worker = self.runtime.agent(worker['id'])
        self.git(self.project, 'checkout', 'main')
        (self.project / 'upstream.txt').write_text('upstream change\n')
        self.git(self.project, 'add', 'upstream.txt')
        self.git(self.project, 'commit', '-qm', 'upstream change')
        self.git(self.project, 'checkout', branch)
        self.git(self.project, 'rebase', 'main')
        self.restart_backend()
        lead = self.runtime.agent(lead['id'])
        worker = self.runtime.agent(worker['id'])
        rejected = self.action(worker, rejected, 'claim')
        self.restart_backend()
        worker = self.runtime.agent(worker['id'])
        self.runtime.stop(worker['id'], descendants=False)
        self.restart_backend()
        lead = self.runtime.agent(lead['id'])
        notice = json.loads(self.events(lead, 'child_result')[0]['text'])
        self.assertFalse(notice['result_submitted'])
        self.assertEqual(len(notice['tasks']), 2)
        states = {item['task_id']: item for item in notice['tasks']}
        self.assertEqual(states[rejected['id']]['current_result_id'], submitted['results'][-1]['id'])
        self.assertEqual(states[rejected['id']]['latest_decision'], 'reject')
        self.assertTrue(states[rejected['id']]['needs_resubmission'])
        self.assertEqual(states[open_task['id']]['status'], 'running')
        self.assertFalse(states[open_task['id']]['result_submitted'])

    def test_recovery_events_survive_lead_restart_and_deliver_once(self):
        lead = self.lead()
        self.runtime.accounts.data['accounts']['secondary-fixture'] = {
            'id': 'secondary-fixture', 'provider': 'codex', 'status': 'ready',
            'home': str(self.project),
        }
        original_get = self.runtime.accounts.get
        account_patch = patch.object(self.runtime.accounts, 'get', side_effect=lambda key:
            copy.deepcopy(self.runtime.accounts.data['accounts']['secondary-fixture'])
            if key == 'secondary-fixture' else original_get(key))
        account_patch.start()
        self.addCleanup(account_patch.stop)
        self.runtime.connect('secondary-fixture')
        result_agent = self.runtime.create({'name': 'Result worker', 'role': 'reviewer',
            'prompt': 'Inspect', 'account_key': 'secondary-fixture'}, lead['id'], defer=True)
        task_worker = self.worker(lead, 'Submit worker')
        failed_agent = self.worker(lead, 'Failure worker')
        stopped_worker = self.worker(lead, 'Stopped worker')
        task = self.work(lead, 'Submitted during lead recovery', owner=task_worker['id'])
        release_task = self.work(lead, 'Release during lead recovery', owner=failed_agent['id'])
        self.agent_update(lead, status='interrupted', autoWake=False, inFlight=False,
                          threadId='lead-thread', restartRecovery={
                              'stage': 'pending', 'autoWake': True, 'epoch': lead['epoch'],
                              'accountKey': lead.get('accountKey', 'default'),
                              'threadId': 'lead-thread', 'turnId': 'lead-turn', 'at': 1})
        result_worker = self.start(result_agent)
        self.runtime.servers['secondary-fixture'].complete(
            result_worker['threadId'], result_worker['turnId'], 'Saved child result')
        self.action(task_worker, task, 'submit', result='Task result', checks='Passed', revision='rev')
        task_worker = self.runtime.agent(task_worker['id'])
        task_worker = self.agent_update(task_worker, status='interrupted', autoWake=False,
            restartRecovery={'stage': 'pending', 'autoWake': True, 'epoch': task_worker['epoch'],
                'accountKey': task_worker.get('accountKey', 'default'),
                'threadId': task_worker.get('threadId'), 'turnId': 'task-worker-turn', 'at': 1})
        failed_worker = self.start(failed_agent)
        self.runtime.server.notify({'method': 'turn/completed', 'params': {
            'threadId': failed_worker['threadId'], 'turn': {'id': failed_worker['turnId'],
                'status': 'failed', 'error': {'message': 'Native failure'}}}})
        with self.runtime.lock, self.runtime.db() as db:
            agents = self.runtime.records(db, 'agents')
            self.runtime.release_failed_work(db, agents, force=True)
        self.runtime.stop(stopped_worker['id'], descendants=False)
        self.restart_backend()
        lead = self.runtime.agent(lead['id'])
        with self.runtime.db() as db:
            rows = db.execute("SELECT kind,status FROM runtime_events WHERE agent=? "
                              "AND kind IN ('child_result','work_review','work_released') ORDER BY created",
                              (lead['id'],)).fetchall()
            decision = db.execute("SELECT status FROM runtime_events WHERE agent=? AND kind='work_decision'",
                                  (task_worker['id'],)).fetchone()
        self.assertEqual([row['kind'] for row in rows].count('work_review'), 1)
        self.assertEqual([row['kind'] for row in rows].count('work_released'), 1)
        self.assertEqual([row['kind'] for row in rows].count('child_result'), 3)
        self.assertTrue(all(row['status'] == 'pending' for row in rows))
        self.assertIsNone(decision)
        self.agent_update(lead, status='idle', autoWake=True, restartRecovery=None,
                          threadId=None, turnId=None)
        self.runtime.send(lead['id'], 'Reconcile saved recovery events')
        self.runtime.dispatch()
        fixture.eventually(lambda: all(row['status'] == 'delivered' for row in self.events(lead)))
        self.assertEqual(len(self.events(lead, 'work_review')), 1)
        self.assertEqual(len(self.events(lead, 'work_released')), 1)
        self.assertEqual(len(self.events(lead, 'child_result')), 3)
        lead = self.runtime.agent(lead['id'])
        self.action(lead, task, 'reject', result='Revise the result')
        task_worker = self.runtime.agent(task_worker['id'])
        self.assertEqual(self.events(task_worker, 'work_decision')[0]['status'], 'pending')
        self.agent_update(task_worker, status='idle', autoWake=True, restartRecovery=None, threadId=None)
        self.runtime.send(task_worker['id'], 'Resume the revised assignment')
        self.runtime.dispatch(task_worker['id'])
        fixture.eventually(lambda: self.events(task_worker, 'work_decision')[0]['status'] == 'delivered')

    def test_terminal_capacity_and_workspace_holds_notify_parent_once(self):
        lead = self.lead()
        self.agent_update(lead, status='idle', autoWake=True)
        retry_worker = self.worker(lead, 'Retry worker')
        workspace_worker = self.worker(lead, 'Workspace worker')
        retry_id = 'capacity-retry-for-test'
        self.agent_update(retry_worker, status='failed', autoWake=False,
                          capacityRetry={'id': retry_id, 'status': 'scheduled',
                                         'accountKey': 'default', 'dueAt': 1234567890})
        operation_id = 'workspace-op-for-test'
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime._put_workspace_operation(db, {'id': operation_id,
                'agent': workspace_worker['id'], 'kind': 'restore', 'phase': 'provider_pending'})
        self.runtime._require_workspace_recovery(operation_id, workspace_worker['id'],
                                                 RuntimeError('Provider response was lost'))
        self.restart_backend()
        rows = self.events(lead, 'child_result')
        self.assertEqual(len(rows), 2)
        decoded = [json.loads(row['text']) for row in rows]
        self.assertEqual({item['agent_id'] for item in decoded},
                         {retry_worker['id'], workspace_worker['id']})
        self.assertTrue(all(row['status'] == 'pending' for row in rows))

    def test_usage_limit_hold_defers_until_no_resume_is_scheduled(self):
        lead = self.lead()
        self.agent_update(lead, status='completed', autoWake=True)
        worker = self.worker(lead)
        self.agent_update(worker, usageResumeEnabled=False)
        running = self.start(worker)
        self.runtime.server.notify({'method': 'turn/completed', 'params': {
            'threadId': running['threadId'], 'turn': {'id': running['turnId'], 'status': 'failed',
                'error': {'message': 'Usage limit reached', 'codexErrorInfo': 'usageLimitExceeded'}}}})
        rows = self.events(lead, 'child_result')
        self.assertEqual(len(rows), 1)
        payload = __import__('json').loads(rows[0]['text'])
        self.assertEqual(payload['status'], 'failed')
        self.assertEqual(payload['reason']['codexErrorInfo'], 'usageLimitExceeded')
        self.assertTrue(self.runtime.agent(worker['id'])['nativeFailureHold'])

    def test_scheduled_usage_resume_does_not_notify_before_continuation(self):
        lead = self.lead()
        self.agent_update(lead, status='completed', autoWake=True)
        worker = self.worker(lead)
        running = self.start(worker)
        self.runtime.server.notify({'method': 'turn/completed', 'params': {
            'threadId': running['threadId'], 'turn': {'id': running['turnId'], 'status': 'failed',
                'error': {'message': 'Usage limit reached', 'codexErrorInfo': 'usageLimitExceeded'}}}})
        self.assertEqual(self.runtime.agent(worker['id'])['usageResume']['status'], 'scheduled')
        self.assertEqual(self.events(lead, 'child_result'), [])

    def test_stopped_event_is_not_recreated_after_runtime_restart(self):
        lead = self.lead()
        self.agent_update(lead, status='completed', autoWake=True)
        worker = self.worker(lead)
        self.runtime.stop(worker['id'], descendants=False)
        self.assertEqual(len(self.events(lead, 'child_result')), 1)
        state = self.state
        runtime_type = type(self.runtime)
        self.runtime.close()
        self.runtime = runtime_type(state, fixture.WorkspaceServer)
        self.assertEqual(len(self.events(lead, 'child_result')), 1)

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
        for mode in ('clean', 'dirty', 'missing'):
            dirty = mode == 'dirty'
            worker = self.worker(lead, mode.title())
            path = self.project / '.worktrees' / 'codex-agents' / worker['id']
            path.parent.mkdir(parents=True, exist_ok=True)
            self.git(self.project, 'worktree', 'add', '-q', '-b', 'worker-' + worker['id'], str(path), 'main')
            self.agent_update(worker, role='implementer', cwd=str(path), worktree=True,
                              worktreeReady=True, status='completed')
            task = self.work(lead, 'Task', owner=worker['id'])
            # This archive fixture starts after the assigned worker has read its task.
            with self.runtime.lock, self.runtime.db() as db:
                db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=? AND kind='work_ready'",
                           (worker['id'],))
                current = self.runtime.agent(worker['id'], db)
                current['status'] = 'completed'
                self.runtime.put(db, 'agents', current)
            self.action(worker, task, 'submit', result='Done', checks='Checked', revision=commit)
            if dirty:
                (path / 'untracked.txt').write_text('keep me\n')
            if mode == 'missing':
                shutil.rmtree(path)
            result = self.runtime.work_action(lead['id'], {'action': 'accept', 'task_id': task['id'],
                                                              'result': 'Reviewed'}, 'accept-' + worker['id'],
                                              actor=lead['id'])
            self.assertEqual(result['archive']['status'], 'kept' if dirty else 'archived')
            detail = self.runtime.model_work(lead['id'], {'action': 'get', 'task_id': task['id']})
            self.assertEqual(detail['archive']['status'], result['archive']['status'])
            if dirty:
                self.assertIn('changes', result['archive']['reason'])
                self.assertTrue(path.exists())
                self.assertEqual(len(self.events(worker, 'work_decision')), 1)
            else:
                self.assertFalse(path.exists())
                self.assertEqual(self.events(worker, 'work_decision'), [])
                if mode == 'missing':
                    self.assertEqual(result['archive']['worktree']['state'], 'missing')
                    self.assertEqual(result['archive']['worktree']['reason'],
                                     'worktree folder missing; branch saved to archive ref')
                else:
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
