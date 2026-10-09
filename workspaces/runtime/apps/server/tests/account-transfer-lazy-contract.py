#!/usr/bin/env python3
"""Instant account rebinding and automatic native history transfer."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
from pathlib import Path
import threading
import time
import unittest
import uuid

spec = importlib.util.spec_from_file_location('account_transfer_fixture', Path(__file__).with_name('account-transfer-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_account_transfer import AccountTransfers


class LazyTransferContract(unittest.TestCase):
    def setUp(self):
        self.t = fixture.TransferContract('test_transfer_keeps_identity_history_queue_and_settings')
        self.t.setUp()
        self.t.drive_lazy_for_tests = False
        self.rt = self.t.runtime
        self.store = self.t.store
        self.aid = self.t.lead_agent['id']

    def tearDown(self):
        self.t.tearDown()

    def test_finish_history_now_completes_without_input_and_duplicate_calls(self):
        op = self.t.start_transfer()
        self.assertFalse(self.rt.agent(self.aid)['accountTransfer']['canFinishHistory'])
        self.store.action(op['id'], 'finish_history')
        self.store.action(op['id'], 'finish_history')
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.tick()
        self.assertEqual(len(self.t.pending), 1)
        self.assertEqual(self.rt.agent(self.aid)['accountTransfer']['movingNow'], 1)
        from codex_runtime import PreparationPending
        with self.assertRaises(PreparationPending) as wait:
            self.store.before_start(self.rt.agent(self.aid))
        self.t.set_agent(self.aid, status='starting', inFlight=True,
                         startAttempt={'submitted': False, 'events': []})
        self.t.complete_fork()
        self.t.until(lambda: not self.store.running)
        self.assertTrue(wait.exception.future.done())
        self.assertEqual(self.store.before_start(self.rt.agent(self.aid))['threadId'], 'target-thread-0')
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')
        self.store.action(op['id'], 'finish_history')
        self.t.tick()
        self.assertEqual(len(self.t.pending), 1)
        with self.rt.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events WHERE agent=?', (self.aid,)).fetchone()[0], 0)

    def test_finish_history_intent_survives_restart_before_submission(self):
        op = self.t.start_transfer()
        self.store.action(op['id'], 'finish_history')
        self.store = AccountTransfers(self.rt)
        self.store.copy_history = lambda *args: Path('/fixture/import.jsonl')
        with self.rt.lock, self.rt.db() as db:
            self.store.tick(self.rt.records(db, 'agents'))
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')

    def test_finish_history_preserves_queued_input(self):
        self.t.set_agent(self.aid, status='failed', autoWake=True)
        op = self.t.start_transfer()
        self.rt.send(self.aid, 'Keep this queued', 'finish-input', delivery='after_tool')
        self.store.action(op['id'], 'finish_history')
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        self.t.until(lambda: not self.store.running)
        with self.rt.db() as db:
            self.assertEqual([tuple(row) for row in db.execute('SELECT id,status FROM runtime_events WHERE agent=?', (self.aid,))],
                             [('finish-input', 'pending')])

    def test_automatic_history_does_not_resume_failed_agent(self):
        self.t.set_agent(self.aid, status='failed', autoWake=True)
        op = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        self.t.until(lambda: not self.store.running)
        with self.rt.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events WHERE agent=?', (self.aid,)).fetchone()[0], 0)

    def test_finish_history_waits_for_active_work_without_interrupting(self):
        op = self.t.start_transfer()
        self.t.set_agent(self.aid, status='running', inFlight=True, turnId='active-turn')
        self.store.action(op['id'], 'finish_history')
        self.t.tick()
        self.assertEqual(self.t.pending, [])
        self.assertEqual(self.t.native_calls, [])
        self.assertEqual(self.rt.agent(self.aid)['accountTransfer']['waiting'], 'Waiting for the current turn')
        self.t.set_agent(self.aid, status='completed', inFlight=False, turnId=None)
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        self.t.until(lambda: not self.store.running)

    def test_finish_history_does_not_repeat_an_unknown_submission(self):
        op = self.t.start_transfer()
        with self.rt.lock, self.rt.db() as db:
            record = self.store.get(db, op['id'])
            record['members'][self.aid].update(phase='unknown', error='Native receipt unknown',
                                              nativeMethod='thread/fork', submittedAt=time.time())
            self.store.save(db, record)
        self.store.action(op['id'], 'finish_history')
        self.t.tick()
        self.assertEqual(self.t.pending, [])
        self.assertEqual(self.t.native_calls, [])
        self.assertEqual(self.t.receipt(op['id'])['members'][self.aid]['phase'], 'unknown')

    def test_automatic_history_waits_for_pending_model_list_without_manual_retry(self):
        from codex_catalog import CatalogPending
        from unittest.mock import patch
        op = self.t.start_transfer()
        original = self.rt.catalog
        pending = True
        now = [time.time()]

        def catalog(key='default'):
            if pending:
                raise CatalogPending('Existing metadata request is pending')
            return original(key)

        self.rt.catalog = catalog
        with patch('codex_account_transfer.time.time', side_effect=lambda: now[0]):
            self.t.tick()
            self.t.until(lambda: not self.store.running)
            member = self.t.receipt(op['id'])['members'][self.aid]
            self.assertEqual(member['phase'], 'lazy')
            self.assertEqual(member['waiting'], 'Waiting for the destination model list')
            self.assertIsNone(member['error'])
            self.assertEqual(self.t.pending, [])
            pending = False
            now[0] += 1.05
            self.t.tick()
            self.t.until(lambda: len(self.t.pending) == 1)
            self.t.complete_fork()
            self.t.until(lambda: not self.store.running)
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')

    def test_account_choice_alone_completes_history_once_without_model_input(self):
        op = self.t.start_transfer()
        self.assertNotIn('finishHistory', op)
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        repeated = self.store.request(self.aid, self.t.other_key, op['id'])
        self.assertEqual(repeated['id'], op['id'])
        self.t.tick()
        self.assertEqual(len(self.t.pending), 1)
        self.t.complete_fork()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')
        with self.rt.db() as db:
            self.assertEqual(db.execute('SELECT count(*) FROM runtime_events WHERE agent=?', (self.aid,)).fetchone()[0], 0)

    def test_restart_finishes_existing_deferred_history_without_an_extra_action(self):
        op = self.t.start_transfer()
        self.assertNotIn('finishHistory', op)
        self.store = AccountTransfers(self.rt)
        self.store.copy_history = lambda *args: Path('/fixture/import.jsonl')
        with self.rt.lock, self.rt.db() as db:
            self.store.tick(self.rt.records(db, 'agents'))
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        self.t.until(lambda: not self.store.running)
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')

    def test_300_idle_members_rebind_within_one_second_and_without_native_calls(self):
        with self.rt.lock, self.rt.db() as db:
            lead = self.rt.agent(self.aid, db)
            for index in range(299):
                member = copy.deepcopy(lead)
                member.update(id=str(uuid.uuid4()), name=f'Idle {index}', isLead=False,
                              parentId=self.aid, rootId=self.aid, threadId=None,
                              accountKey='default', status='completed', inFlight=False,
                              autoWake=False, created=lead['created'] - 1)
                self.rt.put(db, 'agents', member)
        began = time.monotonic()
        op = self.t.start_transfer()
        elapsed = time.monotonic() - began
        with self.rt.db() as db:
            members = [a for a in self.rt.records(db, 'agents') if a.get('rootId') == self.aid or a['id'] == self.aid]
        self.assertLess(elapsed, 1.0)
        self.assertEqual(len(members), 300)
        self.assertTrue(all(a['accountKey'] == self.t.other_key for a in members))
        self.assertTrue(all(a.get('lazyAccountTransfer', {}).get('id') == op['id'] for a in members))
        self.assertEqual(self.t.native_calls, [])
        self.assertEqual(self.t.pending, [])
        self.assertEqual(self.rt.agent(self.aid)['accountTransfer']['nativeHistoryPending'], 300)

    def test_restart_then_first_start_submits_one_fork_and_commits(self):
        op = self.t.start_transfer()
        self.assertEqual(self.t.native_calls, [])
        restarted = AccountTransfers(self.rt)
        restarted.copy_history = lambda *args: Path('/fixture/import.jsonl')
        errors = []
        worker = threading.Thread(target=lambda: self._before_start(restarted, errors), daemon=True)
        worker.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.assertEqual(self.t.pending[0][0], 'thread/fork')
        self.t.complete_fork()
        worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertEqual(len([x for x in self.t.pending if x[0] == 'thread/fork']), 1)
        agent = self.rt.agent(self.aid)
        self.assertEqual(agent['accountKey'], self.t.other_key)
        self.assertEqual(agent['threadId'], 'target-thread-0')
        self.assertNotIn('lazyAccountTransfer', agent)
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')

    def test_new_thread_without_source_history_stays_loaded_for_first_turn(self):
        self.t.set_agent(self.aid, threadId=None)
        op = self.t.start_transfer()
        errors = []
        worker = threading.Thread(target=self._move_member, args=(op['id'], self.aid, errors), daemon=True)
        worker.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.assertEqual(self.t.pending[0][0], 'thread/start')
        self.t.complete_fork()
        worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(errors, [])
        self.assertIn(self.aid, self.rt.loaded)
        self.assertEqual(self.rt.agent(self.aid)['threadId'], 'target-thread-0')

    def test_two_moved_members_waking_together_fork_once_each(self):
        child = self.rt.create({'name': 'Worker', 'prompt': 'Task'}, parent=self.aid, defer=True)
        self.t.set_agent(child['id'], status='completed', threadId='worker-native', autoWake=False)
        op = self.t.start_transfer()
        members = [self.aid, child['id']]
        errors = []
        workers = [threading.Thread(target=self._move_member, args=(op['id'], aid, errors), daemon=True)
                   for aid in members]
        for worker in workers:
            worker.start()
        self.t.until(lambda: len(self.t.pending) == 2)
        self.assertEqual([method for method, _, _ in self.t.pending], ['thread/fork', 'thread/fork'])
        self.t.complete_fork(0)
        self.t.complete_fork(1)
        for worker in workers:
            worker.join(3)
        self.assertFalse(any(worker.is_alive() for worker in workers))
        self.assertEqual(errors, [])
        self.assertEqual(len([p for p in self.t.pending if p[0] == 'thread/fork']), 2)
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')

    def test_lazy_failure_blocks_with_reason_and_explicit_retry_reuses_no_input(self):
        op = self.t.start_transfer()
        self.rt.send(self.aid, 'Keep this queued', 'lazy-input', delivery='after_tool')
        original = self.t.source_server.call
        failed = False
        def fail_first_read(method, params, timeout=60):
            nonlocal failed
            if method == 'thread/read' and not failed:
                failed = True
                raise RuntimeError('Source history temporarily unavailable')
            return original(method, params, timeout)
        self.t.source_server.call = fail_first_read
        with self.assertRaisesRegex(RuntimeError, 'Native history move blocked'):
            self.store.before_start(self.rt.agent(self.aid))
        member = self.t.receipt(op['id'])['members'][self.aid]
        self.assertEqual(member['phase'], 'blocked')
        self.assertIn('temporarily unavailable', member['error'])
        self.assertEqual(self.t.pending, [])
        with self.rt.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='lazy-input'").fetchone()[0], 'pending')
        self.store.action(op['id'], 'retry')
        worker = threading.Thread(target=self._move_member, args=(op['id'], self.aid, []), daemon=True)
        worker.start()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.t.complete_fork()
        worker.join(3)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len([p for p in self.t.pending if p[0] == 'thread/fork']), 1)
        with self.rt.db() as db:
            self.assertEqual(db.execute("SELECT status FROM runtime_events WHERE id='lazy-input'").fetchone()[0], 'pending')

    def test_cancel_while_native_history_is_lazy_returns_to_source_without_rpc(self):
        op = self.t.start_transfer()
        self.assertEqual(self.rt.agent(self.aid)['accountKey'], self.t.other_key)
        self.assertTrue(self.rt.agent(self.aid).get('lazyAccountTransfer'))
        self.store.action(op['id'], 'cancel')
        agent = self.rt.agent(self.aid)
        self.assertEqual(agent['accountKey'], 'default')
        self.assertNotIn('lazyAccountTransfer', agent)
        self.assertNotIn('accountTransferId', agent)
        self.assertEqual(self.t.native_calls, [])
        self.assertEqual(self.t.pending, [])

    def test_active_member_still_interrupts_then_moves(self):
        self.t.set_agent(self.aid, status='running', inFlight=True, turnId='active-turn')
        op = self.t.start_transfer()
        self.t.tick()
        self.t.until(lambda: any(method == 'turn/interrupt' for method, _ in self.t.native_calls))
        self.t.until(lambda: not self.store.running)
        self.assertNotIn('lazyAccountTransfer', self.rt.agent(self.aid))
        self.assertEqual(self.rt.agent(self.aid)['accountKey'], 'default')
        self.t.set_agent(self.aid, status='completed', inFlight=False, turnId=None)
        self.t.tick()
        self.t.tick()
        self.t.until(lambda: len(self.t.pending) == 1)
        self.assertEqual(self.t.pending[0][0], 'thread/fork')
        self.t.complete_fork()
        self.t.until(lambda: self.t.receipt(op['id'])['status'] == 'completed')
        self.assertEqual(self.rt.agent(self.aid)['accountKey'], self.t.other_key)
        self.assertEqual(self.t.receipt(op['id'])['status'], 'completed')

    def _before_start(self, store, errors):
        try:
            store.before_start(self.rt.agent(self.aid))
        except Exception as error:
            errors.append(str(error))

    def _move_member(self, key, aid, errors):
        try:
            self.store.before_start(self.rt.agent(aid))
        except Exception as error:
            errors.append(str(error))


if __name__ == '__main__':
    unittest.main()
