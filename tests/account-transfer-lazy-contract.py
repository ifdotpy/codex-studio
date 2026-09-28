#!/usr/bin/env python3
"""Instant account rebinding and per-agent lazy native history migration."""
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
        self.rt = self.t.runtime
        self.store = self.t.store
        self.aid = self.t.lead_agent['id']

    def tearDown(self):
        self.t.tearDown()

    def test_300_idle_members_rebind_within_one_second_and_without_native_calls(self):
        with self.rt.lock, self.rt.db() as db:
            lead = self.rt.agent(self.aid, db)
            for index in range(299):
                member = copy.deepcopy(lead)
                member.update(id=str(uuid.uuid4()), name=f'Idle {index}', isLead=False,
                              parentId=self.aid, rootId=self.aid, threadId=None,
                              accountKey='default', status='complete', inFlight=False,
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

    def _before_start(self, store, errors):
        try:
            store.before_start(self.rt.agent(self.aid))
        except Exception as error:
            errors.append(str(error))


if __name__ == '__main__':
    unittest.main()
