#!/usr/bin/env python3
"""The progress cutover preserves live callbacks, state, and HTTP closure cells."""
import copy
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
import codex_progress_update as update
import codex_runtime
import codex_efficiency
import codex_panel
import codex_canvas


def cell(value=None):
    return (lambda: value).__closure__[0]


class InstallMeta(type):
    fail_install = False

    def __setattr__(cls, name, value):
        if name == 'progress_file' and cls.fail_install:
            type.__setattr__(cls, 'fail_install', False)
            raise RuntimeError('Controlled descriptor failure')
        super().__setattr__(name, value)


class ProgressUpdate(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {module: subprocess.check_output(
            ['git', 'show', update._BASE_COMMIT + ':scripts/' + module + '.py'],
            cwd=ROOT, text=True) for module, _ in update._TARGETS}

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='studio-progress-update-')
        self.modules = {}
        for name in self.old:
            module = ModuleType(name)
            module.__dict__.update(vars(sys.modules[name]))
            self.modules[name] = module
        self.efficiency = type('EfficiencyMixin', (), {})
        self.panel = type('PanelMixin', (), {})
        self.owner = InstallMeta('Runtime', (self.efficiency, self.panel), {})
        for module, owner in [('codex_runtime', self.owner), ('codex_efficiency', self.efficiency), ('codex_panel', self.panel)]:
            owner.__module__ = module
            setattr(self.modules[module], owner.__name__, owner)
        self.runtime = self.owner()
        self.runtime.root = Path(self.temporary.name)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'one': object(), 'two': object()}
        self.runtime.state = object()
        self.handler = type('Handler', (), {'__module__': 'codex_canvas',
                           '__qualname__': 'make_server.<locals>.Handler'})
        self.cells = {name: cell(value) for name, value in {
            'canvas': SimpleNamespace(runtime=self.runtime),
            'cost_reader': [None], 'remote': object(), 'snapshot': None,
            'sync': None, 'terminal_lock': threading.RLock(), 'terminals': None,
            'token': 'fixture-token', 'sync_store': [None], 'terminal_manager': [None],
        }.items()}
        for name in ('snapshot', 'sync', 'terminals'):
            function = self.compile('codex_canvas', ('make_server', name))
            self.cells[name].cell_contents = function
        for name in ('do_POST', 'trusted', 'stream_sync'):
            setattr(self.handler, name, self.compile('codex_canvas', ('make_server', 'Handler', name)))
        self.functions = {}
        for module, path in update._TARGETS:
            name = path[-1]
            if name in update._NEW_METHODS:
                continue
            function = self.compile(module, path)
            self.assertEqual(update.signature(function), update._EXPECTED[name]['old'])
            self.functions[name] = function
            owner = (self.handler if name == 'do_GET' else
                     getattr(self.modules[module], path[0]) if len(path) > 1 else self.modules[module])
            setattr(owner, name, staticmethod(function) if name == 'turn_permissions' else function)
        runtime_module = self.modules['codex_runtime']
        runtime_module.INSTRUCTIONS = update.source_instructions(self.old['codex_runtime'])
        runtime_module.panel_tools = self.functions['panel_tools']
        runtime_module.TOOLS = copy.deepcopy(codex_runtime.TOOLS)
        legacy = self.functions['panel_tools'](codex_runtime.tool, codex_runtime.TEXT)
        index = next(i for i, item in enumerate(runtime_module.TOOLS)
                     if item['name'] == codex_runtime.request_tools(codex_runtime.tool, codex_runtime.TEXT)[0]['name'])
        runtime_module.TOOLS[index:index] = legacy
        self.assertEqual(update.digest(runtime_module.TOOLS), update._TOOLS['old'])
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()

    def compile(self, module, path):
        source = self.old[module]
        function, _ = update.source_function(source, path, vars(self.modules[module]))
        closure = tuple(self.cells[name] for name in function.__code__.co_freevars) or None
        return update.source_function(source, path, vars(self.modules[module]), closure=closure)[0]

    def tearDown(self):
        self.module_patch.stop()
        self.temporary.cleanup()

    def state(self):
        return {
            'codes': {name: function.__code__ for name, function in self.functions.items()},
            'runtime': dict(vars(self.owner)), 'panel': dict(vars(self.panel)),
            'tools': copy.deepcopy(self.modules['codex_runtime'].TOOLS),
            'instructions': self.modules['codex_runtime'].INSTRUCTIONS,
            'cells': tuple(self.handler.do_GET.__closure__),
        }

    def test_cutover_preserves_callback_and_closure_identity_without_state_io(self):
        captured = self.runtime.dynamic
        captured_http = self.handler.do_GET
        captured_context = self.runtime.model_context
        imported_tools = self.modules['codex_runtime'].panel_tools
        tools = self.modules['codex_runtime'].TOOLS
        state, connections = self.runtime.state, self.runtime.connections
        cells = captured_http.__closure__
        contents = tuple(item.cell_contents for item in cells)
        receipt = update.apply(self.runtime, self.handler)
        self.assertEqual(receipt['status'], 'applied')
        self.assertIs(captured.__func__, self.runtime.dynamic.__func__)
        self.assertIs(captured_context.__func__, self.runtime.model_context.__func__)
        self.assertIs(captured_http, self.handler.do_GET)
        self.assertIs(captured_http.__closure__, cells)
        self.assertTrue(all(before is after.cell_contents for before, after in zip(contents, cells)))
        self.assertIs(self.runtime.state, state)
        self.assertIs(self.runtime.connections, connections)
        self.assertIs(self.modules['codex_runtime'].TOOLS, tools)
        self.assertIs(imported_tools, self.modules['codex_panel'].panel_tools)
        self.assertEqual(imported_tools(lambda *args: self.fail('Retired tool schema evaluated'), {}), [])
        for module, path in update._TARGETS:
            owner = self.handler if path[-1] == 'do_GET' else getattr(self.modules[module], path[0]) if len(path) > 1 else self.modules[module]
            self.assertEqual(update.signature(getattr(owner, path[-1])), update._EXPECTED[path[-1]]['new'])
        self.assertEqual(list(self.runtime.root.iterdir()), [])
        saved = self.state()
        self.assertEqual(update.apply(self.runtime, self.handler)['status'], 'already_applied')
        self.assertEqual(saved, self.state())

    def test_static_to_instance_preserves_readonly_and_agent_scope(self):
        update.apply(self.runtime, self.handler)
        reviewer = {'id': 'reviewer', 'cwd': '/workspace', 'role': 'reviewer', 'yoloMode': False}
        with patch('codex_progress.provision_progress', side_effect=AssertionError('Read-only must not gain a root')):
            self.assertEqual(self.runtime.turn_permissions(reviewer),
                             {'approvalPolicy': 'on-request', 'sandboxPolicy': {'type': 'readOnly'}})
        for identity in ('first', 'second'):
            worker = {**reviewer, 'id': identity, 'role': 'worker'}
            policy = self.runtime.turn_permissions(worker)
            self.assertEqual(policy['sandboxPolicy'], {'type': 'workspaceWrite', 'networkAccess': False,
                             'writableRoots': ['/workspace', str(self.runtime.root / 'progress' / identity)]})
        with patch('codex_progress.provision_progress', side_effect=PermissionError('Read-only disk')):
            self.assertEqual(self.runtime.turn_permissions(worker)['sandboxPolicy']['writableRoots'], ['/workspace'])
        self.assertEqual(self.runtime.turn_permissions({**reviewer, 'yoloMode': True})['sandboxPolicy'],
                         {'type': 'dangerFullAccess'})
        self.assertEqual(self.runtime.turn_permissions({**reviewer, 'yoloMode': None}), {})

    def test_captured_http_keeps_origin_gate_and_uses_new_reader(self):
        callback = self.handler.do_GET
        update.apply(self.runtime, self.handler)
        request = self.handler()
        request.path = '/api/panel?agent=first'
        request.headers = {}
        request.client_address = ('127.0.0.1', 1)
        request.server = SimpleNamespace(server_port=1)
        request.send = lambda value, status=200: (status, value)
        request.trusted = lambda: False
        self.assertEqual(callback(request), (403, {'error': 'Local origin required'}))
        request.trusted = lambda: True
        self.runtime.get_panel = lambda agent: {'markdown': 'New path', 'agent': agent}
        self.assertEqual(callback(request), (200, {'markdown': 'New path', 'agent': 'first'}))

    def test_unknown_live_function_defaults_and_globals_refuse(self):
        function = self.functions['dynamic']
        function.__defaults__ = ('different-account', None)
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'Unknown live progress implementation'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_unknown_live_function_namespace_refuses(self):
        function, _ = update.source_function(self.old['codex_runtime'], ('Runtime', 'dynamic'),
                                            dict(vars(self.modules['codex_runtime'])))
        self.owner.dynamic = function
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'Unknown live progress function namespace'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_unknown_tools_refuse_without_removing_any_definition(self):
        self.modules['codex_runtime'].TOOLS.append({'name': 'unreviewed'})
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'Unknown live progress tool definitions'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_other_runtime_closure_refuses(self):
        self.cells['canvas'].cell_contents.runtime = object()
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'another runtime'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_replaced_closure_cell_refuses(self):
        self.handler.trusted = update.source_function(self.old['codex_canvas'],
            ('make_server', 'Handler', 'trusted'), vars(self.modules['codex_canvas']),
            closure=(cell(self.cells['remote'].cell_contents), self.cells['token']))[0]
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'closure cells do not match'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_unreviewed_new_source_refuses_before_mutation(self):
        read_text = Path.read_text
        def changed(path, *args, **kwargs):
            source = read_text(path, *args, **kwargs)
            return source.replace('return read_progress(self.root, identity)', 'return {}') if path.name == 'codex_panel.py' else source
        before = self.state()
        with patch.object(Path, 'read_text', changed), self.assertRaisesRegex(RuntimeError, 'Unreviewed progress replacement'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_failed_descriptor_install_rolls_back_static_method_and_new_methods(self):
        before = self.state()
        self.owner.fail_install = True
        with self.assertRaisesRegex(RuntimeError, 'Controlled descriptor failure'):
            update.apply(self.runtime, self.handler)
        del self.owner.fail_install
        self.assertEqual(before, self.state())
        self.assertIsInstance(vars(self.owner)['turn_permissions'], staticmethod)

    def test_mid_code_failure_restores_codes_tools_instructions_and_descriptors(self):
        before = self.state()
        armed = [True]
        target = self.functions['model_context']
        def fail_once(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled code replacement failure')
        sys.addaudithook(fail_once)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled code replacement failure'):
                update.apply(self.runtime, self.handler)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())

    def test_stale_loaded_progress_helper_refuses_before_mutation(self):
        import codex_progress
        before = self.state()
        with patch.object(codex_progress, 'read_progress', lambda root, agent: {}):
            with self.assertRaisesRegex(RuntimeError, 'Unknown live progress function namespace'):
                update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_progress_helper_disk_change_refuses_before_mutation(self):
        read_bytes = Path.read_bytes
        def changed(path, *args, **kwargs):
            content = read_bytes(path, *args, **kwargs)
            return content + b'\n# unreviewed\n' if path.name == 'codex_progress.py' else content
        before = self.state()
        with patch.object(Path, 'read_bytes', changed), self.assertRaisesRegex(RuntimeError, 'Unreviewed progress file helper source'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_compile_does_not_execute_module_decorators_or_defaults(self):
        function, static = update.source_function('class Test:\n @staticmethod\n def value(a=None):\n  return a\nraise RuntimeError("Do not execute")\n', ('Test', 'value'), {})
        self.assertTrue(static)
        self.assertEqual(function('value'), 'value')
        with self.assertRaises(ValueError):
            update.source_function('def value(a=__import__("os").getpid()): pass', ('value',), {})

    def test_busy_runtime_refuses_without_mutation(self):
        class Busy:
            def acquire(self, *, timeout):
                self.timeout = timeout
                return False
            def release(self):
                raise AssertionError('Unowned lock released')
        self.runtime.lock = Busy()
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'Runtime remains busy'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(self.runtime.lock.timeout, 10)
        self.assertEqual(before, self.state())


if __name__ == '__main__':
    unittest.main()
