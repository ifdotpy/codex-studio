#!/usr/bin/env python3
"""Pending notification reconciliation. Fake native only."""

import importlib.util
import json
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('wake_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_wakeups import pending_batch


class WakeupContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    start = f.WorkspaceContract.start
    agent_update = f.WorkspaceContract.agent_update
    events = f.WorkspaceContract.events
    tool = f.WorkspaceContract.tool

    def calls(self):
        return [params for method, params in self.runtime.server.calls if method == 'turn/start']

    def complete(self, agent, text='Done; no further action.'):
        current = self.runtime.agent(agent['id'])
        self.runtime.server.complete(current['threadId'], current['turnId'], text)
        f.eventually(lambda: not self.runtime.agent(agent['id']).get('inFlight'))

    def dispatch(self, agent):
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(agent['id']).get('inFlight'))
        f.eventually(lambda: self.runtime.agent(agent['id']).get('turnId'))

    def quiet_dispatch(self, agent):
        before = len(self.calls())
        self.runtime.dispatch()
        self.assertFalse(self.runtime.agent(agent['id']).get('inFlight'))
        self.assertEqual(len(self.calls()), before)

    def answer(self, lead, complaint):
        self.runtime.complaint(lead['id'], {'action': 'respond', 'complaint_id': complaint['id'],
            'text': 'Verified.', 'status': 'resolved'}, 'answer:' + complaint['id'])

    def test_answered_complaint_does_not_start_empty_turn(self):
        lead = self.start(self.lead())
        c = self.runtime.complaint(lead['id'], {'action': 'submit', 'text': 'Verify this.'}, 'c', user=True)
        self.answer(lead, c)
        self.complete(lead)
        self.quiet_dispatch(lead)
        self.assertEqual(self.events(lead, 'complaint')[0]['status'], 'stored_only')

    def test_mixed_complaint_reminder_preserves_unanswered_and_unknown_deliveries(self):
        lead = self.start(self.lead())
        one, two = [self.runtime.complaint(lead['id'], {'action': 'submit', 'text': 'Verify ' + key}, key, user=True)
                    for key in ('one', 'two')]
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enqueue(db, self.runtime.agent(lead['id'], db), 'complaint',
                                 self.runtime.complaint_message(db, [one, two]), 'mixed')
            self.runtime.enqueue(db, self.runtime.agent(lead['id'], db), 'complaint', 'Unknown legacy reminder', 'unknown')
            self.runtime.enqueue(db, self.runtime.agent(lead['id'], db), 'complaint',
                                 self.runtime.complaint_message(db, [one]), 'uncertain')
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id='uncertain'")
        self.answer(lead, one)
        statuses = {row['id']: row['status'] for row in self.events(lead, 'complaint')}
        self.assertEqual(statuses['mixed'], 'pending')
        self.assertEqual(statuses['unknown'], 'pending')
        self.assertEqual(statuses['uncertain'], 'uncertain')
        self.answer(lead, two)
        self.assertEqual(next(row for row in self.events(lead) if row['id'] == 'mixed')['status'], 'stored_only')

    def task(self, lead, owner):
        return self.runtime.work_action(lead['id'], {'action': 'create', 'title': 'Task', 'owner': owner['id']}, actor=lead['id'])

    def submit(self, task, actor):
        return self.runtime.work_action(actor['id'], {'action': 'submit', 'task_id': task['id'],
            'result': 'Implemented.', 'checks': 'Tests passed.', 'revision': 'fixture'}, actor=actor['id'])

    def decide(self, task, lead, actor=None):
        return self.runtime.work_action(lead['id'], {'action': 'accept', 'task_id': task['id'],
            'result': 'Verified.'}, actor=actor)

    def test_self_task_accepted_without_extra_turn(self):
        lead = self.start(self.lead())
        task = self.submit(self.task(lead, lead), lead)
        self.decide(task, lead, lead['id'])
        self.complete(lead)
        self.quiet_dispatch(lead)
        self.assertEqual(self.events(lead, 'work_review'), [])
        self.assertEqual(self.events(lead, 'work_decision'), [])

    def test_worker_and_user_decisions_remain_visible_in_single_mode(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        task = self.task(lead, worker)
        self.agent_update(lead, agentMode='single')
        task = self.submit(task, worker)
        self.assertEqual(self.events(lead, 'work_review')[0]['status'], 'pending')
        self.decide(task, lead, lead['id'])
        self.assertEqual(self.events(lead, 'work_review')[0]['status'], 'stored_only')
        self.assertEqual(self.events(worker, 'work_decision')[0]['status'], 'pending')
        own = self.submit(self.task(lead, lead), lead)
        self.decide(own, lead)
        self.assertEqual(self.events(lead, 'work_decision')[0]['status'], 'pending')

    def test_decision_preserves_unrelated_and_inflight_reviews(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        first = self.submit(self.task(lead, worker), worker)
        second = self.submit(self.task(lead, worker), worker)
        key = 'work-result:' + first['results'][-1]['id']
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='uncertain' WHERE id=?", (key,))
        self.decide(first, lead, lead['id'])
        self.assertEqual([row['status'] for row in self.events(lead, 'work_review')], ['uncertain', 'pending'])

    def progress(self, worker, lead, version, key=None):
        return self.runtime.chat_message(worker['id'], lead['id'], 'Progress version ' + str(version),
            key or 'p' + str(version), importance='progress', progress_key='same-task', progress_version=version)

    def test_65_progress_versions_need_one_turn_with_full_history(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        self.complete(lead)
        for version in range(65):
            self.progress(worker, lead, version)
        before = len(self.calls())
        with patch.object(self.runtime, 'progress_batch_ready', return_value=True):
            self.dispatch(lead)
        text = self.calls()[-1]['input'][0]['text']
        self.assertIn('Progress version 64', text)
        self.assertNotIn('Progress version 31', text)
        self.assertIn('earlierProgressUpdates', text)
        self.complete(lead)
        self.quiet_dispatch(lead)
        self.assertEqual(len(self.calls()), before + 1)
        self.assertEqual(len(self.events(lead, 'agent_message')), 65)
        self.assertEqual(sum(row['status'] == 'stored_only' for row in self.events(lead, 'agent_message')), 64)
        room = 'private:' + ':'.join(sorted([lead['id'], worker['id']]))
        self.assertEqual(len(self.runtime.chat_read(room, lead['id'])['messages']), 65)

    def test_conflicts_across_pages_preserve_all_versions_and_unknown_events(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        for version in range(65):
            self.progress(worker, lead, version)
        self.progress(worker, lead, 0, 'duplicate')
        with self.runtime.lock, self.runtime.db() as db:
            actor = self.runtime.agent(lead['id'], db)
            self.runtime.enqueue(db, actor, 'agent_message', '{unknown', 'unknown')
            rows = pending_batch(self.runtime, db, actor)
            self.assertEqual(len(rows), 67)
            self.assertTrue(all(row.get('preserveProgress') for row in rows[:66]))
            text = self.runtime.model_event_text(rows[:32])
            for version in range(32):
                self.assertIn('Progress version ' + str(version), text)
            # One conflict has left the pending page; the ambiguity stays durable.
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id='chat:p0:' || ?", (lead['id'],))
            rows = pending_batch(self.runtime, db, actor)
            self.assertEqual(len(rows), 66)
            self.assertTrue(all(row.get('preserveProgress') for row in rows[:-1]))
            self.assertEqual(rows[-1]['text'], '{unknown')









    def test_unknown_complaint_ids_and_payload_types_are_preserved(self):
        lead = self.start(self.lead())
        payloads = ['{"complaints":[{"complaint_id":"unknown"}],"text":"Keep this evidence"}',
                    '{"complaints":[{"complaint_id":{}}]}']
        with self.runtime.lock, self.runtime.db() as db:
            for index, payload in enumerate(payloads):
                self.runtime.enqueue(db, self.runtime.agent(lead['id'], db), 'complaint', payload, 'unknown:' + str(index))
            rows = pending_batch(self.runtime, db, self.runtime.agent(lead['id'], db))
            self.assertEqual(len(rows), 2)
            self.assertTrue(all(row['preserveComplaint'] for row in rows))
            text = self.runtime.model_event_text(rows)
            self.assertIn('Keep this evidence', text)

    def test_budget_guard_preserves_queue_and_resumes_after_budget_change(self):
        lead = self.start(self.lead())
        self.complete(lead)
        self.runtime.send(lead['id'], 'New work', 'budget-user')
        with patch('codex_budget.budget_admission', side_effect=ValueError('Team token budget reached')):
            self.quiet_dispatch(lead)
        self.assertEqual(next(row for row in self.events(lead) if row['id'] == 'budget-user')['status'], 'pending')
        self.assertEqual(self.runtime.agent(lead['id'])['budgetBlocked'], 'Team token budget reached')
        self.assertFalse(self.runtime.agent(lead['id']).get('nativeFailureHold'))
        self.dispatch(lead)
        self.assertNotIn('budgetBlocked', self.runtime.agent(lead['id']))



    def hold_start(self):
        captured = []
        original = self.runtime.start
        with patch.object(self.runtime, 'start', side_effect=lambda *args: captured.append(args)):
            self.runtime.dispatch()
            f.eventually(lambda: bool(captured))
        return original, captured[0]

    def test_complaint_resolved_after_reservation_before_prepare_skips_turn(self):
        lead = self.start(self.lead())
        self.complete(lead)
        complaint = self.runtime.complaint(lead['id'], {'action': 'submit', 'text': 'Verify this.'}, 'reserved', user=True)
        original, args = self.hold_start()
        self.answer(lead, complaint)
        before = len(self.calls())
        with patch.object(self.runtime, 'prepare', wraps=self.runtime.prepare) as prepare:
            original(*args)
            prepare.assert_not_called()
        self.assertEqual(len(self.calls()), before)
        self.assertFalse(self.runtime.agent(lead['id'])['inFlight'])
        self.assertEqual(self.events(lead, 'complaint')[0]['status'], 'stored_only')
        # A stale preparation callback cannot revive the settled attempt.
        original(*args)
        self.assertEqual(len(self.calls()), before)

    def test_complaint_resolved_during_prepare_preserves_mixed_user_input(self):
        lead = self.start(self.lead())
        self.complete(lead)
        complaint = self.runtime.complaint(lead['id'], {'action': 'submit', 'text': 'Verify this.'}, 'during', user=True)
        self.runtime.send(lead['id'], 'Keep the user request exactly.', 'mixed-race-user')
        original, args = self.hold_start()
        prepare = self.runtime.prepare
        def resolve_during_prepare(agent):
            result = prepare(agent)
            self.answer(lead, complaint)
            return result
        before = len(self.calls())
        with patch.object(self.runtime, 'prepare', side_effect=resolve_during_prepare):
            original(*args)
        self.assertEqual(len(self.calls()), before)
        events = {row['id']: row for row in self.events(lead)}
        self.assertEqual(events['mixed-race-user']['status'], 'pending')
        self.assertEqual(events['mixed-race-user']['text'], 'Keep the user request exactly.')
        self.assertFalse(self.runtime.agent(lead['id'])['inFlight'])
        self.dispatch(lead)
        self.assertEqual(len(self.calls()), before + 1)
        self.assertIn('Keep the user request exactly.', self.calls()[-1]['input'][0]['text'])
        self.assertNotIn('No complaints require a response', self.calls()[-1]['input'][0]['text'])

    def test_reserved_exact_work_decision_retires_only_decided_result(self):
        lead = self.start(self.lead())
        worker = self.worker(lead)
        self.complete(lead)
        first = self.submit(self.task(lead, worker), worker)
        second = self.submit(self.task(lead, worker), worker)
        original, args = self.hold_start()
        self.decide(first, lead, lead['id'])
        before = len(self.calls())
        original(*args)
        self.assertEqual(len(self.calls()), before)
        statuses = {row['id']: row['status'] for row in self.events(lead, 'work_review')}
        self.assertEqual(statuses['work-result:' + first['results'][-1]['id']], 'stored_only')
        self.assertEqual(statuses['work-result:' + second['results'][-1]['id']], 'pending')

    def test_reconcile_start_never_retires_submitted_or_uncertain_input(self):
        from codex_wakeups import reconcile_start
        lead = self.start(self.lead())
        self.complete(lead)
        complaint = self.runtime.complaint(lead['id'], {'action': 'submit', 'text': 'Verify this.'}, 'unknown-start', user=True)
        original, args = self.hold_start()
        self.answer(lead, complaint)
        for submitted, status in [(True, 'dispatching'), (False, 'uncertain')]:
            with self.runtime.lock, self.runtime.db() as db:
                current = self.runtime.agent(lead['id'], db)
                current['startAttempt']['submitted'] = submitted
                for row in args[1]:
                    db.execute('UPDATE runtime_events SET status=? WHERE id=?', (status, row['id']))
                self.assertFalse(reconcile_start(self.runtime, db, current, args[1]))
                for row in args[1]:
                    self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', (row['id'],)).fetchone()[0], status)

    def test_projection_marker_preserves_metadata_only_for_submitted_input(self):
        lead = self.start(self.lead())
        self.complete(lead)
        complaint = self.runtime.complaint(lead['id'], {'action': 'submit', 'text': 'Verify this.'}, 'projection-complaint', user=True)
        self.runtime.send(lead['id'], 'Keep this user input.', 'projection-user')
        with self.runtime.db() as db:
            metadata = json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?', ('projection-user',)).fetchone()[0])
            metadata['fixtureIdentity'] = 'exact'
            db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?', (json.dumps(metadata), 'projection-user'))
        original, args = self.hold_start()
        self.answer(lead, complaint)
        original(*args)
        with self.runtime.db() as db:
            for row in args[1]:
                saved = db.execute('SELECT record FROM runtime_event_meta WHERE id=?', (row['id'],)).fetchone()
                self.assertFalse(saved and json.loads(saved[0]).get('modelEventProjection'))
        self.dispatch(lead)
        with self.runtime.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_event_meta WHERE id=?', ('projection-user',)).fetchone()[0])
            self.assertEqual(saved['modelEventProjection'], 1)
            for key, value in metadata.items():
                self.assertEqual(saved[key], value)


if __name__ == '__main__':
    unittest.main()
