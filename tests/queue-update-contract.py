#!/usr/bin/env python3
"""The scoped queue update preserves live identities and rejects unknown code."""
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
import codex_work
import codex_queue_update as update


class QueueUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = subprocess.check_output(['git', 'show', update.BASE_COMMIT + ':scripts/codex_work.py'], cwd=ROOT, text=True)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.work = ModuleType('codex_work')
        vars(self.work).update(vars(codex_work))
        self.owner = type('WorkMixin', (codex_work.WorkMixin,), {'__module__': 'codex_work'})
        self.work.WorkMixin = self.owner
        self.owner.queue_action = self.compile()
        self.module = ModuleType('codex_runtime')
        vars(self.module).update(vars(codex_runtime))
        self.runtime_class = type('Runtime', (self.owner, codex_runtime.Runtime), {'__module__': 'codex_runtime'})
        self.module.Runtime = self.runtime_class
        self.module.WorkMixin = self.owner
        self.runtime = self.runtime_class.__new__(self.runtime_class)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.db_path = Path(self.tmp.name) / 'queue.sqlite'
        self.runtime.ui_condition = threading.Condition()
        self.runtime.ui_revisions = {}
        self.runtime.changed = threading.Event()
        self.runtime.connections = {'live': object()}
        self.runtime.servers = {'default': object()}
        with self.runtime.db() as db:
            db.executescript('''
                CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_events(id TEXT PRIMARY KEY, agent TEXT, text TEXT, kind TEXT, status TEXT, created REAL, epoch INTEGER);
                CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
            ''')
            db.execute('INSERT INTO runtime_agents VALUES (?,?)', ('agent', json.dumps({'id': 'agent', 'epoch': 0})))
            for index in range(3):
                db.execute('INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?)',
                           (f'message-{index}', 'agent', f'Text {index}', 'user', 'pending', index + 1, 0))
        self.module_patch = patch.dict(sys.modules, {'codex_runtime': self.module, 'codex_work': self.work})
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)

    def compile(self, current=False, namespace=None):
        source = (ROOT / 'scripts/codex_work.py').read_text() if current else self.old
        return update.compile_function(source, 'WorkMixin', 'queue_action',
                                       vars(self.work) if namespace is None else namespace)

    def state(self):
        with self.runtime.db() as db:
            rows = {table: [tuple(row) for row in db.execute('SELECT * FROM ' + table)]
                    for table in ('runtime_agents', 'runtime_events', 'runtime_event_meta', 'runtime_operation_receipts')}
        return (self.owner.queue_action, self.owner.queue_action.__code__, rows,
                self.runtime.connections, self.runtime.servers)

    def test_update_preserves_callback_connections_and_database_then_runs_new_queue(self):
        before = self.state()
        callback = self.runtime.queue_action
        receipt = update.apply(self.runtime)
        self.assertEqual(receipt, {'status': 'applied', 'baseCommit': update.BASE_COMMIT, 'methods': ['queue_action']})
        self.assertIs(callback.__func__, before[0])
        self.assertIs(self.owner.queue_action, before[0])
        self.assertEqual(update.signature(callback.__func__), update.EXPECTED[1])
        after = self.state()
        self.assertEqual(after[2:], before[2:])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
        self.assertEqual(self.state(), after)
        view = callback('agent')
        self.assertTrue(view['capabilities']['receipts'])
        request = {'action': 'reorder', 'request_id': 'reorder-once', 'expected_revision': view['revision'],
                   'ordered_ids': ['message-2', 'message-0', 'message-1']}
        result = callback('agent', request)
        self.assertEqual([item['id'] for item in callback('agent')['items']], request['ordered_ids'])
        self.assertEqual(callback('agent', request), result)
        edit = {'action': 'edit', 'request_id': 'edit-once', 'expected_revision': result['revision'],
                'message_id': 'message-2', 'text': 'Edited'}
        callback('agent', edit)
        self.assertEqual(callback('agent')['items'][0]['text'], 'Edited')
        self.assertEqual(self.runtime.connections, before[3])
        self.assertEqual(self.runtime.servers, before[4])

    def test_unknown_function_defaults_keywords_namespace_and_closure_refuse(self):
        for kind in ('code', 'defaults', 'keywords', 'namespace', 'closure'):
            function = self.compile(namespace=dict(vars(self.work)) if kind == 'namespace' else None)
            if kind == 'code':
                function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
            elif kind == 'defaults':
                function.__defaults__ = ('other',)
            elif kind == 'keywords':
                function.__kwdefaults__ = {'other': True}
            elif kind == 'closure':
                def factory():
                    value = 1
                    return lambda self, agent_id, data=None: value
                function = factory()
            self.owner.queue_action = function
            before = self.state()
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())

    def test_instance_and_class_overrides_refuse(self):
        before = self.state()
        with patch.object(self.runtime, 'queue_action', lambda *args: None), self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)
        with patch.object(self.runtime_class, 'queue_action', self.owner.queue_action), self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_locations_class_identity_and_unreviewed_source_refuse(self):
        before = self.state()
        for module in (self.module, self.work):
            with patch.object(module, '__file__', '/unknown/module.py'), self.assertRaisesRegex(RuntimeError, 'location'):
                update.apply(self.runtime)
        with patch.object(self.module, 'WorkMixin', type('Other', (), {})), self.assertRaisesRegex(RuntimeError, 'class'):
            update.apply(self.runtime)
        with patch.object(self.owner, '__module__', 'unknown'), self.assertRaisesRegex(RuntimeError, 'class'):
            update.apply(self.runtime)
        with patch.object(update, 'EXPECTED', (update.EXPECTED[0], '0' * 64)), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_assignment_failure_preserves_old_function_and_state(self):
        before = self.state()
        target = self.owner.queue_action
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled queue replacement failure')
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled queue'):
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
