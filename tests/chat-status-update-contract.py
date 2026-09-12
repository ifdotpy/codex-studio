#!/usr/bin/env python3
"""The chat status update preserves callbacks, connections, and rollback."""
from pathlib import Path
import subprocess
import sys
import threading
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_work
import codex_chat_status_update as update


class ChatStatusUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(
            ['git', 'show', update.BASE_COMMIT + ':scripts/' + name + '.py'],
            cwd=ROOT, text=True) for name in ('codex_runtime', 'codex_work')}

    def setUp(self):
        self.modules = {}
        for name in self.old:
            module = ModuleType(name)
            vars(module).update(vars(sys.modules[name]))
            self.modules[name] = module
        self.work = type('WorkMixin', (), {'__module__': 'codex_work'})
        self.owner = type('Runtime', (self.work,), {'__module__': 'codex_runtime'})
        self.modules['codex_work'].WorkMixin = self.work
        self.modules['codex_runtime'].Runtime = self.owner
        self.runtime = self.owner()
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'original': object()}
        self.runtime.database = object()
        for target, allowed in update.EXPECTED.items():
            module, owner, name = target.split('.')
            function = self.compile(target)
            self.assertEqual(update.signature(function), allowed[0])
            setattr(getattr(self.modules[module], owner), name, function)
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        sys.modules.pop('codex_chat_read_state', None)

    def tearDown(self):
        self.module_patch.stop()

    def compile(self, target, current=False, namespace=None):
        module, owner, name = target.split('.')
        source = (ROOT / ('scripts/' + module + '.py')).read_text() if current else self.old[module]
        return update.compile_function(source, owner, name,
            vars(self.modules[module]) if namespace is None else namespace)

    def state(self):
        functions = []
        for target in update.EXPECTED:
            module, owner, name = target.split('.')
            function = vars(getattr(self.modules[module], owner))[name]
            functions.append((function, function.__code__))
        return (functions, sys.modules.get('codex_chat_read_state'),
                self.runtime.connections, self.runtime.database)

    def test_baseline_preserves_captured_callbacks_and_is_idempotent(self):
        before = self.state()
        callbacks = [self.runtime.snapshot, self.runtime.chat_organization]
        result = update.apply(self.runtime)
        self.assertEqual(result, {'status': 'applied', 'baseCommit': update.BASE_COMMIT,
                                  'methods': list(update.EXPECTED)})
        for callback, (function, _), target in zip(callbacks, before[0], update.EXPECTED):
            self.assertIs(callback.__func__, function)
            self.assertEqual(update.signature(function), update.EXPECTED[target][1])
        self.assertEqual(before[2:], self.state()[2:])
        helper = sys.modules['codex_chat_read_state']
        self.assertEqual(update.signature(helper.read_state), update.HELPER_SIGNATURE)
        after = self.state()
        self.assertEqual(update.apply(self.runtime),
                         {'status': 'already_applied', 'baseCommit': update.BASE_COMMIT})
        self.assertEqual(after, self.state())

    def test_partial_known_update_finishes_without_replacing_helper(self):
        update.apply(self.runtime)
        helper = sys.modules['codex_chat_read_state']
        self.owner.snapshot.__code__ = self.compile('codex_runtime.Runtime.snapshot').__code__
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(sys.modules['codex_chat_read_state'], helper)

    def test_unknown_code_defaults_keywords_and_namespace_refuse(self):
        for target in update.EXPECTED:
            module, owner, name = target.split('.')
            cls = getattr(self.modules[module], owner)
            original = getattr(cls, name)
            for kind in ('code', 'defaults', 'keywords', 'namespace'):
                function = self.compile(target,
                    namespace=dict(vars(self.modules[module])) if kind == 'namespace' else None)
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
                if kind == 'defaults':
                    function.__defaults__ = ('unknown',)
                if kind == 'keywords':
                    function.__kwdefaults__ = {'unknown': True}
                setattr(cls, name, function)
                before = self.state()
                with self.subTest(target=target, kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    update.apply(self.runtime)
                self.assertEqual(before, self.state())
            setattr(cls, name, original)

    def test_instance_and_inherited_method_shadow_refuse(self):
        for name in ('snapshot', 'chat_organization'):
            setattr(self.runtime, name, lambda *args: None)
            before = self.state()
            with self.subTest(name=name), self.assertRaisesRegex(RuntimeError, 'override'):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())
            delattr(self.runtime, name)
        self.owner.chat_organization = self.compile('codex_work.WorkMixin.chat_organization')
        with self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)

    def test_source_paths_and_hashes_refuse_before_mutation(self):
        before = self.state()
        for module in self.modules.values():
            with patch.object(module, '__file__', '/unknown/' + module.__name__ + '.py'), \
                    self.assertRaisesRegex(RuntimeError, 'location'):
                update.apply(self.runtime)
        with patch.object(update, 'HELPER_SHA', '0' * 64), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        read = Path.read_text
        def unreviewed(path, *args, **kwargs):
            return read(path, *args, **kwargs).replace('readStateSupported', 'unknownReadStateSupported')
        with patch.object(Path, 'read_text', unreviewed), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_loaded_helper_path_code_and_namespace_refuse(self):
        update.apply(self.runtime)
        helper = sys.modules['codex_chat_read_state']
        before = self.state()
        with patch.object(helper, '__file__', '/unknown/codex_chat_read_state.py'), \
                self.assertRaisesRegex(RuntimeError, 'helper'):
            update.apply(self.runtime)
        original = helper.read_state
        for namespace in (vars(helper), dict(vars(helper))):
            function = update.compile_function((ROOT / 'scripts/codex_chat_read_state.py').read_text(),
                                                None, 'read_state', namespace)
            if namespace is vars(helper):
                function.__defaults__ = ('unknown',)
            with patch.object(helper, 'read_state', function), self.assertRaisesRegex(RuntimeError, 'helper'):
                update.apply(self.runtime)
        self.assertIs(helper.read_state, original)
        self.assertEqual(before, self.state())

    def test_mid_install_failure_restores_callbacks_and_added_module(self):
        before = self.state()
        target = self.work.chat_organization
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled chat status failure')
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled chat status'):
                update.apply(self.runtime)
        finally:
            armed[0] = False
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
