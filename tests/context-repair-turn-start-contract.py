#!/usr/bin/env python3
"""A late context fork resumes the same normal Runtime.start reservation once."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('action_fixture', Path(__file__).with_name('native-action-context-repair-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
repair, eventually = f.f.repair, f.f.f.eventually


class TurnRepair(f.NativeActionRepair):
    def test_late_fork_resumes_one_exact_input(self):
        original_call = self.server.call
        def call(method, params, timeout=60):
            if method == 'turn/start':
                self.server.calls.append((method, params))
                return {'turn': {'id':'new-turn','status':'inProgress'}}
            return original_call(method, params, timeout)
        # The fixture submit closure uses its original call implementation for
        # non-fork methods. Override only the native turn result at submission.
        original_submit = self.server.submit
        def submit(method, params):
            if method == 'turn/start':
                import concurrent.futures
                result = concurrent.futures.Future()
                result.set_result(call(method, params))
                return 'turn-request', method, result
            return original_submit(method, params)
        self.server.submit = submit
        self.server.hold = True
        text = 'One new user request after context repair.'
        self.runtime.send(self.a['id'], text, message_id='new-user-exact')
        with patch.object(repair, 'WAIT_SECONDS', 0.01):
            self.runtime.dispatch()
            eventually(lambda: (self.runtime.agent(self.a['id']).get('contextRepair') or {}).get('phase') == 'unknown')
        before = self.runtime.agent(self.a['id'])
        attempt = before['startAttempt']['id']
        self.assertFalse(any(m == 'turn/start' for m, _ in self.server.calls))
        pending = [future for future in self.server.futures if not future.done()]
        self.assertEqual(len(pending), 1)
        pending[0].set_result({'thread': {'id':'repaired-native'}})
        eventually(lambda: self.runtime.agent(self.a['id']).get('turnId') == 'new-turn')
        current = self.runtime.agent(self.a['id'])
        self.assertEqual(current['startAttempt']['id'], attempt)
        self.assertEqual(current['threadId'], 'repaired-native')
        self.assertEqual(current['startAttempt']['threadId'], 'repaired-native')
        starts = [p for m, p in self.server.calls if m == 'turn/start']
        self.assertEqual(len(starts), 1)
        self.assertEqual(starts[0]['threadId'], 'repaired-native')
        self.assertEqual(starts[0]['clientUserMessageId'], 'new-user-exact')
        self.assertEqual(sum(part.get('text','').count(text) for part in starts[0]['input']), 1)
        self.assertEqual(len(self.forks()), 1)
        with self.runtime.db() as db:
            self.assertEqual(db.execute('SELECT status FROM runtime_events WHERE id=?', ('new-user-exact',)).fetchone()[0], 'delivered')


if __name__ == '__main__':
    suite = unittest.TestSuite(TurnRepair(name) for name in TurnRepair.__dict__ if name.startswith('test_'))
    raise SystemExit(not unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful())
