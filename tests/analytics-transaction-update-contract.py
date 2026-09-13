#!/usr/bin/env python3
"""The analytics live update preserves callbacks and does not replay samples."""
from pathlib import Path
import sqlite3
import subprocess
import sys
import threading
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_analytics
import codex_runtime
import codex_analytics_transaction_update as update


class AnalyticsTransactionUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = subprocess.check_output(['git', 'show', update.BASE_COMMIT + ':scripts/codex_analytics.py'], cwd=ROOT, text=True)

    def setUp(self):
        self.module = ModuleType('codex_analytics')
        vars(self.module).update(vars(codex_analytics))
        self.runtime_module = ModuleType('codex_runtime')
        vars(self.runtime_module).update(vars(codex_runtime))
        self.owner = type('AnalyticsMixin', (), {'__module__': 'codex_analytics'})
        self.runtime_owner = type('Runtime', (self.owner,), {'__module__': 'codex_runtime'})
        self.module.AnalyticsMixin = self.owner
        self.runtime_module.AnalyticsMixin = self.owner
        self.runtime_module.Runtime = self.runtime_owner
        self.owner.analytics_safe = self.compile()
        self.runtime = self.runtime_owner()
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'default': object()}
        self.runtime.requests = {'pending': {'stage': 'running'}}
        self.runtime.agents = {'active': {'status': 'running', 'turnId': 'retained'}}
        self.db = sqlite3.connect(':memory:')
        self.addCleanup(self.db.close)
        self.db.executescript('CREATE TABLE samples (value INTEGER); CREATE TABLE analytics_meta (key TEXT PRIMARY KEY, value TEXT);')
        self.trace = []
        self.db.set_trace_callback(self.trace.append)
        self.module_patch = patch.dict(sys.modules, {'codex_runtime': self.runtime_module, 'codex_analytics': self.module})
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)

    def compile(self, namespace=None):
        return update.compile_function(self.old, 'AnalyticsMixin', 'analytics_safe',
            vars(self.module) if namespace is None else namespace)

    def state(self):
        return (self.owner.analytics_safe, self.owner.analytics_safe.__code__,
                self.runtime.connections, repr(self.runtime.requests), repr(self.runtime.agents), tuple(self.trace))

    def test_captured_callback_batches_samples_without_replay_and_second_apply_is_noop(self):
        callback = self.runtime.analytics_safe
        before = self.state()
        result = update.apply(self.runtime)
        self.assertEqual(result['status'], 'applied')
        self.assertIs(callback.__func__, self.owner.analytics_safe)
        self.assertEqual(before[2:], self.state()[2:])
        self.assertEqual(update.signature(callback.__func__), update.EXPECTED[1])
        with self.db:
            for value in range(128):
                callback(self.db, lambda db, value: db.execute('INSERT INTO samples VALUES (?)', (value,)), value)
        self.assertEqual(self.trace.count('BEGIN'), 1)
        self.assertEqual(self.trace.count('COMMIT'), 1)
        self.assertEqual(self.db.execute('SELECT count(*) FROM samples').fetchone()[0], 128)
        after = self.state()
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
        self.assertEqual(after, self.state())

    def test_unknown_code_defaults_globals_module_and_closure_refuse_before_mutation(self):
        for kind in ('code', 'defaults', 'globals', 'module', 'closure'):
            function = self.compile(namespace=dict(vars(self.module)) if kind == 'globals' else None)
            if kind == 'code':
                function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
            elif kind == 'defaults':
                function.__defaults__ = (None,)
            elif kind == 'module':
                function.__module__ = 'unknown'
            elif kind == 'closure':
                value = 1
                function = lambda *args: value
            self.owner.analytics_safe = function
            before = self.state()
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())

    def test_overrides_locations_class_identity_and_replacement_source_refuse(self):
        before = self.state()
        with patch.object(self.runtime, 'analytics_safe', lambda *args: None), self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)
        with patch.object(self.runtime_owner, 'analytics_safe', lambda *args: None, create=True), self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)
        with patch.object(self.module, '__file__', '/unknown/codex_analytics.py'), self.assertRaisesRegex(RuntimeError, 'location'):
            update.apply(self.runtime)
        with patch.object(self.owner, '__module__', 'unknown'), self.assertRaisesRegex(RuntimeError, 'class'):
            update.apply(self.runtime)
        with patch.object(update, 'EXPECTED', (update.EXPECTED[0], '0' * 64)), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_failed_code_assignment_preserves_callback_and_releases_lock(self):
        before = self.state()
        target = self.owner.analytics_safe
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled analytics assignment failure')
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled analytics'):
                update.apply(self.runtime)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())
        self.assertTrue(self.runtime.lock.acquire(blocking=False))
        self.runtime.lock.release()

    def test_closed_busy_and_unknown_runtime_refuse(self):
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            update.apply(object())
        with patch.object(sys, 'version_info', (3, 13)), self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            update.apply(self.runtime)
        self.runtime.closed = True
        with self.assertRaisesRegex(RuntimeError, 'closed'):
            update.apply(self.runtime)
        self.runtime.closed = False
        runtime = self.runtime
        class Lock:
            close_on_acquire = False
            released = False
            def acquire(self, *, timeout):
                self.timeout = timeout
                runtime.closed = self.close_on_acquire
                return self.close_on_acquire
            def release(self):
                self.released = True
        self.runtime.lock = Lock()
        with self.assertRaisesRegex(RuntimeError, 'busy'):
            update.apply(self.runtime)
        self.assertEqual(self.runtime.lock.timeout, 10)
        self.assertFalse(self.runtime.lock.released)
        self.runtime.lock.close_on_acquire = True
        with self.assertRaisesRegex(RuntimeError, 'closed'):
            update.apply(self.runtime)
        self.assertTrue(self.runtime.lock.released)
        self.assertEqual(before, self.state())


if __name__ == '__main__':
    unittest.main()
