#!/usr/bin/env python3
"""Exercise the exact old code, live callbacks, rollback and catalog transition."""
import concurrent.futures
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
import codex_catalog
import codex_canvas
import codex_agent_modes_update as update


class AgentModesUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(
            ['git', 'show', update.BASE_COMMIT + ':scripts/' + name + '.py'], cwd=ROOT, text=True)
            for name in {key.split('.')[0] for key in update.EXPECTED}}

    def setUp(self):
        self.modules, self.owners = {}, {}
        for name in self.old:
            module = ModuleType(name)
            vars(module).update(vars(importlib.import_module(name)))
            self.modules[name] = module
        for target in update.EXPECTED:
            module, *path = target.split('.')
            if len(path) == 2 and path[0] != 'Runtime':
                self.owners[module + '.' + path[0]] = type(path[0], (), {'__module__': module})
        mixins = [owner for name, owner in self.owners.items() if name != 'codex_catalog.ModelCatalogCache']
        self.owner = type('Runtime', tuple(mixins), {'__module__': 'codex_runtime'})
        self.owners['codex_runtime.Runtime'] = self.owner
        for name, owner in self.owners.items():
            module, cls = name.split('.')
            setattr(self.modules[module], cls, owner)
        self.handler = type('Handler', (), {'__module__': 'codex_canvas',
                            '__qualname__': 'make_server.<locals>.Handler'})
        canvas_owner = type('Canvas', (), {'__module__': 'codex_canvas'})
        descriptor = codex_canvas.Canvas.runtime
        canvas_owner.runtime = property(
            FunctionType(descriptor.fget.__code__, vars(self.modules['codex_canvas'])),
            FunctionType(descriptor.fset.__code__, vars(self.modules['codex_canvas'])))
        self.modules['codex_canvas'].Canvas = canvas_owner
        self.canvas = canvas_owner()
        def cell(value=None):
            return (lambda: value).__closure__[0]
        self.cells = {key: cell(value) for key, value in {
            'canvas': self.canvas, 'remote': object(), 'token': 'token',
            'snapshot': None, 'sync': None, 'terminals': None, 'cost_reader': [None],
            'terminal_lock': threading.RLock(), 'sync_store': [None], 'terminal_manager': [None],
        }.items()}
        for name in ('snapshot', 'sync', 'terminals'):
            self.cells[name].cell_contents = self.compile('codex_canvas', ('make_server', name))[0]
        for name in ('do_GET', 'trusted', 'stream_sync'):
            setattr(self.handler, name, self.compile('codex_canvas', ('make_server', 'Handler', name))[0])
        for target in update.EXPECTED:
            owner, method = self.target(target)
            module, *path = target.split('.')
            function, static = self.compile(module, tuple(path))
            setattr(owner, method, staticmethod(function) if static else function)
        runtime_module = self.modules['codex_runtime']
        vars(runtime_module).pop('DEFAULT_LEAD_MODEL', None)
        runtime_module.LEAD_MODELS = ('gpt-6-astra', 'gpt-5.6-sol')
        self.runtime = self.owner()
        self.runtime.lock, self.runtime.closed = threading.RLock(), False
        self.canvas.runtime = self.runtime
        self.runtime.connections = {'active': object()}
        self.runtime.requests = {'unknown': {'stage': 'running'}}
        self.cache = self.owners['codex_catalog.ModelCatalogCache']()
        # Existing constructor is unchanged. Use it to create the original layout.
        codex_catalog.ModelCatalogCache.__init__(self.cache, wait_seconds=0.001)
        self.runtime._catalog_cache = self.cache
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)
        sys.modules.pop(update.HELPER_NAME, None)

    def target(self, target):
        name, *path = target.split('.')
        return (self.handler if len(path) == 3 else self.modules[name] if len(path) == 1 else self.owners[name + '.' + path[0]], path[-1])

    def compile(self, module, path, namespace=None):
        namespace = vars(self.modules[module]) if namespace is None else namespace
        function, _ = update.source_function(self.old[module], path, namespace)
        closure = tuple(self.cells[name] for name in function.__code__.co_freevars) or None
        return update.source_function(self.old[module], path, namespace, closure=closure)

    def apply(self):
        return update.apply(self.runtime, self.handler)

    def functions(self):
        return [getattr(*self.target(key)) for key in update.EXPECTED]

    def state(self):
        return (tuple((f, f.__code__, f.__defaults__, f.__kwdefaults__) for f in self.functions()),
                vars(self.modules['codex_runtime']).get('DEFAULT_LEAD_MODEL', update.MISSING),
                sys.modules.get(update.HELPER_NAME), self.runtime.connections, repr(self.runtime.requests),
                self.runtime._catalog_cache, dict(self.cache.entries),
                self.handler.do_GET.__closure__, self.handler.do_POST.__closure__)

    def test_callback_objects_static_descriptors_and_state_survive(self):
        before = self.state()
        callbacks = self.functions()
        records_descriptor = vars(self.owner)['records']
        self.assertEqual(self.apply()['status'], 'applied')
        self.assertEqual(callbacks, self.functions())
        self.assertIs(vars(self.owner)['records'], records_descriptor)
        self.assertEqual(before[3:], self.state()[3:])
        for key, function in zip(update.EXPECTED, callbacks):
            self.assertEqual(update.signature(function), update.EXPECTED[key][1])
        after = self.state()
        self.assertEqual(self.apply()['status'], 'already_applied')
        self.assertEqual(after, self.state())

    def test_manager_discovers_existing_server_without_new_connections(self):
        server_type = type('LocalServer', (ThreadingHTTPServer,), {
            '__module__': 'codex_canvas', '__qualname__': 'make_server.<locals>.LocalServer'})
        server = server_type.__new__(server_type)
        server.RequestHandlerClass = self.handler
        server.socket = socket.socket()
        self.addCleanup(server.socket.close)
        self.assertIs(update._find_handler(self.runtime), self.handler)
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        server.socket.close()
        with self.assertRaisesRegex(RuntimeError, 'one live HTTP server'):
            update.apply(self.runtime)

    def test_each_unknown_code_default_global_and_module_rejects_without_changes(self):
        for key in update.EXPECTED:
            owner, method = self.target(key)
            original = vars(owner)[method]
            module, *path = key.split('.')
            for kind in ('code', 'defaults', 'globals', 'module'):
                namespace = vars(self.modules[module])
                function, static = self.compile(module, tuple(path),
                    dict(namespace) if kind == 'globals' else namespace)
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
                elif kind == 'defaults': function.__defaults__ = ('unknown',)
                elif kind == 'module': function.__module__ = 'unknown'
                setattr(owner, method, staticmethod(function) if static else function)
                before = self.state()
                with self.subTest(target=key, kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    self.apply()
                self.assertEqual(before, self.state())
            setattr(owner, method, original)

    def test_unknown_helper_global_origin_cache_and_overrides_reject(self):
        before = self.state()
        with patch.object(update, 'HELPER_SHA', '0' * 64), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            self.apply()
        with patch.object(self.runtime, 'send', lambda *args: None, create=True), self.assertRaisesRegex(RuntimeError, 'override'):
            self.apply()
        with patch.object(self.cache, 'read', lambda *args: None, create=True), self.assertRaisesRegex(RuntimeError, 'override'):
            self.apply()
        with patch.object(self.runtime, '_catalog_cache', object()), self.assertRaisesRegex(RuntimeError, 'Unknown'):
            self.apply()
        with patch.object(self.modules['codex_work'], '__file__', '/unknown'), self.assertRaisesRegex(RuntimeError, 'location'):
            self.apply()
        with patch.object(self.modules['codex_runtime'], 'DEFAULT_LEAD_MODEL', 'unknown', create=True), self.assertRaisesRegex(RuntimeError, 'Unknown'):
            self.apply()
        self.assertEqual(before, self.state())
        self.apply()
        helper = sys.modules[update.HELPER_NAME]
        helper.change_mode = lambda *args: None
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'helper function'):
            self.apply()
        self.assertEqual(before, self.state())

    def test_closed_runtime_version_and_http_closure_refuse(self):
        before = self.state()
        with patch.object(self.runtime, 'closed', True), self.assertRaisesRegex(RuntimeError, 'closed'):
            self.apply()
        with patch.object(sys, 'version_info', (3, 13)), self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            self.apply()
        with patch.object(self.canvas, '_runtime', object()), self.assertRaisesRegex(RuntimeError, 'another runtime'):
            self.apply()
        self.assertEqual(before, self.state())

    def test_partial_assignment_rolls_back_functions_defaults_helper_and_binding(self):
        target = self.functions()[3]
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled assignment failure')
        sys.addaudithook(audit)
        before = self.state()
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled assignment'):
                self.apply()
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())
        self.assertEqual(self.apply()['status'], 'applied')

    def test_old_settings_frame_settles_before_cutover(self):
        entered, release = threading.Event(), threading.Event()
        errors = []
        class PendingData(dict):
            def get(self, key, *args):
                entered.set()
                if not release.wait(10): raise AssertionError('Fixture did not release')
                raise ValueError('Settings call settled')
        def call():
            try: self.runtime.conversation_settings('lead', PendingData())
            except ValueError as error: errors.append(str(error))
        thread = threading.Thread(target=call)
        thread.start()
        try:
            self.assertTrue(entered.wait(5))
            before = self.state()
            with self.assertRaisesRegex(RuntimeError, 'earlier call: codex_runtime.Runtime.conversation_settings'):
                self.apply()
            self.assertEqual(before, self.state())
        finally:
            release.set()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, ['Settings call settled'])
        self.assertEqual(self.apply()['status'], 'applied')

    def test_old_catalog_read_frame_must_settle_before_cutover(self):
        entered = threading.Event()
        native = concurrent.futures.Future()
        results = []
        class Server:
            def submit(self, method, params):
                entered.set()
                return native
        server = Server()
        self.cache.wait_seconds = 10
        thread = threading.Thread(target=lambda: results.append(
            self.cache.read('account', server, 'connection', lambda: True)))
        thread.start()
        try:
            self.assertTrue(entered.wait(5))
            before = self.state()
            with self.assertRaisesRegex(RuntimeError, 'earlier call: codex_catalog.ModelCatalogCache.read'):
                self.apply()
            self.assertEqual(before, self.state())
        finally:
            native.set_result({'data': [{'model': 'old-page'}], 'nextCursor': 'next'})
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(results[0]['nextCursor'], 'next')
        self.assertEqual(self.apply()['status'], 'applied')

    def test_legacy_pending_page_future_is_retained_then_complete_catalog_replaces_it(self):
        calls, native = [], []
        class Server:
            def submit(self, method, params):
                calls.append((method, params))
                future = concurrent.futures.Future()
                native.append(future)
                if len(native) == 1:
                    error = RuntimeError('Unknown pipe write outcome')
                    error.submitted = future
                    raise error
                return future
        server = Server()
        current = lambda: True
        with self.assertRaises(codex_catalog.CatalogPending):
            self.cache.read('account', server, 'connection', current)
        entry = self.cache.entries['account']
        future = entry['future']
        self.assertEqual(len(calls), 1)
        self.apply()
        self.assertIs(self.cache.entries['account'], entry)
        self.assertIs(entry['future'], future)
        with self.assertRaises(codex_catalog.CatalogPending):
            self.cache.read('account', server, 'connection', current)
        self.assertEqual(len(calls), 1)
        native[0].set_result({'data': [{'model': 'old-first'}], 'nextCursor': 'old-second'})
        self.assertTrue(future.done())
        with self.assertRaises(codex_catalog.CatalogPending):
            self.cache.read('account', server, 'connection', current)
        self.assertEqual(len(calls), 2)
        native[1].set_result({'data': [{'model': 'first'}], 'nextCursor': 'second'})
        self.assertEqual(len(calls), 3)
        self.assertEqual(calls[2], ('model/list', {'limit': 100, 'cursor': 'second'}))
        native[2].set_result({'data': [{'model': 'last'}], 'nextCursor': None})
        result = self.cache.read('account', server, 'connection', current)
        self.assertEqual([row['model'] for row in result['data']], ['first', 'last'])
        self.assertEqual(len(calls), 3)

    def test_legacy_successful_cache_never_admits_partial_page(self):
        class Server:
            def submit(self, method, params):
                future = concurrent.futures.Future()
                future.set_result({'data': [{'model': 'complete'}]})
                return future
        server = Server()
        future = concurrent.futures.Future()
        future.set_result({'data': [{'model': 'partial'}], 'nextCursor': 'next'})
        self.cache.entries['account'] = {'server': server, 'connectionId': 'connection',
            'future': future, 'expires': self.cache.clock() + 300, 'value': future.result()}
        self.apply()
        result = self.cache.read('account', server, 'connection', lambda: True)
        self.assertEqual(result['data'], [{'model': 'complete'}])


if __name__ == '__main__':
    unittest.main()
