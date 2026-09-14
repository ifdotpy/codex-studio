#!/usr/bin/env python3
"""Timer review settings, durable delivery, and cross-team room boundaries."""

import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'review_workspace_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class ChatReviewsContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    agent_update = f.WorkspaceContract.agent_update

    def pair(self):
        target = self.lead('Target')
        reviewer = self.worker(target, 'Reviewer', prompt='')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.item(db, target['id'], 'request', 'user', 'Build the exact requested interface')
        return target['id'], reviewer['id']

    def configure(self, target, reviewer, now=1000, **changes):
        with patch('codex_chat_reviews.time.time', return_value=now):
            return self.runtime.chat_organization(target, {'id': target, 'review_schedule': {
                'reviewer_id': reviewer, 'expected_revision': 0, **changes}})

    def schedule(self, target, index=0):
        return self.runtime.agent(target)['reviewSchedules'][index]

    def run_now(self, target, reviewer, request='click', revision=1):
        return self.runtime.chat_organization(target, {'id': target, 'review_schedule': {
            'action': 'run', 'reviewer_id': reviewer, 'expected_revision': revision, 'request_id': request}})

    def test_manual_review_retries_and_busy_clicks_never_duplicate(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        with patch('codex_chat_reviews.time.time', return_value=1200):
            self.run_now(target, reviewer)
        first = self.schedule(target)['lastEventId']
        self.assertEqual(self.schedule(target)['nextAt'], 3000)
        self.assertEqual(self.events()[0]['agent'], reviewer)
        self.assertIn('manual-', first)
        for status in ['pending', 'reserved', 'dispatching', 'uncertain', 'delivered']:
            with self.runtime.db() as db:
                db.execute('UPDATE runtime_events SET status=? WHERE id=?', (status, first))
            self.run_now(target, reviewer, 'busy-' + status)
            self.assertEqual(len(self.events()), 1)
        self.completed(target, reviewer)
        self.run_now(target, reviewer)
        self.assertEqual(len(self.events()), 1, 'Lost response retry after completion is still idempotent')
        self.run_now(target, reviewer, 'second-click')
        self.assertEqual(len(self.events()), 2, 'Explicit manual review can recheck unchanged content')
        self.assertNotEqual(self.schedule(target)['lastEventId'], first)

    def test_manual_cross_team_review_and_request_identity_conflict(self):
        target, _ = self.pair()
        reviewer = self.lead('Foreign reviewer')['id']
        self.configure(target, reviewer)
        self.run_now(target, reviewer)
        from codex_team_isolation import validate_event
        with self.runtime.db() as db:
            self.assertIsNone(validate_event(self.runtime, db, self.runtime.agent(reviewer, db), self.events()[0]))
        other = self.lead('Other target')['id']
        self.configure(other, reviewer)
        with self.assertRaises(ValueError):
            self.run_now(other, reviewer)
        self.assertEqual(len(self.events()), 1)

    def test_manual_review_preserves_pause_recovery_and_revision_boundaries(self):
        target, reviewer = self.pair()
        with self.assertRaisesRegex(ValueError, 'Enable'):
            self.run_now(target, reviewer)
        self.configure(target, reviewer)
        with self.assertRaisesRegex(ValueError, 'changed'):
            self.run_now(target, reviewer, revision=0)
        self.update(reviewer, nativeFailureHold=True)
        with self.assertRaisesRegex(ValueError, 'recovery'):
            self.run_now(target, reviewer)
        self.update(reviewer, nativeFailureHold=False)
        self.configure(target, reviewer, enabled=False, expected_revision=1)
        with self.assertRaisesRegex(ValueError, 'Enable'):
            self.run_now(target, reviewer, revision=2)
        self.assertEqual(len(self.events()), 0)

    def tick(self, now):
        with patch('codex_rules.time.time', return_value=now):
            self.runtime.rules_tick()

    def events(self):
        with self.runtime.db() as db:
            return [dict(row) for row in db.execute("SELECT * FROM runtime_events WHERE kind='chat_review'")]

    def update(self, agent_id, **fields):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(agent_id, db)
            agent.update(fields)
            self.runtime.put(db, 'agents', agent)

    def completed(self, target, reviewer, turn='review-turn'):
        event = self.schedule(target)['lastEventId']
        with self.runtime.lock, self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered',turn_id=? WHERE id=?", (turn, event))
            db.execute('INSERT INTO runtime_completed_turns VALUES (?)', (reviewer + ':' + turn,))
        self.update(reviewer, status='completed', inFlight=False)

    def test_explicit_foreign_review_allows_only_the_pair_and_revokes_on_pause(self):
        target, worker = self.pair()
        reviewer = self.lead('Reviewer in another team')['id']
        outsider = self.lead('Unassigned')['id']
        self.configure(target, reviewer)
        schedule = self.schedule(target)
        self.assertEqual(schedule['authorizedRoots'], {target:target, reviewer:reviewer})
        self.tick(2800)
        events = self.events()
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]['agent'], reviewer)
        from codex_team_isolation import validate_event
        with self.runtime.db() as db:
            self.assertIsNone(validate_event(self.runtime, db, self.runtime.agent(reviewer,db), events[0]))
        message = self.runtime.chat_message(reviewer, target, 'Review finding', 'foreign-review-message')
        self.assertIn(target, message['deliveries'])
        self.runtime.chat_message(target, reviewer, 'Review reply', 'foreign-review-reply')
        self.assertEqual(len(self.runtime.chat_read(schedule['roomId'], reviewer)['messages']), 2)
        for sender, recipient in [(reviewer, worker), (reviewer, outsider), (outsider, target)]:
            with self.assertRaisesRegex(ValueError, 'another team'):
                self.runtime.chat_message(sender, recipient, 'Denied', sender + recipient)
        with self.assertRaisesRegex(ValueError, 'unavailable'):
            self.runtime.chat_read(schedule['roomId'], outsider)
        self.configure(target, reviewer, enabled=False, expected_revision=1)
        self.tick(3000)
        self.assertEqual(self.schedule(target)['status'], 'paused')
        self.assertIsNone(self.schedule(target)['reason'])
        with self.assertRaisesRegex(ValueError, 'another team'):
            self.runtime.chat_message(reviewer, target, 'Paused', 'paused-message')
        with self.assertRaisesRegex(ValueError, 'unavailable'):
            self.runtime.chat_read(schedule['roomId'], reviewer)
        with self.runtime.db() as db:
            self.assertIsNotNone(validate_event(self.runtime, db, self.runtime.agent(reviewer,db), events[0]))
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_chat_messages WHERE room=?', (schedule['roomId'],)).fetchone()[0], 2)

    def test_missing_team_identity_fails_closed(self):
        target, reviewer = self.pair()
        self.update(target, rootId=None)
        self.update(reviewer, rootId=None)
        with self.assertRaisesRegex(ValueError, 'team identity'):
            self.configure(target, reviewer)
        self.assertNotIn('reviewSchedules', self.runtime.agent(target))

    def test_existing_foreign_schedule_cancels_only_pending_and_preserves_history(self):
        for status in ['pending', 'reserved', 'dispatching', 'uncertain', 'delivered']:
            with self.subTest(status=status):
                target, reviewer = self.pair()
                self.configure(target, reviewer)
                self.tick(2800)
                event_id = self.schedule(target)['lastEventId']
                with self.runtime.db() as db:
                    db.execute('UPDATE runtime_events SET status=?,turn_id=? WHERE id=?',
                               (status, 'existing-turn', event_id))
                    before = dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (event_id,)).fetchone())
                self.update(reviewer, rootId=reviewer, inFlight=status == 'dispatching')
                with patch('codex_chat_reviews._snapshot', side_effect=AssertionError('Read foreign chat')):
                    self.tick(50000)
                schedule = self.schedule(target)
                self.assertEqual(schedule['status'], 'blocked')
                self.assertEqual(schedule['reason'], 'The review assignment no longer matches these chats')
                self.assertEqual(schedule['lastEventId'], event_id)
                with self.runtime.db() as db:
                    after = dict(db.execute('SELECT * FROM runtime_events WHERE id=?', (event_id,)).fetchone())
                expected = dict(before)
                if status == 'pending':
                    expected.update(status='cancelled', error=schedule['reason'])
                self.assertEqual(after, expected)
                self.assertEqual(self.runtime.agent(reviewer)['inFlight'], status == 'dispatching')
                self.assertTrue(self.runtime.agent(reviewer)['autoWake'])
                count = len(self.events())
                self.tick(60000)
                self.assertEqual(len(self.events()), count)

    def test_foreign_schedule_removal_preserves_room_and_explicit_reassignment_renews_grant(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        self.tick(2800)
        room_id = self.schedule(target)['roomId']
        self.update(reviewer, rootId=reviewer)
        with self.runtime.db() as db:
            before = db.execute('SELECT record FROM runtime_rooms WHERE id=?', (room_id,)).fetchone()[0]
        self.configure(target, reviewer, expected_revision=1, enabled=False, removed=True)
        removed = self.schedule(target)
        self.configure(target, reviewer, expected_revision=1, enabled=False, removed=True)
        self.assertEqual(self.schedule(target), removed)
        self.assertTrue(removed['removed'])
        self.assertEqual(self.events()[0]['status'], 'cancelled')
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT record FROM runtime_rooms WHERE id=?', (room_id,)).fetchone()[0], before)
        self.configure(target, reviewer, expected_revision=2)
        self.assertTrue(self.schedule(target)['enabled'])
        self.assertEqual(self.schedule(target)['authorizedRoots'], {target:target, reviewer:reviewer})

    def test_default_timer_room_and_restart_persistence_without_model_call(self):
        target, reviewer = self.pair()
        calls_before = list(self.runtime.server.calls)
        self.configure(target, reviewer)
        schedule = self.schedule(target)
        self.assertEqual((schedule['intervalMinutes'], schedule['nextAt'], schedule['revision']), (30, 2800, 1))
        self.assertEqual(self.events(), [])
        # Worker creation reads the catalog. Review settings must not call it
        # again or start a model turn.
        self.assertEqual(self.runtime.server.calls, calls_before)
        self.assertFalse(any(method == 'turn/start' for method, _ in self.runtime.server.calls))
        room = self.runtime.chat_read(schedule['roomId'], reviewer)['room']
        self.assertEqual(room['reviewTargets'], [target])
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.assertEqual(self.schedule(target), schedule)
        self.tick(2799)
        self.assertEqual(self.events(), [])
        self.tick(50000)
        self.assertEqual(len(self.events()), 1)
        self.assertEqual(self.schedule(target)['nextAt'], 51800)
        self.tick(50001)
        self.assertEqual(len(self.events()), 1)

    def test_revision_lost_response_replay_pause_remove_and_readd(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        original = self.schedule(target)
        self.configure(target, reviewer, now=1050)
        self.assertEqual(self.schedule(target), original)
        with self.assertRaisesRegex(ValueError, 'changed'):
            self.configure(target, reviewer, interval_minutes=12)
        self.configure(target, reviewer, expected_revision=1, enabled=False)
        self.assertIsNone(self.schedule(target)['nextAt'])
        self.configure(target, reviewer, expected_revision=2, enabled=False, removed=True)
        self.assertTrue(self.schedule(target)['removed'])
        with self.assertRaisesRegex(ValueError, 'changed'):
            self.configure(target, reviewer)
        self.configure(target, reviewer, expected_revision=3, interval_minutes=5)
        self.assertEqual((self.schedule(target)['revision'], self.schedule(target)['nextAt']), (4, 1300))
        self.assertFalse(self.schedule(target)['removed'])
        self.assertEqual(len(self.runtime.agent(target)['reviewSchedules']), 1)

    def test_pending_review_survives_restart_without_duplicate(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        self.tick(2800)
        event = self.events()[0]
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.state, f.WorkspaceServer)
        self.tick(99999)
        self.assertEqual(self.events(), [event])
        self.assertEqual(self.schedule(target)['lastEventId'], event['id'])

    def test_schedule_changes_never_cancel_a_dispatched_review(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        self.tick(2800)
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='dispatching' WHERE id=?", (self.events()[0]['id'],))
        self.configure(target, reviewer, now=2801, expected_revision=1, enabled=False, removed=True)
        self.assertEqual(self.events()[0]['status'], 'dispatching')
        self.configure(target, reviewer, now=3000, expected_revision=2)
        self.tick(4800)
        self.assertEqual(len(self.events()), 1)

    def test_validation_and_atomic_conflicts(self):
        target, reviewer = self.pair()
        for invalid in [True, 0, 10081, 1.5, '30', None]:
            with self.subTest(interval=invalid), self.assertRaises(ValueError):
                self.configure(target, reviewer, interval_minutes=invalid)
        for changes in [{'enabled': 1}, {'removed': True}, {'expected_revision': True}, {'bogus': 'x'}]:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.configure(target, reviewer, **changes)
        for bad in [target, 'missing', '', None]:
            with self.subTest(reviewer=bad), self.assertRaises(ValueError):
                self.configure(target, bad)
        self.update(reviewer, deletedAt=1)
        with self.assertRaisesRegex(ValueError, 'deleted'):
            self.configure(target, reviewer)
        self.assertNotIn('reviewSchedules', self.runtime.agent(target))

    def test_pending_dispatched_uncertain_and_delivered_turn_do_not_overlap(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        self.tick(2800)
        event = self.events()[0]
        for status in ['pending', 'reserved', 'dispatching', 'uncertain', 'delivered']:
            with self.runtime.db() as db:
                db.execute('UPDATE runtime_events SET status=?,turn_id=? WHERE id=?', (status, 'turn', event['id']))
            self.update(reviewer, status='completed', inFlight=False)
            self.tick(20000)
            self.assertEqual(len(self.events()), 1, status)
        self.completed(target, reviewer, 'turn')
        self.tick(21000)
        self.assertEqual(len(self.events()), 1)
        self.assertEqual(self.schedule(target)['status'], 'unchanged')
        with self.runtime.db() as db:
            self.runtime.item(db, target, 'request', 'user', 'Correct the prior interface requirement')
        self.tick(23000)
        self.assertEqual(len(self.events()), 2)
        self.assertIn('Correct the prior interface requirement', self.events()[-1]['text'])

    def test_paused_deleted_and_failure_hold_never_wake_or_unpause(self):
        for side in ['target', 'reviewer']:
            for fields in [{'autoWake': False}, {'deletedAt': 1}, {'nativeFailureHold': True},
                           {'threadId': 'blocked-thread', 'nativeThreadBlock': {
                               'threadId': 'blocked-thread', 'error': {'codexErrorInfo': 'misalignmentPolicyViolation'}}}]:
                with self.subTest(side=side, fields=fields):
                    target, reviewer = self.pair()
                    self.configure(target, reviewer)
                    agent = target if side == 'target' else reviewer
                    self.update(agent, **fields)
                    before = len(self.events())
                    self.tick(2800)
                    self.assertEqual(len(self.events()), before)
                    self.assertEqual(self.schedule(target)['status'], 'blocked')
                    for key, value in fields.items():
                        self.assertEqual(self.runtime.agent(agent)[key], value)

    def test_disable_or_target_pause_cancels_only_pending_and_reenable_can_review(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        self.tick(2800)
        self.configure(target, reviewer, now=2801, expected_revision=1, enabled=False)
        self.assertEqual(self.events()[0]['status'], 'cancelled')
        self.configure(target, reviewer, now=3000, expected_revision=2)
        self.update(reviewer, status='waiting')
        self.tick(4800)
        self.assertEqual(len(self.events()), 2)
        self.update(target, autoWake=False)
        self.tick(4801)
        self.assertEqual(self.events()[-1]['status'], 'cancelled')
        self.assertFalse(self.runtime.agent(target)['autoWake'])

    def test_failed_or_interrupted_review_retries_only_after_recovery_and_due_timer(self):
        for outcome in ['failed', 'interrupted']:
            with self.subTest(outcome=outcome):
                target, reviewer = self.pair()
                self.configure(target, reviewer)
                self.tick(2800)
                self.runtime.dispatch()
                f.eventually(lambda: self.runtime.agent(reviewer).get('turnId'))
                active = self.runtime.agent(reviewer)
                self.runtime.server.notify({'method': 'turn/completed', 'params': {
                    'threadId': active['threadId'], 'turn': {'id': active['turnId'], 'status': outcome}}})
                f.eventually(lambda: not self.runtime.agent(reviewer).get('inFlight'))
                count = len(self.events())
                self.tick(4600)
                self.assertEqual(len(self.events()), count)
                self.assertEqual(self.schedule(target)['status'], 'blocked')
                # A later user recovery can complete another turn. The review
                # outcome must still come from its exact persisted receipt.
                self.update(reviewer, nativeFailureHold=False, status='completed',
                            lastCompletedTurn='later-user-turn', lastCompletedTurnStatus='completed')
                self.tick(4601)
                self.assertEqual(len(self.events()), count + 1)
                self.assertEqual(self.schedule(target)['lastOutcome'], outcome)
                self.tick(4602)
                self.assertEqual(len(self.events()), count + 1)
                self.configure(target, reviewer, now=4603, expected_revision=1, enabled=False)

    def test_full_fingerprint_catches_changes_outside_display_excerpt_and_keeps_command_failure(self):
        target, reviewer = self.pair()
        tool = {'type': 'commandExecution', 'command': 'run ' + 'x' * 4000,
                'aggregatedOutput': 'Start\n' + 'x' * 8000 + '\nFAILED assertion at the end',
                'status': 'failed', 'exitCode': 17, 'error': 'Expected 2, received 3'}
        with self.runtime.db() as db:
            self.runtime.item(db, target, 'large-command', 'output', json.dumps(tool), 'commandExecution')
        self.configure(target, reviewer)
        self.tick(2800)
        snapshot = json.loads(self.events()[0]['text'].split('Target context snapshot:\n', 1)[1])
        command = json.loads(snapshot['recentOutcomes'][-1]['text'])
        self.assertEqual(command['exitCode'], 17)
        self.assertEqual(command['status'], 'failed')
        self.assertIn('FAILED assertion at the end', command['output'])
        self.assertIn('Expected 2, received 3', command['error'])
        self.completed(target, reviewer)
        with self.runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_items WHERE id=?', (target + ':large-command',)).fetchone()
            item = json.loads(row[0])
            tool['aggregatedOutput'] = tool['aggregatedOutput'][:4000] + 'CHANGE' + tool['aggregatedOutput'][4006:]
            item['text'] = json.dumps(tool)
            db.execute('UPDATE runtime_items SET record=? WHERE id=?', (json.dumps(item), target + ':large-command'))
        self.tick(4600)
        self.assertEqual(len(self.events()), 2)
        self.assertIn('orchestration_chat_read(room_id=', self.events()[0]['text'])

    def test_busy_reviewer_gets_one_queued_review_and_empty_target_does_not_wake(self):
        target, reviewer = self.pair()
        self.configure(target, reviewer)
        self.update(reviewer, inFlight=True, status='running')
        self.tick(2800)
        self.assertEqual(len(self.events()), 1)
        self.assertEqual(self.schedule(target)['status'], 'queued')
        self.assertEqual(self.runtime.agent(reviewer)['status'], 'running')
        self.assertTrue(self.runtime.agent(reviewer)['inFlight'])
        self.update(reviewer, inFlight=False, status='completed')
        self.tick(2801)
        self.assertEqual(len(self.events()), 1)
        self.tick(4600)
        self.assertEqual(len(self.events()), 1)
        empty_agent = self.lead('Empty')
        empty, fresh = empty_agent['id'], self.worker(empty_agent, 'Fresh reviewer', prompt='')['id']
        self.configure(empty, fresh)
        self.tick(2800)
        self.assertEqual(self.schedule(empty)['reason'], 'The target chat has no messages')

    def test_shared_room_same_team_messages_both_directions_and_viewer_boundary(self):
        target, reviewer = self.pair()
        outsider = self.lead('Outsider')['id']
        self.configure(target, reviewer)
        room = self.schedule(target)['roomId']
        self.runtime.chat_message(reviewer, target, 'Evidence: test fails on the requested input.', 'finding')
        self.runtime.chat_message(target, reviewer, 'Please check the updated test.', 'reply')
        self.assertEqual(len(self.runtime.chat_read(room, reviewer)['messages']), 2)
        self.assertEqual(len(self.runtime.chat_read(room, target)['messages']), 2)
        self.assertEqual(self.runtime.chat_read(room, target)['room']['reviewTargets'], [target])
        with self.assertRaisesRegex(ValueError, 'not a participant'):
            self.runtime.chat_read(room, outsider)
        self.configure(reviewer, target)
        self.assertEqual(self.runtime.chat_read(room, reviewer)['room']['reviewTargets'], sorted([target, reviewer]))

    def test_multiple_and_reciprocal_assignments_preserve_runtime_queue_state(self):
        target, reviewer = self.pair()
        second = self.worker(self.runtime.agent(target), 'Second reviewer', prompt='')['id']
        with self.runtime.db() as db:
            self.runtime.item(db, reviewer, 'own-request', 'user', 'Review my work too')
        self.configure(target, reviewer)
        self.configure(target, second, interval_minutes=10)
        self.configure(reviewer, target)
        self.tick(2800)
        self.assertEqual(len(self.events()), 3)
        self.assertEqual(len(self.runtime.agent(target)['reviewSchedules']), 2)
        for agent in [target, reviewer, second]:
            self.assertEqual(self.runtime.agent(agent)['status'], 'queued')
        self.tick(50000)
        self.assertEqual(len(self.events()), 3)

    def test_prompt_bound_preserves_original_latest_requests_and_dispatches_natively(self):
        target, reviewer = self.pair()
        self.runtime.send(target, 'Original user task', 'original')
        self.runtime.send(target, 'Newest user correction', 'newest')
        with self.runtime.db() as db:
            for index in range(12):
                self.runtime.item(db, target, str(index), 'output', 'x' * 20000, 'commandExecution')
        self.configure(target, reviewer)
        self.tick(2800)
        event = self.events()[0]
        self.assertLess(len(event['text']), 30000)
        for text in ['Original user task', 'Newest user correction', target, self.schedule(target)['roomId'], str(self.project),
                     'Do not edit the target files', 'orchestration_message']:
            self.assertIn(text, event['text'])
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(reviewer).get('turnId'))
        calls = [params for method, params in self.runtime.server.calls if method == 'turn/start']
        self.assertTrue(any('one review request' in params['input'][0]['text'] for params in calls))


if __name__ == '__main__':
    unittest.main()
