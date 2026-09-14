#!/usr/bin/env python3
"""Chat snapshots exclude internal proof while preserving visible state."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('projection_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class Projection(f.RuntimeContract):
    def test_large_internal_proof_does_not_expand_chat_or_mutate_durable_agent(self):
        a = self.lead()
        self.complete(a)
        with self.runtime.lock:
            internal = ('contextRepair', 'contextRepairHistory', 'lastContextRepairCheck',
                        'lastContextRepairWait', 'nativeNameSynced')
            with self.runtime.lock, self.runtime.db() as db:
                a = self.runtime.agent(a['id'], db)
                a.update(status='queued', error='Wait for the exact saved receipt.',
                         contextRepairWait={'phase':'waiting','reason':'native receipt'},
                         startAttempt={'id':'exact-start','prepareError':'Wait for the native response.'})
                self.runtime.put(db, 'agents', a)
            baseline = self.runtime.snapshot(include_work=False)
            with self.runtime.lock, self.runtime.db() as db:
                a = self.runtime.agent(a['id'], db)
                for field in internal:
                    a[field] = {'proof':'record-' * 50000, 'source':field}
                self.runtime.put(db, 'agents', a)
            before = self.runtime.agent(a['id'])
            chat = self.runtime.snapshot(include_work=False)
            full = self.runtime.snapshot()
            self.assertEqual(chat, baseline)
            self.assertLess(len(json.dumps(chat)), 20000)
            self.assertGreater(len(json.dumps(full)), 1500000)
            self.assertEqual(self.runtime.agent(a['id']), before)
            projected = next(item for item in chat['agents'] if item['id'] == a['id'])
            diagnostic = next(item for item in full['agents'] if item['id'] == a['id'])
            for field in internal:
                self.assertNotIn(field, projected)
                self.assertEqual(diagnostic[field], before[field])
            self.assertEqual(projected['contextRepairWait'], before['contextRepairWait'])
            self.assertEqual(projected['startAttempt'], before['startAttempt'])
            self.assertEqual(projected['error'], before['error'])


if __name__ == '__main__':
    suite = unittest.TestSuite(Projection(name) for name in Projection.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
