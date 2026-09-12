#!/usr/bin/env python3
"""The metadata hotpatch preserves HTTP callbacks, ownership, and live state."""
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_file_viewers_update as update
import codex_workspace
import codex_canvas


def cell(value=None):
    return (lambda: value).__closure__[0]


class FileViewersUpdate(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.baseline = subprocess.check_output(
            ['git', 'show', 'a6fa02e:scripts/codex_canvas.py'], cwd=ROOT, text=True)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='studio-file-update-')
        self.modules = {}
        for original in (codex_workspace, codex_canvas):
            module = ModuleType(original.__name__)
            module.__dict__.update(vars(original))
            self.modules[original.__name__] = module
        self.mixin = type('WorkspaceMixin', (), {'__module__': 'codex_workspace'})
        self.modules['codex_workspace'].WorkspaceMixin = self.mixin
        self.owner = type('Runtime', (self.mixin,), {})
        self.runtime = self.owner()
        self.runtime.root = Path(self.temporary.name)
        self.runtime.closed = False
        self.runtime.lock = threading.RLock()
        self.runtime.connections = {'one': object(), 'two': object()}
        self.runtime.database = object()
        self.handler = type('Handler', (), {'__module__': 'codex_canvas',
                            '__qualname__': 'make_server.<locals>.Handler'})
        self.cells = {name: cell(value) for name, value in {
            'canvas': SimpleNamespace(runtime=self.runtime), 'cost_reader': [None],
            'remote': object(), 'snapshot': None, 'sync': None, 'terminals': None,
            'terminal_lock': threading.RLock(), 'token': 'fixture-token',
            'sync_store': [None], 'terminal_manager': [None],
        }.items()}
        for name in ('snapshot', 'sync', 'terminals'):
            self.cells[name].cell_contents = self.compile(('make_server', name))
        for name in ('do_GET', 'do_POST', 'trusted', 'stream_sync'):
            setattr(self.handler, name, self.compile(('make_server', 'Handler', name)))
        self.assertEqual(update.signature(self.handler.do_GET), update._EXPECTED['route_old'])
        self.patch_modules = patch.dict(sys.modules, self.modules)
        self.patch_modules.start()

    def compile(self, path, namespace=None):
        namespace = vars(self.modules['codex_canvas']) if namespace is None else namespace
        function, _ = update.source_function(self.baseline, path, namespace)
        closure = tuple(self.cells[name] for name in function.__code__.co_freevars) or None
        return update.source_function(self.baseline, path, namespace, closure=closure)[0]

    def tearDown(self):
        self.patch_modules.stop()
        self.temporary.cleanup()

    def state(self):
        return (dict(vars(self.mixin)), dict(vars(self.handler)), self.handler.do_GET.__code__,
                self.handler.do_GET.__closure__, self.runtime.connections, self.runtime.database)

    def test_real_baseline_preserves_callback_cells_and_connections_then_is_idempotent(self):
        callback = self.handler.do_GET
        cells = callback.__closure__
        contents = tuple(item.cell_contents for item in cells)
        connections = self.runtime.connections
        database = self.runtime.database
        build = self.modules['codex_canvas'].BACKEND_BUILD
        receipt = update.apply(self.runtime, self.handler)
        self.assertEqual(receipt, {'status': 'applied', 'baseCommit': 'a6fa02e',
                                   'methods': ['file_info', 'do_GET']})
        self.assertIs(self.handler.do_GET, callback)
        self.assertIs(callback.__closure__, cells)
        self.assertTrue(all(before is after.cell_contents for before, after in zip(contents, cells)))
        self.assertIs(self.runtime.connections, connections)
        self.assertIs(self.runtime.database, database)
        self.assertEqual(self.modules['codex_canvas'].BACKEND_BUILD, build)
        self.assertEqual(list(self.runtime.root.iterdir()), [], 'Apply must not create runtime state')
        self.assertEqual(update.signature(callback), update._EXPECTED['route_new'])
        self.assertEqual(update.signature(self.mixin.file_info), update._EXPECTED['info'])
        before = self.state()
        self.assertEqual(update.apply(self.runtime, self.handler),
                         {'status': 'already_applied', 'baseCommit': 'a6fa02e'})
        self.assertEqual(before, self.state())

    def test_captured_http_retains_origin_guard_and_routes_exact_arguments(self):
        callback = self.handler.do_GET
        update.apply(self.runtime, self.handler)
        request = self.handler()
        request.path = '/api/file-info?agent=owner&path=report.txt&asset=stored'
        request.headers = {}
        request.client_address = ('127.0.0.1', 1)
        request.server = SimpleNamespace(server_port=1)
        request.send = lambda value, status=200: (status, value)
        calls = []
        self.runtime.file_info = lambda *args: calls.append(args) or {'name': 'report.txt'}
        request.trusted = lambda: False
        self.assertEqual(callback(request), (403, {'error': 'Local origin required'}))
        self.assertEqual(calls, [])
        request.trusted = lambda: True
        self.assertEqual(callback(request), (200, {'name': 'report.txt'}))
        self.assertEqual(calls, [('owner', 'report.txt', 'stored')])

    def test_installed_metadata_method_checks_owner_before_workspace_file(self):
        update.apply(self.runtime, self.handler)
        path = self.runtime.root / 'report.txt'
        path.write_text('retained data')
        calls = []
        def checked(agent):
            calls.append(('checked', agent))
            if agent != 'owner':
                raise ValueError('Unknown agent')
        self.runtime.checked_actor_in_own_db = checked
        self.runtime.workspace_path = lambda agent, value: calls.append(('path', agent, value)) or path
        with self.assertRaisesRegex(ValueError, 'Unknown agent'):
            self.runtime.file_info('other', 'report.txt')
        self.assertEqual(calls, [('checked', 'other')])
        calls.clear()
        metadata = self.runtime.file_info('owner', 'report.txt')
        self.assertEqual(calls, [('checked', 'owner'), ('path', 'owner', 'report.txt')])
        self.assertEqual(metadata, {'path': str(path), 'name': 'report.txt', 'mime': 'text/plain', 'size': 13})
        self.assertEqual(path.read_text(), 'retained data')

    def test_unknown_live_code_defaults_namespace_and_method_binding_refuse(self):
        for kind in ('code', 'defaults', 'namespace', 'instance', 'subclass'):
            with self.subTest(kind=kind):
                original = self.handler.do_GET
                if kind == 'code':
                    changed = self.compile(('make_server', 'Handler', 'do_GET'))
                    changed.__code__ = changed.__code__.replace(co_consts=changed.__code__.co_consts + ('unreviewed',))
                    self.handler.do_GET = changed
                elif kind == 'defaults':
                    self.handler.do_GET = self.compile(('make_server', 'Handler', 'do_GET'))
                    self.handler.do_GET.__defaults__ = ('unexpected',)
                elif kind == 'namespace':
                    self.handler.do_GET = self.compile(('make_server', 'Handler', 'do_GET'),
                                                       dict(vars(self.modules['codex_canvas'])))
                elif kind == 'instance':
                    self.runtime.file_info = lambda *args: {}
                else:
                    self.owner.file_info = lambda *args: {}
                before = self.state()
                with self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    update.apply(self.runtime, self.handler)
                self.assertEqual(before, self.state())
                self.handler.do_GET = original
                if kind == 'instance':
                    del self.runtime.file_info
                if kind == 'subclass':
                    del self.owner.file_info

    def test_unknown_existing_metadata_method_refuses(self):
        self.mixin.file_info = lambda *args: {}
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'Unknown live file metadata method'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_other_runtime_or_replaced_closure_cell_refuses(self):
        self.cells['canvas'].cell_contents.runtime = object()
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'another runtime'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())
        self.cells['canvas'].cell_contents.runtime = self.runtime
        self.handler.trusted = update.source_function(self.baseline, ('make_server', 'Handler', 'trusted'),
            vars(self.modules['codex_canvas']), closure=(cell(self.cells['remote'].cell_contents), self.cells['token']))[0]
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'closure cells do not match'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_unknown_replacement_code_and_static_decorator_refuse(self):
        original = Path.read_text
        for kind in ('code', 'decorator'):
            with self.subTest(kind=kind):
                def changed(path, *args, **kwargs):
                    source = original(path, *args, **kwargs)
                    if path.name == 'codex_workspace.py':
                        if kind == 'code':
                            return source.replace('def file_info(self, agent_id=None, path=None, asset_id=None):',
                                                  'def file_info(self, agent_id="changed", path=None, asset_id=None):')
                        return source.replace('    def file_info(', '    @staticmethod\n    def file_info(')
                    return source
                before = self.state()
                with patch.object(Path, 'read_text', changed), self.assertRaisesRegex(RuntimeError, 'Unreviewed|decorator'):
                    update.apply(self.runtime, self.handler)
                self.assertEqual(before, self.state())

    def test_route_assignment_failure_rolls_back_new_method_and_code(self):
        before = self.state()
        armed = [True]
        target = self.handler.do_GET
        def fail_once(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled route replacement failure')
        sys.addaudithook(fail_once)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled route replacement failure'):
                update.apply(self.runtime, self.handler)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())
        self.assertNotIn('file_info', vars(self.mixin))

    def test_busy_and_closed_after_wait_refuse_without_mutation(self):
        runtime = self.runtime
        class Lock:
            acquired = False
            released = False
            close_on_acquire = False
            def acquire(self, *, timeout):
                self.timeout = timeout
                if self.close_on_acquire:
                    runtime.closed = True
                    self.acquired = True
                return self.acquired
            def release(self):
                if not self.acquired:
                    raise AssertionError('Unowned lock released')
                self.released = True
        runtime.lock = Lock()
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'Runtime remains busy'):
            update.apply(runtime, self.handler)
        self.assertEqual(runtime.lock.timeout, 10)
        self.assertFalse(runtime.lock.released)
        self.assertEqual(before, self.state())
        runtime.lock.close_on_acquire = True
        with self.assertRaisesRegex(RuntimeError, '[Cc]losed'):
            update.apply(runtime, self.handler)
        self.assertTrue(runtime.lock.released)
        self.assertEqual(before, self.state())


if __name__ == '__main__':
    unittest.main()
