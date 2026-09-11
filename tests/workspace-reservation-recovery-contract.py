#!/usr/bin/env python3
"""Checkpoint reservations settle by identity without replaying native input."""
import importlib.util
from pathlib import Path
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'reservation_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_workspace_delivery import WorkspaceBusyError, defer_workspace_start


class ReservationRecovery(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    agent_update = f.WorkspaceContract.agent_update

    def operation(self, operation_id):
        with self.runtime.db() as db:
            return self.runtime._workspace_operation(db, operation_id)

    def reserve(self, agent, kind='checkpoint'):
        with self.runtime.lock, self.runtime.db() as db:
            return self.runtime._reserve_checkpoint(db, self.runtime.agent(agent['id'], db), kind, 'old-turn')

    def test_capture_receipt_identifies_owner_and_settlement_then_dispatches_once(self):
        owner = self.lead('owner')
        peer = self.lead('peer')
        entered, release = threading.Event(), threading.Event()
        def capture(*args):
            entered.set()
            self.assertTrue(release.wait(5))
            return {'id': 'fixture-checkpoint'}
        with patch.object(self.runtime, 'capture_checkpoint', side_effect=capture):
            capture_future = self.runtime.pool.submit(self.runtime.checkpoint_capture, owner['id'])
            try:
                self.assertTrue(entered.wait(5))
                result = self.runtime.send(peer['id'], 'Exact follow-up', 'capture-wait-input')
                blocker = result['waitingFor'][0]
                self.assertEqual(blocker['agentId'], owner['id'])
                self.assertEqual(blocker['operation'], 'capture')
                self.assertEqual(blocker['phase'], 'capture_running')
                self.assertEqual(self.operation(blocker['operationId'])['phase'], 'capture_running')
                self.runtime.dispatch()
                self.assertFalse(self.runtime.agent(peer['id'])['inFlight'])
                self.runtime.send(peer['id'], 'Exact follow-up', 'capture-wait-input')
            finally:
                release.set()
                capture_future.result(5)
        self.assertEqual(self.operation(blocker['operationId'])['phase'], 'completed')
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(peer['id']).get('turnId'))
        self.assertEqual(sum(m == 'turn/start' for m, _ in self.runtime.server.calls), 1)
        with self.runtime.db() as db:
            self.assertEqual(db.execute("SELECT COUNT(*) FROM runtime_events WHERE id='capture-wait-input'").fetchone()[0], 1)

    def test_late_capture_completion_does_not_clear_replacement_reservation(self):
        a = self.lead()
        operation_id = self.reserve(a)
        entered, release = threading.Event(), threading.Event()
        def capture(*args):
            entered.set()
            self.assertTrue(release.wait(5))
        with patch.object(self.runtime, 'capture_checkpoint', side_effect=capture):
            future = self.runtime.pool.submit(self.runtime.checkpoint_after_turn, a['id'], 'old-turn', operation_id)
            try:
                self.assertTrue(entered.wait(5))
                self.agent_update(a, workspaceOperation='restore_recovery', workspaceReservationId='replacement')
            finally:
                release.set()
                future.result(5)
        current = self.runtime.agent(a['id'])
        self.assertEqual(current['workspaceOperation'], 'restore_recovery')
        self.assertEqual(current['workspaceReservationId'], 'replacement')
        self.assertEqual(self.operation(operation_id)['phase'], 'completed')
        with self.assertRaises(WorkspaceBusyError):
            self.runtime.send(a['id'], 'Must not enter another thread', 'restore-held')

    def test_duplicate_or_superseded_callback_never_captures_again(self):
        a = self.lead()
        operation_id = self.reserve(a)
        with patch.object(self.runtime, 'capture_checkpoint', return_value={'id': 'capture'}) as capture:
            self.runtime.checkpoint_after_turn(a['id'], 'old-turn', operation_id)
            self.runtime.checkpoint_after_turn(a['id'], 'old-turn', operation_id)
            self.assertEqual(capture.call_count, 1)
        replacement = self.reserve(a)
        self.runtime.checkpoint_after_turn(a['id'], 'old-turn', operation_id)
        self.assertEqual(self.runtime.agent(a['id'])['workspaceReservationId'], replacement)

    def test_superseded_pending_callback_settles_without_capturing(self):
        a = self.lead()
        operation_id = self.reserve(a)
        replacement = self.reserve(a)
        with patch.object(self.runtime, 'capture_checkpoint') as capture:
            self.runtime.checkpoint_after_turn(a['id'], 'old-turn', operation_id)
            capture.assert_not_called()
        self.assertEqual(self.operation(operation_id)['phase'], 'failed')
        self.assertEqual(self.runtime.agent(a['id'])['workspaceReservationId'], replacement)

    def test_completion_during_branch_keeps_branch_reservation(self):
        a = self.lead()
        self.runtime.send(a['id'], 'Start', 'first-turn')
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a['id']).get('turnId'))
        a = self.agent_update(a, worktreeReady=True, workspaceOperation='branch')
        with patch.object(self.runtime, 'capture_checkpoint') as capture:
            self.runtime.server.complete(a['threadId'], a['turnId'])
            self.assertEqual(self.runtime.agent(a['id'])['workspaceOperation'], 'branch')
            capture.assert_not_called()
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, 'workspace_operations'), [])

    def complete_with_activity(self, kind):
        a = self.lead('completed owner')
        peer = self.lead('active peer')
        self.runtime.send(a['id'], 'Start', 'first-turn')
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a['id']).get('turnId'))
        a = self.agent_update(a, worktreeReady=True)
        with self.runtime.lock, self.runtime.db() as db:
            if kind == 'agent':
                peer.update(inFlight=True, status='running', turnId='peer-turn')
                self.runtime.put(db, 'agents', peer)
            elif kind == 'command':
                self.runtime.put(db, 'tasks', {'id': 'peer-command', 'agent': peer['id'],
                    'status': 'running', 'kind': 'command', 'processId': 'native-process',
                    'turnId': 'peer-turn'})
            else:
                self.runtime.put(db, 'monitors', {'id': 'peer-monitor', 'agent': peer['id'],
                    'cwd': peer['cwd'], 'status': 'running'})
        with patch.object(self.runtime, 'capture_checkpoint') as capture:
            self.runtime.server.complete(a['threadId'], a['turnId'])
            capture.assert_not_called()
        current = self.runtime.agent(a['id'])
        self.assertIsNone(current.get('workspaceOperation'))
        self.assertIn('Checkpoint skipped:', current['checkpointError'])
        with self.runtime.db() as db:
            self.assertEqual(self.runtime.records(db, 'workspace_operations'), [])
            if kind == 'command':
                self.assertEqual(self.runtime.records(db, 'tasks')[0]['status'], 'running')
            elif kind == 'monitor':
                self.assertEqual(self.runtime.records(db, 'monitors')[0]['status'], 'running')
        return current

    def test_turn_completion_skips_capture_during_peer_turn(self):
        current = self.complete_with_activity('agent')
        self.assertIn('An agent is using this workspace', current['checkpointError'])

    def test_turn_completion_skips_capture_during_peer_command(self):
        current = self.complete_with_activity('command')
        self.assertIn('A command or tool is still active', current['checkpointError'])

    def test_turn_completion_skips_capture_during_peer_monitor(self):
        current = self.complete_with_activity('monitor')
        self.assertIn('A monitor is using this workspace', current['checkpointError'])

    def test_capture_rechecks_monitor_activity_before_reading_files(self):
        a = self.lead()
        operation_id = self.reserve(a)
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'monitors', {'id': 'late-monitor', 'agent': a['id'],
                'cwd': a['cwd'], 'status': 'running'})
        with patch.object(self.runtime, 'capture_checkpoint') as capture:
            self.runtime.checkpoint_after_turn(a['id'], 'old-turn', operation_id)
            capture.assert_not_called()
        self.assertIsNone(self.runtime.agent(a['id']).get('workspaceOperation'))
        self.assertEqual(self.operation(operation_id)['phase'], 'failed')
        self.assertIn('A monitor is using this workspace', self.operation(operation_id)['error'])

    def test_public_idle_guard_reads_current_agent_activity(self):
        a = self.lead()
        self.agent_update(a, inFlight=True, status='running')
        with self.assertRaisesRegex(ValueError, 'An agent is using this workspace'):
            self.runtime.assert_workspace_idle(a)

    def test_completed_current_turn_does_not_block_its_own_checkpoint(self):
        a = self.lead()
        self.runtime.send(a['id'], 'Start', 'first-turn')
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a['id']).get('turnId'))
        a = self.agent_update(a, worktreeReady=True)
        with patch.object(self.runtime, 'capture_checkpoint', return_value={'id': 'capture'}) as capture:
            self.runtime.server.complete(a['threadId'], a['turnId'])
            f.eventually(lambda: self.runtime.agent(a['id']).get('workspaceOperation') is None)
            self.assertEqual(capture.call_count, 1)
        with self.runtime.db() as db:
            operations = self.runtime.records(db, 'workspace_operations')
        self.assertEqual(len(operations), 1)
        self.assertEqual(operations[0]['phase'], 'completed')

    def test_capture_failure_releases_reservation_and_keeps_exact_failure(self):
        a = self.lead()
        operation_id = self.reserve(a)
        with patch.object(self.runtime, 'capture_checkpoint', side_effect=TimeoutError('Git capture timed out')):
            self.runtime.checkpoint_after_turn(a['id'], 'old-turn', operation_id)
        self.assertEqual(self.operation(operation_id)['phase'], 'failed')
        current = self.runtime.agent(a['id'])
        self.assertIsNone(current['workspaceOperation'])
        self.assertEqual(current['checkpointError'], 'Git capture timed out')
        self.assertTrue(current['autoWake'])

    def test_rejected_executor_submission_does_not_leave_directory_reserved(self):
        a = self.lead()
        with self.runtime.lock, self.runtime.db() as db:
            with patch.object(self.runtime.pool, 'submit', side_effect=RuntimeError('executor closed')):
                self.runtime.queue_checkpoint_after_turn(db, a, 'completed-turn')
        current = self.runtime.agent(a['id'])
        self.assertIsNone(current['workspaceOperation'])
        self.assertEqual(current['checkpointError'], 'executor closed')

    def test_restart_releases_only_read_only_capture_and_retains_pending_input(self):
        a = self.lead()
        operation_id = self.reserve(a)
        self.runtime.send(a['id'], 'Continue after snapshot', 'restart-input')
        thread = self.runtime.agent(a['id']).get('threadId')
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        current = self.runtime.agent(a['id'])
        self.assertEqual(current['status'], 'queued')
        self.assertTrue(current['autoWake'])
        self.assertEqual(current.get('threadId'), thread)
        self.assertIsNone(current['workspaceOperation'])
        self.assertEqual(self.operation(operation_id)['phase'], 'failed')
        with self.runtime.db() as db:
            event = db.execute("SELECT * FROM runtime_events WHERE id='restart-input'").fetchone()
            self.assertEqual(event['status'], 'pending')
            self.assertEqual(event['epoch'], current['epoch'])
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(a['id']).get('turnId'))
        self.assertEqual(sum(m == 'turn/start' for m, _ in self.runtime.server.calls), 1)

    def test_restart_releases_legacy_capture_without_resuming_stopped_agent(self):
        a = self.lead()
        self.agent_update(a, workspaceOperation='checkpoint', status='paused', autoWake=False)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        current = self.runtime.agent(a['id'])
        self.assertIsNone(current['workspaceOperation'])
        self.assertFalse(current['autoWake'])
        self.assertEqual(current['status'], 'paused')

    def attempt(self, a, **changes):
        return self.agent_update(a, status='starting', inFlight=True,
                                startAttempt={'id': 'exact-attempt', 'epoch': a['epoch'],
                                              'events': [], 'submitted': False, **changes})

    def test_deferral_uses_original_guard_evidence_not_replacement_operation(self):
        a = self.lead()
        self.attempt(a)
        # A restore can settle before start_error takes its lock. It must remain
        # a hard thread-boundary rejection even if a checkpoint now holds the path.
        self.agent_update(a, workspaceOperation='checkpoint')
        error = WorkspaceBusyError([{'agentId': a['id'], 'operation': 'restore'}])
        self.assertFalse(defer_workspace_start(self.runtime, a['id'], 'exact-attempt', error))
        self.assertTrue(self.runtime.agent(a['id'])['inFlight'])
        # Text from an unrelated provider is not proof of a local guard failure.
        error = ValueError('A workspace operation is active in this directory')
        self.assertFalse(defer_workspace_start(self.runtime, a['id'], 'exact-attempt', error))

    def test_submitted_unknown_epoch_changed_and_stopped_attempts_never_requeue(self):
        a = self.lead()
        error = WorkspaceBusyError([{'agentId': a['id'], 'operation': 'checkpoint'}])
        for changes, unknown in (({'submitted': True}, False), ({}, True), ({'epoch': a['epoch'] + 1}, False),
                                 ({'observedTurnId': 'native-turn'}, False), ({'accountKey': 'other-account'}, False)):
            self.attempt(a, **changes)
            self.assertFalse(defer_workspace_start(self.runtime, a['id'], 'exact-attempt', error, unknown=unknown))
            self.assertTrue(self.runtime.agent(a['id'])['inFlight'])
        self.attempt(a)
        self.agent_update(a, autoWake=False)
        self.assertFalse(defer_workspace_start(self.runtime, a['id'], 'exact-attempt', error))


if __name__ == '__main__':
    unittest.main()
