#!/usr/bin/env python3
"""Transcript receipts preserve Send and queue intent without changing delivery."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('intent_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class MessageIntentContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead

    def receipts(self, agent, **options):
        result = {}
        for item in self.runtime.transcript(agent, **options)['items']:
            for entry in item.get('inputs') or [item]:
                if entry.get('kind') == 'user' or entry.get('role') == 'user':
                    result[entry.get('clientMessageId') or entry['id']] = entry
        return result

    def database(self):
        with self.runtime.db() as db:
            return '\n'.join(db.iterdump())

    def test_idle_send_stays_send_in_every_delivery_phase_without_database_writes(self):
        agent = self.lead()['id']
        self.runtime.send(agent, 'Send now', 'send', delivery='after_tool')
        self.runtime.send(agent, 'Queue later', 'queue', delivery='queue')
        for status in ['pending', 'reserved', 'dispatching', 'uncertain', 'failed']:
            with self.runtime.db() as db:
                db.execute('UPDATE runtime_events SET status=? WHERE agent=?', (status, agent))
            before = self.database()
            receipts = self.receipts(agent)
            self.assertEqual(receipts['send']['requestedDelivery'], 'after_tool', status)
            self.assertEqual(receipts['queue']['requestedDelivery'], 'queue', status)
            self.assertEqual(receipts['send']['deliveryStatus'], status)
            self.assertEqual(receipts['queue']['deliveryStatus'], status)
            self.assertEqual(receipts['send']['pending'], status == 'pending')
            self.assertEqual(self.database(), before)

    def test_real_dispatch_preserves_intent_for_each_input_in_one_native_batch(self):
        agent = self.lead()['id']
        self.runtime.send(agent, 'Send first', 'send', delivery='after_tool')
        self.runtime.send(agent, 'Queued second', 'queue', delivery='queue')
        self.runtime.dispatch()
        f.eventually(lambda: self.runtime.agent(agent).get('turnId'))
        active = self.runtime.agent(agent)
        before = self.database()
        receipts = self.receipts(agent)
        self.assertEqual(set(receipts), {'send', 'queue'})
        self.assertEqual(receipts['send']['requestedDelivery'], 'after_tool')
        self.assertEqual(receipts['queue']['requestedDelivery'], 'queue')
        self.assertTrue(all(entry['materialized'] for entry in receipts.values()))
        self.assertEqual(self.database(), before)
        self.runtime.server.complete(active['threadId'], active['turnId'])
        f.eventually(lambda: not self.runtime.agent(agent).get('inFlight'))
        self.assertEqual(self.receipts(agent)['send']['requestedDelivery'], 'after_tool')

    def test_legacy_metadata_and_missing_event_have_conservative_fallback(self):
        agent = self.lead()['id']
        for event in ['legacy', 'absent', 'explicit']:
            self.runtime.send(agent, event, event)
        with self.runtime.db() as db:
            db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?', (json.dumps({'delivery': 'steer'}), 'legacy'))
            db.execute('DELETE FROM runtime_event_meta WHERE id=?', ('absent',))
            db.execute('UPDATE runtime_event_meta SET record=? WHERE id=?',
                       (json.dumps({'delivery': 'queue', 'requestedDelivery': 'after_tool'}), 'explicit'))
            self.runtime.item(db, agent, 'imported', 'user', 'Imported without a local event', requestedDelivery='after_tool')
        before = self.database()
        receipts = self.receipts(agent)
        self.assertEqual(receipts['legacy']['requestedDelivery'], 'steer')
        self.assertEqual(receipts['absent']['requestedDelivery'], 'queue')
        self.assertEqual(receipts['explicit']['requestedDelivery'], 'after_tool')
        self.assertEqual(receipts[agent + ':imported']['requestedDelivery'], 'queue')
        self.assertEqual(self.database(), before)

    def test_materialized_standalone_receipts_keep_intent_and_pagination(self):
        agent = self.lead()['id']
        with self.runtime.db() as db:
            for index in range(4):
                event = 'item-' + str(index)
                db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                           (event, agent, 'user', event, 'delivered', index + 1, 0, 'turn', None))
                db.execute('INSERT INTO runtime_event_meta VALUES (?,?)',
                           (event, json.dumps({'requestedDelivery': 'after_tool', 'delivery': 'queue'})))
                self.runtime.item(db, agent, event, 'user', event)
                db.execute('UPDATE runtime_items SET created=? WHERE id=?', (index + 1, agent + ':' + event))
        self.runtime.send(agent, 'Still pending', 'outstanding', delivery='after_tool')
        before = self.database()
        page = self.runtime.transcript(agent, limit=2)
        self.assertEqual(page['nextCursor'], agent + ':item-2')
        older = self.receipts(agent, before=page['nextCursor'], limit=2)
        self.assertEqual(set(older), {'item-0', 'item-1'})
        self.assertTrue(all(entry['requestedDelivery'] == 'after_tool' for entry in older.values()))
        self.assertNotIn('outstanding', older)
        self.assertEqual(self.database(), before)


if __name__ == '__main__':
    unittest.main()
