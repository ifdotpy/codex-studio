#!/usr/bin/env python3
"""Team cutover retains callbacks and rejects unknown code or active old frames."""
import copy
import importlib
from http.server import ThreadingHTTPServer
import socket
from pathlib import Path
import subprocess
import sys
import threading
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_canvas
import codex_team_isolation_update as update


def cell(value=None):
    return (lambda: value).__closure__[0]


class TeamIsolationUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(
            ['git', 'show', update.BASE_COMMIT + ':scripts/' + name + '.py'], cwd=ROOT, text=True)
            for name in {key.split('.')[0] for key in update.EXPECTED}}

    def setUp(self):
        self.modules = {}
        for name in self.old:
            original = importlib.import_module(name)
            module = ModuleType(name)
            vars(module).update(vars(original))
            self.modules[name] = module
        self.owners = {}
        for key in update.EXPECTED | update.NEW_METHODS:
            module, *path = key.split('.')
            if len(path) == 2 and path[0] != 'Runtime':
                self.owners[module + '.' + path[0]] = type(path[0], (), {'__module__': module})
        mixins = [owner for key, owner in self.owners.items() if not key.endswith('.Canvas')]
        self.owner = type('Runtime', tuple(mixins), {'__module__': 'codex_runtime'})
        self.owners['codex_runtime.Runtime'] = self.owner
        for key, owner in self.owners.items():
            module, name = key.split('.')
            setattr(self.modules[module], name, owner)
            if name != 'Canvas':
                setattr(self.modules['codex_runtime'], name, owner)
        self.canvas_owner = self.owners['codex_canvas.Canvas']
        descriptor = codex_canvas.Canvas.runtime
        self.canvas_owner.runtime = property(
            FunctionType(descriptor.fget.__code__, vars(self.modules['codex_canvas'])),
            FunctionType(descriptor.fset.__code__, vars(self.modules['codex_canvas'])))
        self.runtime = self.owner()
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'active': object()}
        self.runtime.requests = {'unknown': {'stage': 'running'}}
        self.runtime.agents = {'active': {'status': 'running', 'turnId': 'preserved'}}
        self.canvas = self.canvas_owner()
        self.canvas.runtime = self.runtime
        self.canvas.lock = threading.RLock()
        self.handler = type('Handler', (), {'__module__': 'codex_canvas',
                            '__qualname__': 'make_server.<locals>.Handler'})
        self.cells = {key: cell(value) for key, value in {
            'canvas': self.canvas, 'remote': object(), 'token': 'token',
            'snapshot': None, 'sync': None, 'terminals': None, 'cost_reader': [None],
            'terminal_lock': threading.RLock(), 'sync_store': [None], 'terminal_manager': [None],
        }.items()}
        for name in ('snapshot', 'sync', 'terminals'):
            self.cells[name].cell_contents = self.compile('codex_canvas', ('make_server', name))
        for name in ('do_GET', 'do_POST', 'trusted', 'stream_sync'):
            setattr(self.handler, name, self.compile('codex_canvas', ('make_server', 'Handler', name)))
        for key in update.EXPECTED:
            module, *path = key.split('.')
            owner = self.modules[module] if len(path) == 1 else getattr(self.modules[module], path[0])
            setattr(owner, path[-1], self.compile(module, tuple(path)))
        runtime_module = self.modules['codex_runtime']
        runtime_module.INSTRUCTIONS = update.source_instructions(self.old['codex_runtime'])
        runtime_module.TOOLS = update._tools(self.old['codex_runtime'], codex_runtime.TOOLS)
        self.assertEqual(update.digest(runtime_module.TOOLS), update.TOOLS[0])
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)
        sys.modules.pop(update.HELPER_NAME, None)

    def compile(self, module, path, namespace=None):
        namespace = vars(self.modules[module]) if namespace is None else namespace
        function, _ = update.source_function(self.old[module], path, namespace)
        closure = tuple(self.cells[name] for name in function.__code__.co_freevars) or None
        return update.source_function(self.old[module], path, namespace, closure=closure)[0]

    def functions(self):
        result = []
        for key in update.EXPECTED:
            module, *path = key.split('.')
            owner = self.modules[module] if len(path) == 1 else getattr(self.modules[module], path[0])
            result.append(getattr(owner, path[-1]))
        return result

    def state(self):
        module = self.modules['codex_runtime']
        return (tuple((f, f.__code__) for f in self.functions()), module.INSTRUCTIONS,
                update.digest(module.TOOLS), sys.modules.get(update.HELPER_NAME),
                tuple((key, hasattr(self.owners[key.rsplit('.', 1)[0]], key.rsplit('.', 1)[1]))
                      for key in update.NEW_METHODS), self.runtime.connections,
                repr(self.runtime.requests), repr(self.runtime.agents),
                self.handler.do_GET.__closure__, self.handler.do_POST.__closure__)

    def apply(self):
        return update.apply(self.runtime, self.handler)

    def test_callbacks_tools_and_http_closures_preserved_without_replay(self):
        before = self.state()
        callbacks = self.functions()
        tools = self.modules['codex_runtime'].TOOLS
        self.assertEqual(self.apply()['status'], 'applied')
        self.assertEqual(callbacks, self.functions())
        self.assertEqual(before[5:], self.state()[5:])
        self.assertIs(tools, self.modules['codex_runtime'].TOOLS)
        self.assertEqual(update.digest(tools), update.TOOLS[1])
        for key, function in zip(update.EXPECTED, callbacks):
            self.assertEqual(update.signature(function), update.EXPECTED[key][1])
        self.assertTrue(all(present for _, present in self.state()[4]))
        after = self.state()
        self.assertEqual(self.apply()['status'], 'already_applied')
        self.assertEqual(after, self.state())

    def test_manager_discovers_the_exact_open_server_and_applies(self):
        server_type = type('LocalServer', (ThreadingHTTPServer,), {
            '__module__': 'codex_canvas', '__qualname__': 'make_server.<locals>.LocalServer'})
        server = server_type.__new__(server_type)
        server.RequestHandlerClass = self.handler
        server.socket = socket.socket()
        self.addCleanup(server.socket.close)
        self.assertIs(update._canvas_runtime(self.canvas), self.runtime)
        self.assertIs(update._find_handler(self.runtime), self.handler)
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        server.socket.close()
        with self.assertRaisesRegex(RuntimeError, 'one live HTTP server'):
            update.apply(self.runtime)

    def test_captured_message_callback_rejects_global_target_without_database_writes(self):
        callback = self.runtime.chat_message
        class Database:
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def execute(self, *args): raise AssertionError('Unexpected database write')
        self.runtime.db = Database
        self.runtime.agent = lambda key, db: {'id': key, 'rootId': key, 'parentId': None, 'autoWake': True, 'epoch': 0}
        self.apply()
        with self.assertRaisesRegex(ValueError, 'one team'):
            callback('lead', 'all', 'private information', 'id')

    def test_each_unknown_function_and_changed_defaults_globals_module_refuse(self):
        for key in update.EXPECTED:
            module, *path = key.split('.')
            owner = self.modules[module] if len(path) == 1 else getattr(self.modules[module], path[0])
            original = getattr(owner, path[-1])
            for kind in ('code', 'defaults', 'globals', 'module'):
                function = self.compile(module, tuple(path), dict(vars(self.modules[module])) if kind == 'globals' else None)
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
                elif kind == 'defaults': function.__defaults__ = ('unknown',)
                elif kind == 'module': function.__module__ = 'unknown'
                setattr(owner, path[-1], function)
                before = self.state()
                with self.subTest(target=key, kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    self.apply()
                self.assertEqual(before, self.state())
            setattr(owner, path[-1], original)

    def test_old_authorization_frame_rejects_then_retry_applies_without_replay(self):
        entered, release = threading.Event(), threading.Event()
        errors = []
        class PendingText(str):
            def strip(self):
                entered.set()
                if not release.wait(10): raise AssertionError('Fixture did not release')
                raise ValueError('Fixture call settled')
        callback = self.runtime.chat_message
        def run():
            try: callback('sender', 'target', PendingText('message'), 'id')
            except ValueError as error: errors.append(str(error))
        thread = threading.Thread(target=run)
        thread.start()
        try:
            self.assertTrue(entered.wait(5))
            before = self.state()
            with self.assertRaisesRegex(RuntimeError, 'earlier call: codex_runtime.Runtime.chat_message'):
                self.apply()
            self.assertEqual(before, self.state())
        finally:
            release.set()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, ['Fixture call settled'])
        self.assertEqual(self.apply()['status'], 'applied')

    def test_old_start_frame_during_preparation_blocks_cutover_without_submission(self):
        entered, release = threading.Event(), threading.Event()
        errors = []
        agent = {'id': 'worker', 'epoch': 0, 'autoWake': True,
                 'startAttempt': {'id': 'attempt', 'settingsFixed': True, 'submitted': False}}
        class Database:
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def execute(self, *args): raise AssertionError('Unexpected database mutation')
        self.runtime.db = Database
        self.runtime.agent = lambda *args: agent
        self.runtime.start_error = lambda key, attempt, error, **kwargs: errors.append(str(error))
        def prepare(current):
            entered.set()
            if not release.wait(10): raise AssertionError('Fixture did not release')
            raise ValueError('Fixture preparation settled')
        self.runtime.prepare = prepare
        self.runtime.submit_reserved = lambda *args: self.fail('Unexpected native submission')
        thread = threading.Thread(target=self.runtime.start, args=(agent, [{'id': 'event'}]))
        thread.start()
        try:
            self.assertTrue(entered.wait(5))
            before = self.state()
            with self.assertRaisesRegex(RuntimeError, 'earlier call: codex_runtime.Runtime.start'):
                self.apply()
            self.assertEqual(before, self.state())
        finally:
            release.set()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, ['Fixture preparation settled'])
        self.assertEqual(self.apply()['status'], 'applied')

    def test_partial_assignment_failure_rolls_back_code_methods_helper_and_tools(self):
        target = self.functions()[3]
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled team assignment failure')
        sys.addaudithook(audit)
        before = self.state()
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled team'):
                self.apply()
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())
        self.assertEqual(self.apply()['status'], 'applied')

    def test_source_helper_override_location_closed_runtime_and_tools_refuse(self):
        before = self.state()
        with patch.object(update, 'HELPER_SHA', '0' * 64), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            self.apply()
        with patch.object(self.runtime, 'send', lambda *args: None), self.assertRaisesRegex(RuntimeError, 'override'):
            self.apply()
        with patch.object(self.canvas, 'agent_messages', lambda *args: None, create=True), self.assertRaisesRegex(RuntimeError, 'override'):
            self.apply()
        with patch.object(self.modules['codex_work'], '__file__', '/unknown'), self.assertRaisesRegex(RuntimeError, 'location'):
            self.apply()
        with patch.object(self.modules['codex_runtime'], 'TOOLS', []), self.assertRaisesRegex(RuntimeError, 'tools'):
            self.apply()
        self.runtime.closed = True
        with self.assertRaisesRegex(RuntimeError, 'closed'):
            self.apply()
        self.runtime.closed = False
        self.assertEqual(before, self.state())
        with self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            update.apply(object(), self.handler)
        with patch.object(sys, 'version_info', (3, 13)), self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            self.apply()

    def test_loaded_unknown_helper_refuses_without_mutation(self):
        self.apply()
        helper = sys.modules[update.HELPER_NAME]
        helper.DENIED = 'different policy'
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'helper constant'):
            self.apply()
        self.assertEqual(before, self.state())


if __name__ == '__main__':
    unittest.main()
