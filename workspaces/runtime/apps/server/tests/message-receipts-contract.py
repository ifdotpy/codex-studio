#!/usr/bin/env python3
"""Exact user receipts remain readable beyond the latest transcript page."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('receipt_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class MessageReceiptsContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead

    def test_delivered_receipt_outside_page_is_read_only_and_survives_epoch_change(self):
        agent = self.lead()['id']
        self.runtime.send(agent, 'sso done', 'sso')
        with self.runtime.db() as db:
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id='sso'")
            a = self.runtime.agent(agent, db)
            a['epoch'] += 1
            self.runtime.put(db, 'agents', a)
            for index in range(130):
                self.runtime.item(db, agent, 'new-' + str(index), 'assistant', 'Newer activity')
            before = self.receipt_state(db)
        self.assertFalse(any(item['text'] == 'sso done' for item in self.runtime.transcript(agent)['items']))
        with self.runtime.lock, ThreadPoolExecutor(1) as pool:
            # Reads cannot wait for the unrelated scheduler lock.
            result = pool.submit(self.runtime.user_delivery_receipts, agent, ['sso', 'unknown']).result(timeout=1)
        self.assertEqual(result, {'agent': agent, 'items': [
            {'id': 'sso', 'status': 'delivered', 'error': None, 'materialized': False}]})
        with self.runtime.db() as db:
            self.assertEqual(self.receipt_state(db), before)

    @staticmethod
    def receipt_state(db):
        return {table: [tuple(row) for row in db.execute('SELECT * FROM ' + table + ' ORDER BY id')]
                for table in ['runtime_agents', 'runtime_events', 'runtime_event_meta', 'runtime_items']}

    def test_other_chats_and_non_user_events_are_not_receipts(self):
        agent = self.lead()['id']
        other = self.lead()['id']
        self.runtime.send(other, 'Other chat', 'foreign')
        with self.runtime.db() as db:
            self.runtime.enqueue(db, self.runtime.agent(agent, db), 'followup', 'Followup', 'system')
        self.assertEqual(self.runtime.user_delivery_receipts(agent, ['foreign', 'system'])['items'], [])

    def test_uncertain_and_failed_are_not_success(self):
        agent = self.lead()['id']
        self.runtime.send(agent, 'Original input', 'original')
        for status in ['pending', 'reserved', 'dispatching', 'uncertain', 'failed', 'cancelled']:
            with self.runtime.db() as db:
                db.execute('UPDATE runtime_events SET status=?,error=? WHERE id=?', (status, 'Native result unknown', 'original'))
            self.assertEqual(self.runtime.user_delivery_receipts(agent, ['original'])['items'],
                             [{'id': 'original', 'status': status, 'error': 'Native result unknown', 'materialized': False}])

    def test_materialization_proves_exact_inputs_beyond_the_loaded_page(self):
        agent = self.lead()['id']
        for event in ('first', 'second', 'legacy'):
            self.runtime.send(agent, 'Same text', event)
        with self.runtime.db() as db:
            self.runtime.item(db, agent, 'first', 'user', 'Same text', inputs=[
                {'id': 'first', 'kind': 'user', 'text': 'Same text'},
                {'id': 'second', 'kind': 'user', 'text': 'Same text'},
            ])
            self.runtime.item(db, agent, 'legacy', 'user', 'Same text')
            db.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?", (agent,))
            for index in range(130):
                self.runtime.item(db, agent, 'new-' + str(index), 'assistant', 'Newer activity')
            before = self.receipt_state(db)
        self.assertFalse(any(item['role'] == 'user' for item in self.runtime.transcript(agent)['items']))
        receipts = self.runtime.user_delivery_receipts(agent, ['first', 'second', 'legacy'])
        self.assertEqual({row['id']: row['materialized'] for row in receipts['items']},
                         {'first': True, 'second': True, 'legacy': True})
        with self.runtime.db() as db:
            self.assertEqual(self.receipt_state(db), before)

    def test_materialization_rejects_missing_foreign_and_wrong_batch_inputs(self):
        agent = self.lead()['id']
        other = self.lead()['id']
        self.runtime.send(agent, 'Original', 'original')
        with self.runtime.db() as db:
            self.runtime.item(db, agent, 'wrong-batch', 'user', 'Same text', inputs=[
                {'id': 'other-input', 'kind': 'user', 'text': 'Original'},
            ])
            self.runtime.item(db, agent, 'system-batch', 'user', 'Original', inputs=[
                {'id': 'original', 'kind': 'followup', 'text': 'Original'},
            ])
            self.runtime.item(db, other, 'original', 'user', 'Original')
            self.runtime.item(db, agent, 'assistant', 'assistant', 'Original')
            db.execute("UPDATE runtime_events SET status='delivered' WHERE id='original'")
        for item_id in (agent + ':missing', other + ':original', agent + ':wrong-batch',
                        agent + ':system-batch', agent + ':assistant'):
            with self.subTest(item=item_id), self.runtime.db() as db:
                db.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.transcriptItemId',?) "
                           "WHERE id='original'", (item_id,))
            result = self.runtime.user_delivery_receipts(agent, ['original'])
            self.assertFalse(result['items'][0]['materialized'])

    def test_bounded_ids_and_deleted_chat(self):
        agent = self.lead()['id']
        for ids in [None, {}, 'sso', ['sso'] * 101, [False], [''], ['x' * 201]]:
            with self.assertRaises(ValueError):
                self.runtime.user_delivery_receipts(agent, ids)
        self.assertEqual(self.runtime.user_delivery_receipts(agent, [])['items'], [])
        with self.runtime.db() as db:
            a = self.runtime.agent(agent, db)
            a['deletedAt'] = 1
            self.runtime.put(db, 'agents', a)
        with self.assertRaisesRegex(ValueError, 'deleted'):
            self.runtime.user_delivery_receipts(agent, ['sso'])


if __name__ == '__main__':
    unittest.main()
