#!/usr/bin/env python3
"""The reservation cutover preserves live work and rejects unknown implementations."""
from email.message import Message
import io
import socket
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import FunctionType, ModuleType, SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_agent_management
import codex_canvas
import codex_rules
import codex_runtime
import codex_tool_requests
import codex_resource_removal_update as update


def cell(value=None):
    return (lambda: value).__closure__[0]


class ResourceRemovalUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(
            ['git', 'show', update.BASE_COMMIT + ':scripts/' + name + '.py'], cwd=ROOT, text=True)
            for name in ('codex_runtime', 'codex_rules', 'codex_canvas',
                         'codex_agent_management', 'codex_tool_requests')}

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='studio-resource-removal-')
        self.addCleanup(self.temporary.cleanup)
        self.modules = {}
        for original in (codex_runtime, codex_rules, codex_canvas, codex_agent_management, codex_tool_requests):
            module = ModuleType(original.__name__)
            vars(module).update(vars(original))
            self.modules[module.__name__] = module
        self.rules = type('RulesMixin', (), {'__module__': 'codex_rules'})
        self.owner = type('Runtime', (self.rules,), {'__module__': 'codex_runtime'})
        self.canvas_owner = type('Canvas', (), {'__module__': 'codex_canvas'})
        self.modules['codex_rules'].RulesMixin = self.rules
        self.modules['codex_runtime'].Runtime = self.owner
        self.modules['codex_runtime'].RulesMixin = self.rules
        self.modules['codex_canvas'].Canvas = self.canvas_owner
        descriptor = codex_canvas.Canvas.runtime
        self.canvas_owner.runtime = property(
            FunctionType(descriptor.fget.__code__, vars(self.modules['codex_canvas'])),
            FunctionType(descriptor.fset.__code__, vars(self.modules['codex_canvas'])))
        self.runtime = self.owner()
        self.runtime.root = Path(self.temporary.name)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'retained': object()}
        self.runtime.database = object()
        self.runtime.requests = {'active': {'stage': 'running', 'outcome': 'unknown'}}
        self.runtime.agents = {'active': {'status': 'running', 'turnId': 'retained'}}
        self.board = self.runtime.root / 'codex-board.json'
        self.board.write_text('{"claims":{"old":{"worker":"active"}},"queue":{},"notes":[]}')
        self.canvas = self.canvas_owner()
        self.canvas.runtime = self.runtime
        self.assertNotIn('runtime', vars(self.canvas))
        self.assertIs(self.canvas._runtime, self.runtime)
        self.canvas.root = self.runtime.root
        self.canvas.threads = lambda *args: [{'id': 'active'}]
        self.canvas.chats = lambda: []
        self.canvas.edges = lambda threads: []
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
        for target in update.EXPECTED:
            module, *path = target.split('.')
            if path[0] == 'make_server':
                continue
            owner = self.modules[module] if len(path) == 1 else getattr(self.modules[module], path[0])
            setattr(owner, path[-1], self.compile(module, tuple(path)))
        for name in update.RETIRED:
            setattr(self.rules, name, self.compile('codex_rules', ('RulesMixin', name)))
        self.modules['codex_runtime'].rule_tools = self.modules['codex_rules'].rule_tools
        self.modules['codex_runtime'].INSTRUCTIONS = update.source_instructions(self.old['codex_runtime'])
        tools = list(codex_runtime.TOOLS)
        resource = next(item for item in self.modules['codex_rules'].rule_tools(codex_runtime.tool, codex_runtime.TEXT)
                        if item['name'] == 'orchestration_resource')
        tools.insert(next(i for i, item in enumerate(tools) if item['name'] == 'orchestration_monitor_input'), resource)
        self.modules['codex_runtime'].TOOLS = tools
        self.assertEqual(update.digest(tools), update.TOOLS[0])
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)

    def compile(self, module, path, namespace=None, cells=None, source=None):
        namespace = vars(self.modules[module]) if namespace is None else namespace
        source = self.old[module] if source is None else source
        function, _ = update.source_function(source, path, namespace)
        known = self.cells if cells is None else cells
        closure = tuple(known[name] for name in function.__code__.co_freevars) or None
        return update.source_function(source, path, namespace, closure=closure)[0]

    def functions(self):
        result = []
        for target in update.EXPECTED:
            module, *path = target.split('.')
            owner = (self.handler if path[0] == 'make_server' else self.modules[module]
                     if len(path) == 1 else getattr(self.modules[module], path[0]))
            result.append(getattr(owner, path[-1]))
        result.extend(getattr(self.rules, name) for name in update.RETIRED if hasattr(self.rules, name))
        return result

    def state(self):
        module = self.modules['codex_runtime']
        return (tuple((function, function.__code__) for function in self.functions()),
                module.INSTRUCTIONS, update.digest(module.TOOLS),
                self.handler.do_GET.__closure__, self.handler.do_POST.__closure__,
                self.runtime.connections, self.runtime.database,
                repr(self.runtime.requests), repr(self.runtime.agents), self.board.read_bytes())

    def test_callbacks_closures_instructions_and_tools_update_without_changing_work(self):
        before = self.state()
        callbacks = self.functions()
        tools = self.modules['codex_runtime'].TOOLS
        result = update.apply(self.runtime, self.handler)
        self.assertEqual(result['status'], 'applied')
        self.assertEqual(callbacks, self.functions())
        self.assertEqual(before[3:], self.state()[3:])
        self.assertIs(tools, self.modules['codex_runtime'].TOOLS)
        self.assertEqual(update.digest(tools), update.TOOLS[1])
        self.assertNotIn('orchestration_resource', self.modules['codex_runtime'].INSTRUCTIONS)
        factory = self.modules['codex_rules'].rule_tools
        self.assertNotIn('orchestration_resource', {item['name'] for item in factory(codex_runtime.tool, codex_runtime.TEXT)})
        for name in update.RETIRED:
            result = getattr(self.runtime, name)({'action': 'claim', 'resource': 'old'})
            self.assertFalse(result['ok'])
            self.assertEqual(result['outcome'], 'not_applied')
            self.assertIn('reservations were removed', result['message'])
        snapshot = self.canvas.snapshot()
        self.assertEqual(snapshot['threads'], [{'id': 'active'}])
        self.assertNotIn('board', snapshot)
        self.assertNotIn('boardError', snapshot)
        after = self.state()
        self.assertEqual(update.apply(self.runtime, self.handler)['status'], 'already_applied')
        self.assertEqual(after, self.state())

    def test_http_get_and_post_reject_old_resource_route_without_board_access(self):
        update.apply(self.runtime, self.handler)
        request = self.handler()
        request.path = '/api/resources'
        request.trusted = lambda **kwargs: True
        request.send = lambda value, status=200: (status, value)
        request.headers = Message()
        request.headers['Content-Length'] = '2'
        request.headers['Content-Type'] = 'application/json'
        request.connection = SimpleNamespace(settimeout=lambda timeout: None)
        request.rfile = io.BytesIO(b'{}')
        before = self.state()
        with patch.object(Path, 'read_text', side_effect=AssertionError('Unexpected file read')):
            self.assertEqual(request.do_GET(), (404, {'error': 'Not found'}))
            self.assertEqual(request.do_POST(), (404, {'error': 'Not found'}))
        self.assertEqual(before, self.state())

    def test_live_file_viewer_route_baseline_retains_callbacks_and_retires_resources(self):
        source = subprocess.check_output(
            ['git', 'show', '193d962:scripts/codex_canvas.py'], cwd=ROOT, text=True)
        self.handler.do_GET = self.compile('codex_canvas', ('make_server', 'Handler', 'do_GET'), source=source)
        self.assertEqual(update.signature(self.handler.do_GET),
                         '42dab8a766ab8a53f7d3dcf4f263f3157ef89a085df1fa35adb541df8bbda88a')
        before = self.state()
        callback = self.handler.do_GET
        self.assertEqual(update.apply(self.runtime, self.handler)['status'], 'applied')
        self.assertIs(callback, self.handler.do_GET)
        self.assertEqual(before[3:], self.state()[3:])
        self.assertEqual(update.signature(callback), update.EXPECTED['codex_canvas.make_server.Handler.do_GET'][1])
        request = self.handler()
        request.path = '/api/resources'
        request.trusted = lambda **kwargs: True
        request.send = lambda value, status=200: (status, value)
        self.assertEqual(request.do_GET(), (404, {'error': 'Not found'}))
        self.assertNotIn('board', self.canvas.snapshot())
        after = self.state()
        self.assertEqual(update.apply(self.runtime, self.handler)['status'], 'already_applied')
        self.assertEqual(after, self.state())

    def test_fresh_process_without_retired_methods_is_supported(self):
        update.apply(self.runtime, self.handler)
        for name in update.RETIRED:
            delattr(self.rules, name)
        before = self.state()
        self.assertEqual(update.apply(self.runtime, self.handler)['status'], 'already_applied')
        self.assertEqual(before, self.state())

    def test_unknown_function_code_defaults_globals_and_module_reject_before_any_change(self):
        for target in update.EXPECTED:
            module, *path = target.split('.')
            owner = (self.handler if path[0] == 'make_server' else self.modules[module]
                     if len(path) == 1 else getattr(self.modules[module], path[0]))
            name = path[-1]
            original = getattr(owner, name)
            for kind in ('code', 'defaults', 'globals', 'module'):
                function = self.compile(module, tuple(path),
                    namespace=dict(vars(self.modules[module])) if kind == 'globals' else None)
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
                elif kind == 'defaults':
                    function.__defaults__ = ('unknown',)
                elif kind == 'module':
                    function.__module__ = 'unknown'
                setattr(owner, name, function)
                before = self.state()
                with self.subTest(target=target, kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    update.apply(self.runtime, self.handler)
                self.assertEqual(before, self.state())
                setattr(owner, name, original)

    def test_unknown_retired_function_or_alias_rejects_before_any_change(self):
        before = self.state()
        with patch.object(self.rules, 'resource_locked', lambda *args: None):
            changed = self.state()
            with self.assertRaisesRegex(RuntimeError, 'Unknown retired'):
                update.apply(self.runtime, self.handler)
            self.assertEqual(changed, self.state())
        with patch.object(self.modules['codex_runtime'], 'rule_tools', lambda *args: []):
            with self.assertRaisesRegex(RuntimeError, 'factory alias'):
                update.apply(self.runtime, self.handler)
            self.assertEqual(before, self.state())

    def test_unknown_instructions_tools_and_source_refuse_without_partial_update(self):
        module = self.modules['codex_runtime']
        for field, changed in (('TOOLS', module.TOOLS + [{'name': 'unknown'}]),
                               ('INSTRUCTIONS', module.INSTRUCTIONS + '\nUnknown')):
            with patch.object(module, field, changed):
                before = self.state()
                with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
                    update.apply(self.runtime, self.handler)
                self.assertEqual(before, self.state())
        before = self.state()
        name = next(iter(update.EXPECTED))
        with patch.dict(update.EXPECTED, {name: (update.EXPECTED[name][0], '0' * 64)}):
            with self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
                update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())

    def test_foreign_closure_runtime_location_and_instance_override_refuse(self):
        original = self.handler.do_POST
        self.handler.do_POST = self.compile('codex_canvas', ('make_server', 'Handler', 'do_POST'),
            cells={key: cell(value.cell_contents) for key, value in self.cells.items()})
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'closure cells'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())
        self.handler.do_POST = original
        with patch.object(self.canvas, '_runtime', object()), self.assertRaisesRegex(RuntimeError, 'another runtime'):
            update.apply(self.runtime, self.handler)
        with patch.object(self.runtime, 'dynamic', lambda *args: None), self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime, self.handler)
        with patch.object(self.modules['codex_rules'], '__file__', '/unknown/codex_rules.py'), self.assertRaisesRegex(RuntimeError, 'location'):
            update.apply(self.runtime, self.handler)

    def test_assignment_failure_rolls_back_all_previous_code_changes(self):
        before = self.state()
        target = self.handler.do_POST
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled assignment failure')
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled assignment'):
                update.apply(self.runtime, self.handler)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())
        self.assertTrue(self.runtime.lock.acquire(blocking=False))
        self.runtime.lock.release()

    def test_manager_finds_only_the_open_server_for_this_runtime(self):
        server_type = type('LocalServer', (update.ThreadingHTTPServer,), {
            '__module__': 'codex_canvas', '__qualname__': 'make_server.<locals>.LocalServer'})
        server = server_type.__new__(server_type)
        server.RequestHandlerClass = self.handler
        self.assertIs(update._canvas_runtime(self.canvas), self.runtime)
        listener = socket.socket()
        self.addCleanup(listener.close)
        server.socket = listener
        before = self.state()
        with patch.object(update.gc, 'get_objects', return_value=[]):
            with self.assertRaisesRegex(RuntimeError, 'Expected one'):
                update.apply(self.runtime)
        self.assertEqual(before, self.state())
        with patch.object(self.canvas_owner, 'runtime', property(lambda self: self._runtime)):
            with patch.object(update.gc, 'get_objects', return_value=[server]):
                with self.assertRaisesRegex(RuntimeError, 'runtime property'):
                    update.apply(self.runtime)
        self.assertEqual(before, self.state())
        other = server_type.__new__(server_type)
        other.RequestHandlerClass, other.socket = self.handler, listener
        with patch.object(update.gc, 'get_objects', return_value=[server, other]):
            with self.assertRaisesRegex(RuntimeError, 'Expected one'):
                update.apply(self.runtime)
        self.assertEqual(before, self.state())
        with patch.object(update.gc, 'get_objects', return_value=[server]):
            self.assertEqual(update.apply(self.runtime)['status'], 'applied')
            self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
            listener.close()
            with self.assertRaisesRegex(RuntimeError, 'Expected one'):
                update.apply(self.runtime)

    def test_busy_lock_and_closed_after_wait_do_not_change_state(self):
        runtime = self.runtime
        class Lock:
            close_on_acquire = False
            released = False
            def acquire(self, *, timeout):
                self.timeout = timeout
                if self.close_on_acquire:
                    runtime.closed = True
                    return True
                return False
            def release(self):
                self.released = True
        runtime.lock = Lock()
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'busy'):
            update.apply(runtime, self.handler)
        self.assertEqual(runtime.lock.timeout, 10)
        self.assertFalse(runtime.lock.released)
        runtime.lock.close_on_acquire = True
        with self.assertRaisesRegex(RuntimeError, 'closed'):
            update.apply(runtime, self.handler)
        self.assertTrue(runtime.lock.released)
        self.assertEqual(before, self.state())

    def test_closed_runtime_and_unsupported_python_refuse(self):
        before = self.state()
        self.runtime.closed = True
        with self.assertRaisesRegex(RuntimeError, 'closed'):
            update.apply(self.runtime, self.handler)
        self.runtime.closed = False
        with patch.object(sys, 'version_info', (3, 13)), self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            update.apply(self.runtime, self.handler)
        self.assertEqual(before, self.state())


if __name__ == '__main__':
    unittest.main()
