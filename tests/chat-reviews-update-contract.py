#!/usr/bin/env python3
"""Chat review updates preserve live work and reject unknown implementations."""
import copy
import importlib.util
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
import codex_rules
import codex_work
import codex_chat_reviews_update as update


class ChatReviewsUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(
            ['git', 'show', update.BASE_COMMIT + ':scripts/' + name + '.py'], cwd=ROOT, text=True)
            for name in ('codex_work', 'codex_rules')}

    def setUp(self):
        self.modules = {}
        self.owners = {}
        for source, owner_name, method in ((codex_work, 'WorkMixin', 'chat_organization'),
                                            (codex_rules, 'RulesMixin', 'rules_tick')):
            module = ModuleType(source.__name__)
            vars(module).update(vars(source))
            owner = type(owner_name, (getattr(source, owner_name),), {'__module__': source.__name__})
            setattr(module, owner_name, owner)
            self.modules[source.__name__] = module
            self.owners[method] = owner
            setattr(owner, method, self.compile(source.__name__, owner_name, method))
        module = ModuleType('codex_runtime')
        vars(module).update(vars(codex_runtime))
        runtime_class = type('Runtime', (*self.owners.values(), codex_runtime.Runtime), {'__module__': 'codex_runtime'})
        module.Runtime = runtime_class
        module.WorkMixin = self.owners['chat_organization']
        module.RulesMixin = self.owners['rules_tick']
        self.modules['codex_runtime'] = module
        self.runtime = runtime_class.__new__(runtime_class)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'live': object()}
        self.runtime.servers = {'default': object()}
        self.runtime.agents = {'agent': {'model': 'gpt-5.6-sol', 'inFlight': True, 'turnId': 'active'}}
        self.runtime.events = [{'id': 'pending', 'status': 'pending'}]
        self.runtime.enqueue = lambda *args: self.fail('The update must not enqueue work')
        self.runtime.db = lambda *args: self.fail('The update must not open the database')
        self.runtime.changed = threading.Event()
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)
        sys.modules.pop(update.HELPER_NAME, None)

    def compile(self, module_name, owner_name, method, current=False, foreign_globals=False):
        source = ((ROOT / 'scripts' / (module_name + '.py')).read_text()
                  if current else self.old[module_name])
        namespace = vars(self.modules[module_name])
        return update.compile_function(source, owner_name, method,
                                       dict(namespace) if foreign_globals else namespace)

    def state(self):
        return (tuple((getattr(owner, name), getattr(owner, name).__code__)
                      for name, owner in self.owners.items()),
                copy.deepcopy(self.runtime.agents), copy.deepcopy(self.runtime.events),
                tuple(self.runtime.connections.items()), tuple(self.runtime.servers.items()),
                self.runtime.changed.is_set(), sys.modules.get(update.HELPER_NAME))

    def helper(self):
        path = ROOT / 'scripts' / (update.HELPER_NAME + '.py')
        spec = importlib.util.spec_from_file_location(update.HELPER_NAME, path)
        helper = importlib.util.module_from_spec(spec)
        exec(compile(path.read_bytes(), str(path), 'exec', dont_inherit=True), vars(helper))
        return helper

    def test_apply_preserves_callbacks_and_work_then_is_idempotent(self):
        before = self.state()
        callbacks = [getattr(self.runtime, name) for name in self.owners]
        result = update.apply(self.runtime)
        self.assertEqual(result, {'status': 'applied', 'baseCommit': update.BASE_COMMIT,
                                  'methods': list(update.EXPECTED)})
        for callback, original, allowed in zip(callbacks, before[0], update.EXPECTED.values()):
            self.assertIs(callback.__func__, original[0])
            self.assertEqual(update.signature(callback.__func__), allowed[1])
        after = self.state()
        self.assertEqual(after[1:-1], before[1:-1])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
        self.assertEqual(self.state(), after)

    def test_known_preloaded_helper_and_mixed_method_versions(self):
        helper = self.helper()
        sys.modules[update.HELPER_NAME] = helper
        owner = self.owners['chat_organization']
        owner.chat_organization = self.compile('codex_work', 'WorkMixin', 'chat_organization', current=True)
        callback = self.runtime.chat_organization
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(self.runtime.chat_organization.__func__, callback.__func__)
        self.assertIs(sys.modules[update.HELPER_NAME], helper)
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_code_defaults_keywords_namespace_and_closure_refuse_atomically(self):
        for method, module_name, owner_name in [('chat_organization', 'codex_work', 'WorkMixin'),
                                               ('rules_tick', 'codex_rules', 'RulesMixin')]:
            owner = self.owners[method]
            original = getattr(owner, method)
            for kind in ('code', 'defaults', 'keywords', 'namespace', 'closure'):
                function = self.compile(module_name, owner_name, method, foreign_globals=kind == 'namespace')
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
                elif kind == 'defaults':
                    function.__defaults__ = ('unknown',)
                elif kind == 'keywords':
                    function.__kwdefaults__ = {'unknown': True}
                elif kind == 'closure':
                    def factory():
                        value = 1
                        return lambda self: value
                    function = factory()
                setattr(owner, method, function)
                before = self.state()
                with self.subTest(method=method, kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    update.apply(self.runtime)
                self.assertEqual(self.state(), before)
            setattr(owner, method, original)

    def test_overrides_locations_class_identity_and_source_refuse(self):
        before = self.state()
        for name, owner in self.owners.items():
            with patch.object(self.runtime, name, lambda *args: None), self.assertRaisesRegex(RuntimeError, 'override'):
                update.apply(self.runtime)
            with patch.object(type(self.runtime), name, getattr(owner, name)), self.assertRaisesRegex(RuntimeError, 'override'):
                update.apply(self.runtime)
            with patch.object(owner, '__module__', 'unknown'), self.assertRaisesRegex(RuntimeError, 'class'):
                update.apply(self.runtime)
        for module in self.modules.values():
            with patch.object(module, '__file__', '/unknown/module.py'), self.assertRaisesRegex(RuntimeError, 'location'):
                update.apply(self.runtime)
        for name in ('WorkMixin', 'RulesMixin'):
            with patch.object(self.modules['codex_runtime'], name, object()), self.assertRaisesRegex(RuntimeError, 'class'):
                update.apply(self.runtime)
        with patch.object(update, 'HELPER_SHA', '0' * 64), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        altered = {key: (old, '0' * 64) for key, (old, _) in update.EXPECTED.items()}
        with patch.object(update, 'EXPECTED', altered), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            update.apply(self.runtime)
        self.assertEqual(self.state(), before)

    def test_unknown_preloaded_helper_refuses_without_replacing_it(self):
        for kind in ('location', 'name', 'extra', 'dependency', 'function', 'namespace', 'builtins'):
            helper = self.helper()
            if kind == 'location':
                helper.__file__ = '/unknown/module.py'
            elif kind == 'name':
                helper.__name__ = 'unknown'
            elif kind == 'extra':
                helper.unknown = True
            elif kind == 'dependency':
                helper.native_thread_block = lambda *args: False
            elif kind == 'function':
                helper.review_tick.__code__ = helper.review_tick.__code__.replace(
                    co_consts=helper.review_tick.__code__.co_consts + ('unknown',))
            elif kind == 'namespace':
                helper.review_tick = self.helper().review_tick
            elif kind == 'builtins':
                helper.__builtins__ = dict(helper.__builtins__)
            sys.modules[update.HELPER_NAME] = helper
            before = self.state()
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'helper'):
                update.apply(self.runtime)
            self.assertEqual(self.state(), before)

    def test_second_assignment_failure_rolls_back_both_methods_and_helper(self):
        for preloaded in (False, True):
            if preloaded:
                sys.modules[update.HELPER_NAME] = self.helper()
            before = self.state()
            target = self.owners['rules_tick'].rules_tick
            armed = [True]
            def audit(event, args):
                if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                    armed[0] = False
                    raise RuntimeError('Controlled second assignment failure')
            sys.addaudithook(audit)
            try:
                with self.subTest(preloaded=preloaded), self.assertRaisesRegex(RuntimeError, 'Controlled second'):
                    update.apply(self.runtime)
            finally:
                armed[0] = False
            self.assertEqual(self.state(), before)
            self.assertTrue(self.runtime.lock.acquire(blocking=False))
            self.runtime.lock.release()

    def test_closed_busy_runtime_and_python_refuse(self):
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
        self.assertEqual(self.state(), before)


if __name__ == '__main__':
    unittest.main()
