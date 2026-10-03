#!/usr/bin/env python3
"""Hot delegation switches and arbitrary catalog models, without native model calls."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

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

    def concurrency(self, value, revision=None, request=None):
        return self.rt.conversation_settings(self.lead['id'], {
            'subagent_concurrency': value,
            'expected_mode_revision': self.rt.agent(self.lead['id'])['agentModeRevision'] if revision is None else revision,
            'request_id': request or str(uuid.uuid4())})

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

    def test_concurrency_bounds_canonical_replay_and_mode_projection(self):
        zero = self.concurrency(0, revision=0, request='zero')
        self.assertEqual((zero['concurrency'], zero['agentMode'], zero['agentModeRevision']), (0, 'single', 1))
        self.assertEqual(self.rt.agent(self.worker['id'])['status'], self.worker['status'])
        with self.assertRaisesRegex(ValueError, 'Single agent'):
            self.rt.create({'name': 'Blocked', 'prompt': 'No new worker'}, self.lead['id'])
        with self.assertRaisesRegex(ValueError, 'Single agent'):
            self.rt.send(self.worker['id'], 'No new worker turn')
        for invalid in (-1, 513, True, 1.5):
            with self.subTest(invalid=invalid), self.assertRaisesRegex(ValueError, '0 to 512'):
                self.concurrency(invalid)
        maximum = self.concurrency(512)
        self.assertEqual((maximum['concurrency'], maximum['agentMode']), (512, 'multi'))
        self.assertEqual(maximum['maxAgents'], 514)
        self.assertEqual(self.rt.agent(self.lead['id'])['concurrency'], 512)
        # The exact old request returns the latest canonical value without
        # applying the stale request a second time.
        replay = self.concurrency(0, revision=0, request='zero')
        self.assertEqual((replay['concurrency'], replay['agentModeRevision']), (512, 2))
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.concurrency(1, revision=0, request='zero')

    def test_legacy_single_record_migrates_before_mode_is_derived(self):
        with self.rt.lock, self.rt.db() as db:
            legacy = self.rt.agent(self.lead['id'], db)
            legacy.update(concurrency=32, agentMode='single', agentModeRevision=4)
            legacy.pop('subagentConcurrencyVersion', None)
            self.rt.put(db, 'agents', legacy)
        migrated = self.rt.agent(self.lead['id'])
        self.assertEqual((migrated['concurrency'], migrated['agentMode'],
                          migrated['subagentConcurrencyVersion']), (0, 'single', 2))
        saved = self.concurrency(5, revision=4)
        self.assertEqual((saved['concurrency'], saved['agentMode']), (5, 'multi'))
        with self.rt.db() as db:
            persisted = self.rt.agent(self.lead['id'], db)
            self.assertEqual((persisted['concurrency'], persisted['subagentConcurrencyVersion']), (5, 2))

    def test_public_create_accepts_513th_queued_worker_for_512_slots(self):
        maximum = self.concurrency(512)
        self.assertEqual(maximum['maxAgents'], 514)
        created = None
        for index in range(512):
            created = self.rt.create({'name': f'Worker {index}', 'prompt': 'Work',
                                      '_worktree': False}, self.lead['id'])
        self.assertIsNotNone(created)
        self.assertEqual(created['status'], 'queued')
        self.assertEqual(self.rt.agent(self.lead['id'])['maxAgents'], 514)
        with self.rt.db() as db:
            self.assertEqual(len(self.rt.team_agents(db, self.lead['id'])), 514)

    def test_configure_concurrency_uses_integer_cas_receipt_contract(self):
        with self.assertRaisesRegex(ValueError, 'revision'):
            self.rt.configure(self.lead['id'], {'concurrency': 7})
        for invalid in (True, 1.5, '7'):
            with self.subTest(invalid=invalid), self.assertRaisesRegex(ValueError, 'integer from 0 to 512'):
                self.rt.configure(self.lead['id'], {'concurrency': invalid,
                    'expected_mode_revision': 0, 'request_id': 'invalid-config'})
        result = self.rt.configure(self.lead['id'], {'concurrency': 7,
            'expected_mode_revision': 0, 'request_id': 'configured'})
        self.assertEqual((result['concurrency'], result['agentModeRevision']), (7, 1))
        self.assertEqual(self.rt.configure(self.lead['id'], {'concurrency': 7,
            'expected_mode_revision': 0, 'request_id': 'configured'}), result)

    def test_lowering_limit_keeps_inflight_worker_and_pending_queue(self):
        with self.rt.lock, self.rt.db() as db:
            worker = self.rt.agent(self.worker['id'], db)
            worker.update(status='running', inFlight=True, threadId='native-worker', turnId='active-turn')
            self.rt.put(db, 'agents', worker)
        queued = self.rt.create({'name': 'Queued', 'prompt': 'Remain queued'}, self.lead['id'])
        self.concurrency(1)
        before = self.rt.agent(self.worker['id'])
        self.concurrency(0)
        after = self.rt.agent(self.worker['id'])
        self.assertEqual((after['status'], after['inFlight'], after['turnId']),
                         (before['status'], before['inFlight'], before['turnId']))
        self.assertEqual(self.rt.agent(queued['id'])['status'], 'queued')
        self.concurrency(2)
        self.assertEqual(self.rt.agent(queued['id'])['status'], 'queued')

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
        # The preceding confirmed result carries the updated policy. A second
        # result in the same context does not need to repeat it.
        with self.rt.lock, self.rt.db() as db:
            text = self.rt.model_turn_context(db, self.rt.agent(self.lead['id'], db), 'context-event')
            self.assertIn('Single agent mode', text)
        self.mode('multi')
        result = self.rt.model_tool_result(self.lead['id'], 'test', {'success': True, 'contentItems': []})
        self.assertIn('Multi agent mode', result['contentItems'][-1]['text'])

    def test_default_chat_creation_needs_no_model_catalog(self):
        self.rt.catalog = lambda account='default': (_ for _ in ()).throw(AssertionError('Catalog must not run'))
        request = {'id': str(uuid.uuid4()), 'cwd': self.tmp.name, 'reuse_empty': False}
        created = self.rt.new_lead(request)
        self.assertEqual(created['model'], 'gpt-6-astra')
        self.assertEqual(created['status'], 'idle')
        self.assertIsNone(created['threadId'])
        self.assertEqual(self.rt.new_lead(request)['id'], created['id'])

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
