#!/usr/bin/env python3
"""Hot delegation switches and arbitrary catalog models, without native model calls."""
import copy
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest
import uuid

spec = importlib.util.spec_from_file_location('mode_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_agent_modes import guidance
from codex_chat_reviews import review_schedule, review_tick


class AgentModes(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.rt = f.ControlledRuntime(Path(self.tmp.name), f.f.FakeServer)
        self.rt.catalog = lambda account='default': copy.deepcopy(f.CATALOG)
        self.lead = self.rt.new_lead({'cwd': self.tmp.name})
        self.worker = self.rt.create({'name': 'Worker', 'prompt': 'Check'}, self.lead['id'])

    def tearDown(self):
        self.rt.close()
        self.tmp.cleanup()

    def mode(self, value='single', revision=None, request=None, **extra):
        return self.rt.conversation_settings(self.lead['id'], {'agent_mode': value,
            'expected_mode_revision': self.rt.agent(self.lead['id'])['agentModeRevision'] if revision is None else revision,
            'request_id': request or str(uuid.uuid4()), **extra})

    def test_busy_switch_preserves_turn_workers_and_queue(self):
        with self.rt.lock, self.rt.db() as db:
            lead = self.rt.agent(self.lead['id'], db)
            lead.update(status='running', inFlight=True, threadId='native', turnId='turn')
            self.rt.put(db, 'agents', lead)
            before = [dict(r) for r in db.execute('SELECT * FROM runtime_events')]
        self.rt.loaded.add(lead['id'])
        worker = self.rt.agent(self.worker['id'])
        result = self.mode()
        self.assertEqual((result['agentMode'], result['agentModeRevision']), ('single', 1))
        for key in ['status', 'inFlight', 'threadId', 'turnId', 'epoch', 'autoWake']:
            self.assertEqual(result[key], lead[key])
        self.assertEqual(self.rt.agent(worker['id']), worker)
        self.assertIn(lead['id'], self.rt.loaded)
        with self.rt.db() as db:
            self.assertEqual([dict(r) for r in db.execute('SELECT * FROM runtime_events')], before)
        self.assertFalse(self.rt.thread_config()['features.multi_agent'])

    def test_idempotency_conflicts_stale_revision_and_current_replay(self):
        first = self.mode(request='switch', revision=0)
        self.assertEqual(self.mode(request='switch', revision=0), first)
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.mode('multi', revision=0, request='switch')
        with self.assertRaisesRegex(ValueError, 'changed'):
            self.mode('multi', revision=0)
        current = self.mode('multi')
        self.assertEqual(current['agentModeRevision'], 2)
        self.assertEqual(self.mode(request='switch', revision=0)['agentMode'], 'multi')
        self.assertEqual(self.mode('multi')['agentModeRevision'], 2)

    def test_invalid_requests_leave_no_receipt(self):
        for extra in [{'model': 'worker'}, {'next_turn': True}, {'agent_mode': 'bad'}, {'expected_mode_revision': True}, {'request_id': ''}]:
            body = {'agent_mode': 'single', 'expected_mode_revision': 0, 'request_id': 'bad', **extra}
            with self.assertRaises(ValueError):
                self.rt.conversation_settings(self.lead['id'], body)
        with self.assertRaisesRegex(ValueError, 'lead chat'):
            self.rt.conversation_settings(self.worker['id'], {'agent_mode': 'single', 'expected_mode_revision': 0, 'request_id': 'worker'})
        with self.rt.db() as db:
            self.assertFalse(db.execute("SELECT 1 FROM runtime_operation_receipts WHERE id IN ('bad','worker')").fetchone())

    def test_spawn_send_broadcast_and_peer_routes_reject_without_writes(self):
        other = self.rt.create({'name': 'Other', 'prompt': 'Check'}, self.lead['id'])
        self.mode()
        with self.rt.db() as db:
            before = {name: db.execute('SELECT count(*) FROM runtime_' + name).fetchone()[0]
                      for name in ['agents', 'events', 'rooms', 'chat_messages']}
        actions = [lambda: self.rt.create({'name': 'New', 'prompt': 'Task'}, self.lead['id']),
                   lambda: self.rt.spawn_agents(self.lead, {'agents': [{'name': 'New', 'prompt': 'Task'}]}, 'spawn'),
                   lambda: self.rt.send(self.worker['id'], 'new work'),
                   lambda: self.rt.send(self.worker['id'], 'new work', manual=False, sender=self.lead['id'], sender_epoch=0),
                   lambda: self.rt.chat_message(self.lead['id'], 'broadcast', 'task', 'broadcast'),
                   lambda: self.rt.chat_message(self.worker['id'], other['id'], 'task', 'peer'),
                   lambda: self.rt.native_action(self.worker['id'], 'review')]
        for action in actions:
            with self.assertRaisesRegex(ValueError, 'Single agent'):
                action()
        with self.rt.db() as db:
            self.assertEqual({name: db.execute('SELECT count(*) FROM runtime_' + name).fetchone()[0] for name in before}, before)
        self.rt.chat_message(self.worker['id'], 'lead', 'result', 'result', importance='result')
        self.rt.send(self.lead['id'], 'User follow-up')
        self.mode('multi')
        self.rt.send(self.worker['id'], 'new work')

    def test_prior_delivery_receipt_survives_switch(self):
        result = self.rt.send(self.worker['id'], 'already accepted', 'accepted')
        self.mode()
        replay = self.rt.send(self.worker['id'], 'already accepted', 'accepted')
        self.assertEqual(replay['id'], result['id'])
        self.assertEqual(replay['status'], 'pending')
        with self.rt.db() as db:
            self.assertEqual(db.execute("SELECT count(*) FROM runtime_events WHERE id='accepted'").fetchone()[0], 1)

    def test_assigned_work_finishes_new_assignments_and_rejections_wait(self):
        task = self.rt.work_action(self.lead['id'], {'action': 'create', 'title': 'Work', 'owner': self.worker['id']})
        self.mode()
        self.rt.work_action(self.worker['id'], {'action': 'claim', 'task_id': task['id']}, actor=self.worker['id'])
        with self.assertRaisesRegex(ValueError, 'Single agent'):
            self.rt.work_action(self.lead['id'], {'action': 'create', 'title': 'New', 'owner': self.worker['id']})
        self.rt.work_action(self.worker['id'], {'action': 'submit', 'task_id': task['id'], 'result': 'Done', 'checks': 'Checked', 'revision': 'source'}, actor=self.worker['id'])
        with self.assertRaisesRegex(ValueError, 'Single agent'):
            self.rt.work_action(self.lead['id'], {'action': 'reject', 'task_id': task['id'], 'result': 'Redo'})
        accepted = self.rt.work_action(self.lead['id'], {'action': 'accept', 'task_id': task['id'], 'result': 'Checked'})
        self.assertEqual(accepted['status'], 'accepted')

    def test_timer_blocks_future_events_preserves_pending_and_resumes_interval(self):
        with self.rt.lock, self.rt.db() as db:
            target = self.rt.agent(self.lead['id'], db)
            review_schedule(self.rt, db, target, {'review_schedule': {'reviewer_id': self.worker['id'], 'interval_minutes': 1, 'expected_revision': 0}})
            target = self.rt.agent(self.lead['id'], db)
            target['reviewSchedules'][0]['nextAt'] = 0
            self.rt.put(db, 'agents', target)
            self.rt.enqueue(db, self.worker, 'chat_review', 'Prior accepted review', 'prior-review')
            target = self.rt.agent(self.lead['id'], db)
            target['reviewSchedules'][0]['lastEventId'] = 'prior-review'
            self.rt.put(db, 'agents', target)
        self.mode()
        with self.rt.lock, self.rt.db() as db:
            review_tick(self.rt, db, 100)
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='prior-review'").fetchone()[0], 'pending')
            entry = self.rt.agent(self.lead['id'], db)['reviewSchedules'][0]
            self.assertEqual(entry['status'], 'blocked')
            self.assertIn('Single agent', entry['reason'])
        self.mode('multi')
        with self.rt.lock, self.rt.db() as db:
            review_tick(self.rt, db, 200)
            self.assertEqual(self.rt.agent(self.lead['id'], db)['reviewSchedules'][0]['nextAt'], 260)

    def test_canonical_legacy_projection_and_restart(self):
        with self.rt.lock, self.rt.db() as db:
            row = self.rt.agent(self.lead['id'], db)
            for key in ['agentMode', 'agentModeRevision', 'agentModeSupported']:
                row.pop(key)
            self.rt.put(db, 'agents', row)
        projected = self.rt.snapshot()['agents']
        lead = next(a for a in projected if a['id'] == self.lead['id'])
        self.assertEqual((lead['agentMode'], lead['agentModeRevision'], lead['agentModeSupported']), ('multi', 0, True))
        self.mode()
        self.rt.close()
        self.rt = f.ControlledRuntime(Path(self.tmp.name), f.f.FakeServer)
        self.assertEqual(self.rt.agent(self.lead['id'])['agentMode'], 'single')

    def test_current_guidance_after_switch_and_on_next_turn(self):
        self.mode()
        result = self.rt.model_tool_result(self.lead['id'], 'test', {'success': True, 'contentItems': []})
        self.assertIn('Single agent mode', result['contentItems'][-1]['text'])
        large = self.rt.model_tool_result(self.lead['id'], 'large', {'success': True,
            'contentItems': [{'type': 'inputText', 'text': 'x' * 20000}]})
        self.assertIn('Single agent mode', large['contentItems'][-1]['text'])
        with self.rt.lock, self.rt.db() as db:
            text = self.rt.model_turn_context(db, self.rt.agent(self.lead['id'], db), 'context-event')
            self.assertIn('Single agent mode', text)
        self.mode('multi')
        result = self.rt.model_tool_result(self.lead['id'], 'test', {'success': True, 'contentItems': []})
        self.assertIn('Multi agent mode', result['contentItems'][-1]['text'])

    def test_model_without_reasoning_and_creation_retry_without_catalog(self):
        catalog = copy.deepcopy(f.CATALOG)
        catalog['data'].append({'model': 'plain', 'supportedReasoningEfforts': []})
        self.rt.catalog = lambda account='default': catalog
        request = {'id': str(uuid.uuid4()), 'cwd': self.tmp.name, 'model': 'plain'}
        created = self.rt.new_lead(request)
        self.assertIsNone(created['effort'])
        self.assertIsNone(created['nativeEffort'])
        self.rt.catalog = lambda account='default': (_ for _ in ()).throw(AssertionError('Catalog must not run'))
        self.assertEqual(self.rt.new_lead(request)['id'], created['id'])
        self.mode()
        with self.assertRaisesRegex(ValueError, 'Single agent'):
            self.rt.create({'name': 'New', 'prompt': 'Task'}, self.lead['id'])
        with self.assertRaisesRegex(ValueError, 'Single agent'):
            self.rt.spawn_agents(self.lead, {'agents': [{'name': 'New', 'prompt': 'Task'}]}, 'new')

    def test_empty_reuse_validates_explicit_model_and_catalog_wait_has_no_lock(self):
        blank = self.rt.new_lead({'cwd': self.tmp.name})
        with self.assertRaisesRegex(ValueError, 'not available'):
            self.rt.new_lead({'previous': blank['id'], 'model': 'unknown'})
        reached, release = threading.Event(), threading.Event()
        def catalog(account):
            reached.set()
            release.wait(2)
            return copy.deepcopy(f.CATALOG)
        self.rt.catalog = catalog
        result = []
        thread = threading.Thread(target=lambda: result.append(self.rt.new_lead({'previous': blank['id'], 'model': 'worker'})))
        thread.start()
        self.assertTrue(reached.wait(1))
        self.assertTrue(self.rt.lock.acquire(timeout=.1))
        self.rt.lock.release()
        release.set()
        thread.join(2)
        self.assertEqual((result[0]['id'], result[0]['model']), (blank['id'], 'worker'))


if __name__ == '__main__':
    unittest.main()
