#!/usr/bin/env python3
"""The scoped transcript update preserves live function and runtime identities."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_message_intent_update as update


class MessageIntentUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = subprocess.check_output(['git', 'show', update.BASE_COMMIT + ':scripts/codex_runtime.py'], cwd=ROOT, text=True)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.module = ModuleType('codex_runtime')
        vars(self.module).update(vars(codex_runtime))
        self.owner = type('Runtime', (codex_runtime.Runtime,), {'__module__': 'codex_runtime'})
        self.module.Runtime = self.owner
        self.owner.transcript = self.compile()
        self.runtime = self.owner.__new__(self.owner)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.db_path = Path(self.tmp.name) / 'transcript.sqlite'
        self.runtime.connection_ids = {'default': 'active-native-connection'}
        self.runtime.servers = {'default': object()}
        with self.runtime.db() as db:
            db.executescript('''
                CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, kind TEXT, text TEXT, status TEXT, created REAL, epoch INTEGER, turn_id TEXT, error TEXT);
                CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);
            ''')
            db.execute('INSERT INTO runtime_agents VALUES (?,?)',
                       ('agent', json.dumps({'id': 'agent', 'epoch': 0, 'status': 'queued', 'autoWake': True})))
            db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)',
                       ('send', 'agent', 'user', 'Send now', 'pending', 1, 0, None, None))
            db.execute('INSERT INTO runtime_event_meta VALUES (?,?)',
                       ('send', json.dumps({'delivery': 'queue', 'requestedDelivery': 'after_tool'})))
        self.module_patch = patch.dict(sys.modules, {'codex_runtime': self.module})
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)

    def compile(self, namespace=None):
        return update.compile_function(self.old, 'Runtime', 'transcript',
                                       vars(self.module) if namespace is None else namespace)

    def state(self):
        with self.runtime.db() as db:
            dump = '\n'.join(db.iterdump())
        return (self.owner.transcript, self.owner.transcript.__code__, dump,
                self.runtime.connection_ids, self.runtime.servers)

    def test_bound_callback_updates_once_without_database_or_connection_changes(self):
        before = self.state()
        callback = self.runtime.transcript
        self.assertNotIn('requestedDelivery', callback('agent')['items'][0])
        receipt = update.apply(self.runtime)
        self.assertEqual(receipt, {'status': 'applied', 'baseCommit': update.BASE_COMMIT, 'methods': ['transcript']})
        self.assertIs(callback.__func__, before[0])
        self.assertIs(self.owner.transcript, before[0])
        self.assertEqual(update.signature(callback.__func__), update.EXPECTED[1])
        self.assertEqual(callback('agent')['items'][0]['requestedDelivery'], 'after_tool')
        after = self.state()
        self.assertEqual(after[2:], before[2:])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
        self.assertEqual(self.state(), after)

    def test_unknown_code_defaults_keywords_namespace_module_and_closure_refuse(self):
        for kind in ('code', 'defaults', 'keywords', 'namespace', 'module', 'closure'):
            function = self.compile(namespace=dict(vars(self.module)) if kind == 'namespace' else None)
            if kind == 'code':
                function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
            elif kind == 'defaults':
                function.__defaults__ = (None, None, 121, None)
            elif kind == 'keywords':
                function.__kwdefaults__ = {'other': True}
            elif kind == 'module':
                function.__module__ = 'unknown'
            elif kind == 'closure':
                def factory():
                    value = 1
                    return lambda self, key, before=None, around=None, limit=120, after=None: value
                function = factory()
            self.owner.transcript = function
            before = self.state()
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())

    def test_overrides_locations_class_identity_and_unreviewed_source_refuse(self):
        before = self.state()
        with patch.object(self.runtime, 'transcript', lambda *args: None), self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)
        with patch.object(self.module, '__file__', '/unknown/runtime.py'), self.assertRaisesRegex(RuntimeError, 'location'):
            update.apply(self.runtime)
        with patch.object(self.owner, '__module__', 'unknown'), self.assertRaisesRegex(RuntimeError, 'class'):
            update.apply(self.runtime)
        self.owner.__name__ = 'Other'
        try:
            with self.assertRaisesRegex(RuntimeError, 'class'):
                update.apply(self.runtime)
        finally:
            self.owner.__name__ = 'Runtime'
        with patch.object(update, 'EXPECTED', (update.EXPECTED[0], '0' * 64)), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        with patch.object(self.module, 'Runtime', type('Other', (), {})), self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_assignment_failure_preserves_method_and_releases_lock(self):
        before = self.state()
        target = self.owner.transcript
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled transcript replacement failure')
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled transcript'):
                update.apply(self.runtime)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())
        self.assertTrue(self.runtime.lock.acquire(blocking=False))
        self.runtime.lock.release()

    def test_closed_busy_unknown_runtime_and_python_refuse(self):
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
