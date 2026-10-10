#!/usr/bin/env python3
"""A late context fork resumes the same normal Runtime.start reservation once."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
from types import SimpleNamespace
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('action_fixture', Path(__file__).with_name('native-action-context-repair-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
repair, eventually = f.f.repair, f.f.f.eventually


class TurnRepair(f.NativeActionRepair):
    def exact_scope(self, **attempt_changes):
        attempt = {'id':'same-attempt', 'epoch':self.a['epoch'], 'accountKey':'default',
                   'threadId':self.tid, 'events':['same-input'], 'submitted':False,
                   'activeAtReservation':False, 'created':1, 'settingsFixed':True,
                   'connectionId':self.runtime.connection_ids['default'],
                   'action':None, 'actionRequestId':None, 'actionIdentity':None}
        attempt.update(attempt_changes)
        actor = self.agent_update(self.a, startAttempt=attempt, contextRepair={'id':'same-repair'})
        return actor, {'id':'same-repair', 'agent':actor['id'], 'source':repair._identity(actor),
                       'settings':self.runtime.preparation_settings(actor),
                       'startAttemptSnapshot':copy.deepcopy(attempt), 'nativeIdentity':None,
                       'connectionId':self.runtime.connection_ids['default']}

    def test_added_prepare_error_preserves_exact_attempt_and_snapshot(self):
        actor, operation = self.exact_scope()
        original = copy.deepcopy(operation)
        waiting = 'Thread preparation acknowledgement pending; no turn input has been submitted'
        actor = self.agent_update(actor, startAttempt={**actor['startAttempt'], 'prepareError':waiting})
        with self.runtime.lock, self.runtime.db() as db:
            current = repair._current(self.runtime, db, operation)
        self.assertEqual(current['startAttempt'], actor['startAttempt'])
        self.assertEqual(operation, original)

    def test_changed_existing_prepare_error_and_nonstring_addition_reject(self):
        actor, operation = self.exact_scope(prepareError='Original preparation wait')
        with self.runtime.lock, self.runtime.db() as db:
            self.assertEqual(repair._current(self.runtime, db, operation)['startAttempt'], actor['startAttempt'])
        changed = {**actor['startAttempt'], 'prepareError':'A different wait'}
        self.agent_update(actor, startAttempt=changed)
        with self.runtime.lock, self.runtime.db() as db:
            with self.assertRaisesRegex(ValueError, 'agent changed during context repair'):
                repair._current(self.runtime, db, operation)
        for value in (None, False, 3, {}, []):
            with self.subTest(prepareError=value):
                actor, operation = self.exact_scope()
                malformed = {**actor, 'startAttempt':{**actor['startAttempt'], 'prepareError':value}}
                with patch.object(self.runtime, 'agent', return_value=malformed), self.runtime.lock, self.runtime.db() as db:
                    with self.assertRaisesRegex(ValueError, 'agent changed during context repair'):
                        repair._current(self.runtime, db, operation)

    def test_added_prepare_error_never_hides_another_attempt_change(self):
        actor, operation = self.exact_scope()
        original = copy.deepcopy(actor['startAttempt'])
        changes = {'id':'another-attempt', 'epoch':actor['epoch'] + 1, 'accountKey':'other-account',
                   'threadId':'other-thread', 'events':['another-input'], 'submitted':True,
                   'activeAtReservation':True, 'created':2, 'settingsFixed':False,
                   'connectionId':'another-connection', 'action':'review',
                   'actionRequestId':'another-request', 'actionIdentity':{'id':'other'},
                   'executionOutcome':'unknown'}
        for key, value in changes.items():
            with self.subTest(field=key):
                self.agent_update(actor, startAttempt={**original, 'prepareError':'Waiting', key:value})
                with self.runtime.lock, self.runtime.db() as db:
                    with self.assertRaisesRegex(ValueError, 'agent changed during context repair'):
                        repair._current(self.runtime, db, operation)

    def test_added_prepare_error_preserves_owner_stop_connection_and_native_guards(self):
        actor, operation = self.exact_scope()
        actor = self.agent_update(actor, startAttempt={**actor['startAttempt'], 'prepareError':'Waiting'})
        for change in ({'epoch':actor['epoch'] + 1, 'autoWake':False, 'status':'paused'},
                       {'accountKey':'other-account'}, {'threadId':'other-thread'}, {'deletedAt':1}):
            with self.subTest(change=change):
                changed = {**actor, **change}
                with self.runtime.lock, self.runtime.db() as db:
                    self.runtime.put(db, 'agents', changed)
                    with self.assertRaisesRegex(ValueError, 'agent changed during context repair'):
                        repair._current(self.runtime, db, operation)
                    self.runtime.put(db, 'agents', actor)
        original_connection = self.runtime.connection_ids['default']
        try:
            self.runtime.connection_ids['default'] = 'another-connection'
            real_connection = type(self.runtime).connection_current.__get__(self.runtime, type(self.runtime))
            with patch.object(self.runtime, 'connection_current', real_connection), self.runtime.lock, self.runtime.db() as db:
                with self.assertRaisesRegex(ValueError, 'agent changed during context repair'):
                    repair._current(self.runtime, db, operation)
        finally:
            self.runtime.connection_ids['default'] = original_connection
        with patch.object(self.server, 'supervisor_mode', True, create=True), patch.object(
                self.server, 'proc', SimpleNamespace(root=self.root, handle='same-handle', generation=1), create=True):
            operation['nativeIdentity'] = repair._source_native_identity(self.server)
            self.server.proc.generation = 2
            with self.runtime.lock, self.runtime.db() as db:
                with self.assertRaisesRegex(ValueError, 'agent changed during context repair'):
                    repair._current(self.runtime, db, operation)

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
        eventually(lambda: isinstance((self.runtime.agent(self.a['id']).get('startAttempt') or {}).get('prepareError'), str))
        before = self.runtime.agent(self.a['id'])
        self.assertNotIn('prepareError', before['contextRepair']['startAttemptSnapshot'])
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
