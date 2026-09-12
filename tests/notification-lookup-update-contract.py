#!/usr/bin/env python3
"""Live notification migration preserves callbacks and rolls back its index."""
from contextlib import contextmanager
from pathlib import Path
import sqlite3
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
import codex_notification_lookup_update as update


class NotificationLookupUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = subprocess.check_output(['git', 'show', update.BASE_COMMIT + ':scripts/codex_runtime.py'],
                                          cwd=ROOT, text=True)

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.module = ModuleType('codex_runtime')
        vars(self.module).update(vars(codex_runtime))
        self.owner = type('Runtime', (), {'__module__': 'codex_runtime', 'db': codex_runtime.Runtime.db})
        self.module.Runtime = self.owner
        self.owner.notification = self.compile()
        self.assertEqual(update.signature(self.owner.notification), update.EXPECTED[0])
        self.runtime = self.owner()
        self.runtime.db_path = Path(self.tmp.name) / 'runtime.sqlite'
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'live': object()}
        self.runtime.servers = {'default': object()}
        with self.runtime.db() as db:
            db.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)')
            db.execute('INSERT INTO runtime_agents VALUES (?,?)', ('agent', '{"threadId":"thread","accountKey":"default"}'))
        self.module_patch = patch.dict(sys.modules, {'codex_runtime': self.module})
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)

    def compile(self, current=False, namespace=None):
        source = (ROOT / 'scripts/codex_runtime.py').read_text() if current else self.old
        return update.compile_function(source, 'Runtime', 'notification',
                                       vars(self.module) if namespace is None else namespace)

    def state(self):
        with self.runtime.db() as db:
            index = db.execute('SELECT type,tbl_name,sql FROM sqlite_master WHERE name=?',
                               (update.INDEX_NAME,)).fetchone()
            records = [tuple(row) for row in db.execute('SELECT * FROM runtime_agents')]
        return (self.owner.notification, self.owner.notification.__code__,
                tuple(index) if index else None, records,
                self.runtime.connections, self.runtime.servers)

    def test_baseline_migrates_index_and_preserves_callback_and_connections(self):
        callback = self.runtime.notification
        before = self.state()
        constructor = self.owner.__init__
        receipt = update.apply(self.runtime)
        self.assertEqual(receipt, {'status': 'applied', 'baseCommit': update.BASE_COMMIT,
                                  'methods': ['notification'], 'indexCreated': True})
        self.assertIs(callback.__func__, self.owner.notification)
        self.assertIs(self.owner.notification, before[0])
        self.assertIs(self.owner.__init__, constructor)
        self.assertEqual(update.signature(callback.__func__), update.EXPECTED[1])
        after = self.state()
        self.assertEqual(after[3:], before[3:])
        self.assertEqual(after[2][0:2], ('index', 'runtime_agents'))
        self.assertEqual(' '.join(after[2][2].split()), ' '.join(update.INDEX_SQL.split()))
        self.assertEqual(update.apply(self.runtime), {'status': 'already_applied',
            'baseCommit': update.BASE_COMMIT, 'methods': ['notification'], 'indexCreated': False})
        self.assertEqual(self.state(), after)

    def test_existing_reviewed_startup_index_and_missing_index_on_new_code(self):
        with self.runtime.db() as db:
            db.execute(update.INDEX_SQL.replace('CREATE INDEX ', 'CREATE INDEX IF NOT EXISTS ', 1))
        result = update.apply(self.runtime)
        self.assertFalse(result['indexCreated'])
        self.assertEqual(result['status'], 'applied')
        with self.runtime.db() as db:
            db.execute('DROP INDEX ' + update.INDEX_NAME)
        result = update.apply(self.runtime)
        self.assertTrue(result['indexCreated'])
        self.assertEqual(update.signature(self.owner.notification), update.EXPECTED[1])

    def test_unknown_same_name_index_or_schema_object_refuses(self):
        definitions = [
            'CREATE INDEX ' + update.INDEX_NAME + ' ON runtime_agents(id)',
            update.INDEX_SQL.replace("THEN 'default'", "THEN 'other'"),
            'CREATE TABLE ' + update.INDEX_NAME + '(id TEXT)',
        ]
        for ddl in definitions:
            with self.runtime.db() as db:
                db.execute(ddl)
            before = self.state()
            with self.subTest(ddl=ddl), self.assertRaisesRegex(RuntimeError, 'Unknown existing'):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())
            with self.runtime.db() as db:
                db.execute(('DROP TABLE ' if 'CREATE TABLE ' in ddl else 'DROP INDEX ') + update.INDEX_NAME)

    def test_unknown_code_defaults_keywords_namespace_and_override_refuse(self):
        for kind in ('code', 'defaults', 'keywords', 'namespace'):
            function = self.compile(namespace=dict(vars(self.module)) if kind == 'namespace' else None)
            if kind == 'code':
                function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
            elif kind == 'defaults':
                function.__defaults__ = ('other', None)
            elif kind == 'keywords':
                function.__kwdefaults__ = {'unknown': True}
            self.owner.notification = function
            before = self.state()
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())
        self.owner.notification = self.compile()
        self.runtime.notification = lambda *args: None
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_source_location_and_unreviewed_replacement_refuse(self):
        before = self.state()
        with patch.object(self.module, '__file__', '/unknown/codex_runtime.py'), \
                self.assertRaisesRegex(RuntimeError, 'location'):
            update.apply(self.runtime)
        with patch.object(update, 'EXPECTED', (update.EXPECTED[0], '0' * 64)), \
                self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_code_assignment_failure_rolls_back_new_index(self):
        before = self.state()
        target = self.owner.notification
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled notification replacement failure')
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled notification'):
                update.apply(self.runtime)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())

    def test_transaction_failure_restores_changed_callback_and_index(self):
        before = self.state()
        original_db = self.runtime.db
        @contextmanager
        def failed_commit():
            with original_db() as db:
                yield db
                self.assertEqual(update.signature(self.owner.notification), update.EXPECTED[1])
                db.set_authorizer(lambda action, value, *_:
                    sqlite3.SQLITE_DENY if action == sqlite3.SQLITE_TRANSACTION and value == 'COMMIT'
                    else sqlite3.SQLITE_OK)
        with patch.object(self.runtime, 'db', failed_commit), \
                self.assertRaisesRegex(sqlite3.DatabaseError, 'not authorized'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

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
