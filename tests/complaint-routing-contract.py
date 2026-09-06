#!/usr/bin/env python3
"""Complaint responsibility, notification, and response identity. No model calls."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('defaults', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_runtime import ComplaintConflict


class ComplaintRouting(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.lead = self.runtime.create({'name':'Lead', 'prompt':'', 'cwd':str(self.root)}, draft=True)

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def submit(self, actor=None, key='complaint'):
        return self.runtime.complaint(actor or self.lead['id'], {'action':'submit', 'text':'Monitor selects the wrong Python interpreter.'}, key)

    def events(self, kind):
        with self.runtime.db() as db:
            return [dict(r) for r in db.execute('SELECT * FROM runtime_events WHERE kind=?', (kind,))]

    def respond(self, complaint, key='user:response', **values):
        return self.runtime.complaint_response_from_user({'action':'respond', 'complaint_id':complaint['id'],
            'version':complaint['version'], 'text':'The shell fix is deployed and verified.', 'status':'resolved', **values}, key)

    def test_lead_files_for_user_without_self_wake_or_self_response(self):
        c = self.submit()
        self.assertEqual(c['recipient'], 'user')
        self.assertEqual(self.events('complaint'), [])
        self.assertTrue(self.runtime.snapshot()['complaints'][0]['needsResponse'])
        self.runtime.complaint(self.lead['id'], {'action':'read'}, 'read')
        self.assertIsNone(self.runtime.complaint_detail(c['id'])['readAt'])
        with self.assertRaisesRegex(ValueError, 'assigned to the user'):
            self.runtime.complaint(self.lead['id'], {'action':'respond', 'complaint_id':c['id'],
                'text':'I reported it.', 'status':'resolved'}, 'self-response')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.enforce_complaints(db, self.lead, 'finished')
        self.assertEqual(self.events('complaint'), [])

    def test_worker_files_for_lead_and_user_cannot_take_over(self):
        worker = self.runtime.create({'name':'Worker', 'prompt':'Check', 'role':'reviewer'}, parent=self.lead['id'])
        c = self.submit(worker['id'])
        self.assertEqual(c['recipient'], 'lead')
        self.assertEqual(self.events('complaint')[0]['agent'], self.lead['id'])
        with self.assertRaisesRegex(ValueError, 'orchestrator'):
            self.respond(c)
        response = self.runtime.complaint(self.lead['id'], {'action':'respond', 'complaint_id':c['id'],
            'text':'Use the checked Python path while I report the defect.', 'status':'in_progress'}, 'lead-response')
        self.assertEqual(response['responses'][0]['author'], self.lead['id'])
        self.assertFalse(self.runtime.snapshot()['complaints'][0]['needsResponse'])

    def test_user_response_wakes_finished_author_and_is_idempotent(self):
        c = self.submit()
        with self.runtime.lock, self.runtime.db() as db:
            self.lead['status'] = 'completed'
            self.runtime.put(db, 'agents', self.lead)
        response = self.respond(c)
        self.assertEqual(response['version'], 2)
        self.assertEqual(response['responses'][0]['author'], 'user')
        self.assertEqual(self.runtime.agent(self.lead['id'])['status'], 'queued')
        event = self.events('complaint_response')[0]
        self.assertEqual(json.loads(event['text'])['responder'], 'user')
        self.assertEqual(self.respond(c), response)
        self.assertEqual(len(self.events('complaint_response')), 1)
        self.assertFalse(self.runtime.snapshot()['complaints'][0]['needsResponse'])
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.respond(c, text='A different result')
        with self.assertRaises(ComplaintConflict):
            self.respond(c, key='user:stale')
        other = self.submit(key='another-complaint')
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.respond(other)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.assertEqual(self.respond(c), response)

    def test_user_response_preserves_stop_boundary(self):
        c = self.submit()
        self.runtime.stop(self.lead['id'])
        self.respond(c)
        self.assertFalse(self.runtime.agent(self.lead['id'])['autoWake'])
        self.assertEqual(self.events('complaint_response')[0]['status'], 'cancelled')
        self.assertEqual(self.runtime.complaint_detail(c['id'])['status'], 'resolved')

    def test_user_response_requires_reason_and_current_version(self):
        c = self.submit()
        for patch in [{'text':''}, {'status':'anything'}, {'version':True}, {'version':None}]:
            with self.assertRaises(ValueError):
                self.respond(c, **patch)
        self.assertEqual(self.runtime.complaint_detail(c['id'])['responses'], [])

    def test_migration_cancels_self_wake_but_keeps_worker_complaints(self):
        c = self.submit()
        c.pop('recipient')
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'complaints', c)
            self.runtime.enqueue(db, self.lead, 'complaint', 'Old self complaint reminder', 'self-wake')
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.assertEqual(self.events('complaint')[0]['status'], 'cancelled')
        worker = self.runtime.create({'name':'Worker', 'prompt':'Check', 'role':'reviewer'}, parent=self.lead['id'])
        self.submit(worker['id'], 'worker-complaint')
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.assertEqual(sum(r['status']=='pending' for r in self.events('complaint')), 1)

    def test_legacy_self_response_does_not_count_as_user_action(self):
        c = self.submit()
        c.pop('recipient')
        c.pop('version')
        c.update(status='resolved', readAt=100, responses=[{'id':'old', 'author':self.lead['id'],
            'text':'I reported this to the owner.', 'status':'resolved', 'at':100}])
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'complaints', c)
        self.runtime.close()
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        migrated = self.runtime.complaint_detail(c['id'])
        self.assertEqual(migrated['recipient'], 'user')
        self.assertEqual(migrated['status'], 'open')
        self.assertEqual(migrated['legacyStatus'], 'resolved')
        self.assertIsNone(migrated['readAt'])
        self.assertEqual(migrated['responses'], c['responses'])
        self.assertTrue(self.runtime.snapshot()['complaints'][0]['needsResponse'])
        self.respond(migrated)
        self.assertFalse(self.runtime.snapshot()['complaints'][0]['needsResponse'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
