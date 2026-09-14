#!/usr/bin/env python3
"""Guard exact baseline code, callback identity, active frames and rollback."""
import ast
import copy
import importlib
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
import codex_limit_fixes_update as update


class LimitFixesUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(
            ['git', 'show', update.BASE_COMMIT + ':scripts/' + name + '.py'], cwd=ROOT, text=True)
            for name in update.SOURCE_SHA if name not in update.NEW_MODULES}
        # The reviewed fixture stores only changed tool definitions. Reconstruct
        # the previous list from literal baseline definitions in the manifest.
        cls.old_tools = copy.deepcopy(codex_runtime.TOOLS)
        for index, row in enumerate(cls.old_tools):
            if row['name'] in OLD_TOOL_CHANGES:
                cls.old_tools[index] = copy.deepcopy(OLD_TOOL_CHANGES[row['name']])
        if update.digest(cls.old_tools) != update.TOOLS_DIGEST[0]:
            raise AssertionError('Fixture does not reconstruct the reviewed baseline tools')

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
        mixins = [owner for name, owner in self.owners.items()
                  if name not in {'codex_native_voice.NativeVoice', 'codex_account_transfer.AccountTransfers'}]
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
        for target, allowed in update.EXPECTED.items():
            owner, method = self.target(target)
            if allowed[0] is None:
                vars(owner).pop(method, None) if isinstance(owner, ModuleType) else None
                continue
            module, *path = target.split('.')
            function, static = self.compile(module, tuple(path))
            setattr(owner, method, staticmethod(function) if static else function)
        for module, values in update.GLOBAL_IMPORTS.items():
            for key in values:
                vars(self.modules[module]).pop(key, None)
        for target, (old, _) in update.CONSTANTS.items():
            name, key = target.split('.')
            setattr(self.modules[name], key, copy.deepcopy(old))
        self.modules['codex_runtime'].efficiency_tools = self.modules['codex_efficiency'].efficiency_tools
        self.modules['codex_runtime'].TOOLS = copy.deepcopy(self.old_tools)
        self.runtime = self.owner()
        self.runtime.lock, self.runtime.closed = threading.RLock(), False
        self.canvas.runtime = self.runtime
        self.runtime.connections = {'active': object()}
        self.runtime.requests = {'unknown': {'stage': 'running'}}
        self.runtime.streams = {'native': object()}
        self.runtime.inflight = {'accepted': object()}
        self.transfers = self.owners['codex_account_transfer.AccountTransfers']()
        self.transfers.rt, self.transfers.futures = self.runtime, {'pending': object()}
        self.runtime._account_transfers = self.transfers
        voice_module = ModuleType('codex_voice')
        voice_module.__file__ = str(ROOT / 'scripts' / 'codex_voice.py')
        voice_module.NativeVoice = self.owners['codex_native_voice.NativeVoice']
        voice_module.VoiceStore = type('VoiceStore', (voice_module.NativeVoice,), {'__module__': 'codex_voice'})
        self.modules['codex_voice'] = voice_module
        self.voice = voice_module.VoiceStore()
        self.voice.runtime, self.voice.connections = self.runtime, {'stream': object()}
        self.voice.native_lock = threading.RLock()
        self.runtime._voice_store = self.voice
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        self.addCleanup(self.module_patch.stop)
        for name in update.NEW_MODULES:
            sys.modules.pop(name, None)

    def target(self, target):
        name, *path = target.split('.')
        return (self.handler if len(path) == 3 else self.modules[name] if len(path) == 1
                else self.owners[name + '.' + path[0]], path[-1])

    def compile(self, module, path, namespace=None):
        namespace = vars(self.modules[module]) if namespace is None else namespace
        function, _ = update.source_function(self.old[module], path, namespace)
        closure = tuple(self.cells[name] for name in function.__code__.co_freevars) or None
        return update.source_function(self.old[module], path, namespace, closure=closure)

    def apply(self):
        return update.apply(self.runtime, self.handler)

    def functions(self):
        return [getattr(*self.target(key), update.MISSING) for key in update.EXPECTED]

    def state(self):
        return (tuple((f, f.__code__, f.__defaults__, f.__kwdefaults__) if isinstance(f, FunctionType)
                      else f for f in self.functions()),
                tuple((name, sys.modules.get(name)) for name in update.NEW_MODULES),
                tuple((name, key, vars(self.modules[name]).get(key, update.MISSING))
                      for name, values in update.GLOBAL_IMPORTS.items() for key in values),
                self.modules['codex_runtime'].TOOLS, repr(self.modules['codex_runtime'].TOOLS),
                tuple(map(id, self.modules['codex_runtime'].TOOLS)),
                self.runtime.connections, repr(self.runtime.requests), self.runtime.streams,
                self.runtime.inflight, self.handler.do_GET.__closure__, self.handler.do_POST.__closure__,
                self.runtime._account_transfers, self.transfers.futures, repr(self.transfers.futures),
                self.runtime._voice_store, self.voice.connections, repr(self.voice.connections), self.voice.native_lock,
                tuple((getattr(self.modules[name], key), frozenset(getattr(self.modules[name], key)))
                      for name, key in (target.split('.') for target in update.CONSTANTS)))

    def test_manifest_covers_all_changed_functions_and_imports(self):
        def functions(tree):
            result = {}
            for node in tree.body:
                if isinstance(node, ast.FunctionDef):
                    if node.name == 'make_server':
                        for owner in node.body:
                            if isinstance(owner, ast.ClassDef) and owner.name == 'Handler':
                                for function in owner.body:
                                    if isinstance(function, ast.FunctionDef):
                                        result[('make_server', 'Handler', function.name)] = function
                    else:
                        result[(node.name,)] = node
                elif isinstance(node, ast.ClassDef):
                    for function in node.body:
                        if isinstance(function, ast.FunctionDef):
                            result[(node.name, function.name)] = function
            return result
        def surrounding(tree, module):
            nodes = []
            for original in tree.body:
                node = copy.deepcopy(original)
                if isinstance(node, (ast.Import, ast.ImportFrom)):
                    continue
                if isinstance(node, ast.FunctionDef):
                    if node.name != 'make_server':
                        continue
                    for owner in node.body:
                        if isinstance(owner, ast.ClassDef) and owner.name == 'Handler':
                            owner.body = [entry for entry in owner.body if not isinstance(entry, ast.FunctionDef)]
                elif isinstance(node, ast.ClassDef):
                    node.body = [entry for entry in node.body if not isinstance(entry, ast.FunctionDef)]
                elif (isinstance(node, ast.Assign) and any(isinstance(target, ast.Name)
                      and module + '.' + target.id in update.CONSTANTS for target in node.targets)):
                    continue
                elif (module == 'codex_runtime' and isinstance(node, ast.Assign)
                      and any(isinstance(target, ast.Name) and target.id == 'TOOLS' for target in node.targets)):
                    continue
                nodes.append(ast.dump(node))
            return nodes
        changed = set()
        for name, previous_source in self.old.items():
            current_source = (ROOT / 'scripts' / (name + '.py')).read_bytes()
            previous, current = functions(ast.parse(previous_source)), functions(ast.parse(current_source))
            self.assertEqual(surrounding(ast.parse(previous_source), name), surrounding(ast.parse(current_source), name),
                             'A global, class layout or HTTP constructor change needs an explicit live transition: ' + name)
            self.assertFalse(previous.keys() - current.keys(), 'Removed functions need an explicit live transition')
            for path, node in current.items():
                if path not in previous or ast.dump(node) != ast.dump(previous[path]):
                    changed.add(name + '.' + '.'.join(path))
            prior_imports = {ast.dump(node) for node in ast.parse(previous_source).body
                             if isinstance(node, (ast.Import, ast.ImportFrom))}
            for node in ast.parse(current_source).body:
                if isinstance(node, (ast.Import, ast.ImportFrom)) and ast.dump(node) not in prior_imports:
                    for value in node.names:
                        self.assertIn(value.asname or value.name, update.GLOBAL_IMPORTS.get(name, {}))
        self.assertEqual(changed, set(update.EXPECTED))
        for target, allowed in update.CONSTANTS.items():
            name, key = target.split('.')
            for source, expected in zip((self.old[name], (ROOT / 'scripts' / (name + '.py')).read_bytes()), allowed):
                node = next(node for node in ast.parse(source).body if isinstance(node, ast.Assign)
                            and any(isinstance(value, ast.Name) and value.id == key for value in node.targets))
                self.assertEqual(ast.literal_eval(node.value), expected)

    def test_preserves_callbacks_pending_requests_streams_and_inflight(self):
        before = self.state()
        functions = self.functions()
        tools = self.modules['codex_runtime'].TOOLS
        self.assertEqual(self.apply()['status'], 'applied')
        self.assertIs(tools, self.modules['codex_runtime'].TOOLS)
        for target, previous, current in zip(update.EXPECTED, functions, self.functions()):
            if previous is not update.MISSING:
                self.assertIs(previous, current)
            self.assertEqual(update.signature(current), update.EXPECTED[target][1])
        self.assertEqual(before[6:-1], self.state()[6:-1])
        for (previous, _), (current, value) in zip(before[-1], self.state()[-1]):
            self.assertIs(previous, current)
        for target, (_, desired) in update.CONSTANTS.items():
            name, key = target.split('.')
            self.assertEqual(getattr(self.modules[name], key), desired)
        after = self.state()
        self.assertEqual(self.apply()['status'], 'already_applied')
        self.assertEqual(after, self.state())

    def test_unknown_code_defaults_globals_module_and_new_method_reject(self):
        for target, allowed in update.EXPECTED.items():
            owner, method = self.target(target)
            original = vars(owner).get(method, update.MISSING)
            name, *path = target.split('.')
            if allowed[0] is None:
                setattr(owner, method, lambda: None)
                before = self.state()
                with self.subTest(target=target), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    self.apply()
                self.assertEqual(before, self.state())
                delattr(owner, method)
                continue
            for kind in ('code', 'defaults', 'globals', 'module'):
                namespace = vars(self.modules[name])
                function, static = self.compile(name, tuple(path), dict(namespace) if kind == 'globals' else namespace)
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
                elif kind == 'defaults': function.__defaults__ = ('unknown',)
                elif kind == 'module': function.__module__ = 'unknown'
                setattr(owner, method, staticmethod(function) if static else function)
                if target == 'codex_efficiency.efficiency_tools':
                    self.modules['codex_runtime'].efficiency_tools = function
                before = self.state()
                with self.subTest(target=target, kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    self.apply()
                self.assertEqual(before, self.state())
            setattr(owner, method, original)
            if target == 'codex_efficiency.efficiency_tools':
                self.modules['codex_runtime'].efficiency_tools = original

    def test_source_global_helper_origin_and_override_reject(self):
        before = self.state()
        with patch.dict(update.SOURCE_SHA, {'codex_runtime': '0' * 64}), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
            self.apply()
        with patch.object(self.runtime, 'native_action', lambda *args: None, create=True), self.assertRaisesRegex(RuntimeError, 'override'):
            self.apply()
        with patch.object(self.modules['codex_work'], '__file__', '/unknown'), self.assertRaisesRegex(RuntimeError, 'location'):
            self.apply()
        with patch.object(self.modules['codex_runtime'], 'efficiency_tools', lambda: None), self.assertRaisesRegex(RuntimeError, 'alias'):
            self.apply()
        with patch.object(self.modules['codex_efficiency'], 'EfficiencyMixin',
                          type('EfficiencyMixin', (), {'__module__': 'codex_efficiency'})), self.assertRaisesRegex(RuntimeError, 'owner'):
            self.apply()
        with patch.object(self.transfers, 'local_blocker', lambda *args: None, create=True), self.assertRaisesRegex(RuntimeError, 'override'):
            self.apply()
        with patch.object(self.voice, 'start', lambda *args: None, create=True), self.assertRaisesRegex(RuntimeError, 'override'):
            self.apply()
        with patch.object(self.voice, 'runtime', object()), self.assertRaisesRegex(RuntimeError, 'voice store'):
            self.apply()
        with patch.object(self.transfers, 'rt', object()), self.assertRaisesRegex(RuntimeError, 'transfer store'):
            self.apply()
        for target in update.CONSTANTS:
            name, key = target.split('.')
            live = getattr(self.modules[name], key)
            live.add('unknown')
            with self.assertRaisesRegex(RuntimeError, 'constant'):
                self.apply()
            live.remove('unknown')
        tools = self.modules['codex_runtime'].TOOLS
        with patch.object(self.modules['codex_runtime'], 'TOOLS', tools + [{'name': 'unknown'}]), self.assertRaisesRegex(RuntimeError, 'tool definitions'):
            self.apply()
        self.assertEqual(before, self.state())
        self.apply()
        for name in update.NEW_MODULES:
            helper = sys.modules[name]
            helper.unknown = True
            before = self.state()
            with self.assertRaisesRegex(RuntimeError, 'helper globals'):
                self.apply()
            self.assertEqual(before, self.state())
            del helper.unknown

    def test_unloaded_optional_helpers_register_without_runtime_operations(self):
        for name in update.OPTIONAL_MODULES:
            sys.modules.pop(name, None)
        sys.modules.pop('codex_voice', None)
        del self.runtime._voice_store
        def forbidden(*args, **kwargs):
            raise AssertionError('The patch must not execute a runtime operation')
        self.runtime.db = forbidden
        self.runtime.pool = type('Pool', (), {'submit': forbidden})()
        self.assertEqual(self.apply()['status'], 'applied')
        for name in update.OPTIONAL_MODULES:
            module = sys.modules[name]
            for target, allowed in update.EXPECTED.items():
                if target.startswith(name + '.'):
                    value = module
                    for part in target.split('.')[1:]:
                        value = getattr(value, part)
                    self.assertEqual(update.signature(value), allowed[1])
        self.assertEqual(self.apply()['status'], 'already_applied')

    def test_closed_runtime_version_and_http_closure_reject(self):
        before = self.state()
        with patch.object(self.runtime, 'closed', True), self.assertRaisesRegex(RuntimeError, 'closed'):
            self.apply()
        with patch.object(sys, 'version_info', (3, 13)), self.assertRaisesRegex(RuntimeError, 'Unknown runtime'):
            self.apply()
        with patch.object(self.canvas, '_runtime', object()), self.assertRaisesRegex(RuntimeError, 'another runtime'):
            self.apply()
        self.assertEqual(before, self.state())

    def test_partial_assignment_rolls_back_exact_baseline(self):
        target = next(f for f in reversed(self.functions()) if isinstance(f, FunctionType))
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

    def test_active_old_frame_rejects_then_future_call_applies(self):
        entered, release = threading.Event(), threading.Event()
        errors = []
        class PendingText(str):
            def strip(self, *args):
                entered.set()
                if not release.wait(10): raise AssertionError('Fixture did not release')
                raise ValueError('Old call settled')
        def call():
            try: self.runtime.chat_message('lead', 'worker', PendingText('message'), 'request')
            except ValueError as error: errors.append(str(error))
        thread = threading.Thread(target=call)
        thread.start()
        try:
            self.assertTrue(entered.wait(5))
            before = self.state()
            with self.assertRaisesRegex(RuntimeError, 'earlier call.*chat_message'):
                self.apply()
            self.assertEqual(before, self.state())
        finally:
            release.set()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(errors, ['Old call settled'])
        self.assertEqual(self.apply()['status'], 'applied')


# BEGIN BASELINE TOOLS
OLD_TOOL_CHANGES = {'orchestration_message': {'description': 'Share a finding, question, or answer with other agents during '
                                          'work. target is an agent id in your team, parent, lead, or '
                                          'broadcast (your team). Broadcasts notify only active agents; '
                                          'other recipients can read them in chat history. Private chats are '
                                          'visible to their participants and the user. Direct messages wake '
                                          'idle recipients but never resume stopped agents. Use '
                                          'importance=progress only for routine updates; these batch briefly '
                                          'and keep the latest progress per sender, room and progress_key '
                                          'when progress_version increases. Use the task id as progress_key. '
                                          'Without these fields, every update is retained. Original messages '
                                          'remain in chat history. Questions and blockers deliver '
                                          'immediately. Send when you have new information or an answer for '
                                          'the recipient.',
                           'inputSchema': {'additionalProperties': False,
                                           'properties': {'importance': {'enum': ['message',
                                                                                  'progress',
                                                                                  'question',
                                                                                  'blocker',
                                                                                  'result'],
                                                                         'type': 'string'},
                                                          'progress_key': {'type': 'string'},
                                                          'progress_version': {'minimum': 0,
                                                                               'type': 'integer'},
                                                          'target': {'type': 'string'},
                                                          'text': {'type': 'string'}},
                                           'required': ['target', 'text'],
                                           'type': 'object'},
                           'name': 'orchestration_message',
                           'type': 'function'},
 'orchestration_read': {'description': 'Read a full saved tool response by output_ref. Offsets are Unicode '
                                       'characters; pages are bounded. Use contains to locate relevant '
                                       'output. Same-agent records only. Never rerun a mutation to recover '
                                       'its result.',
                        'inputSchema': {'additionalProperties': False,
                                        'properties': {'contains': {'type': 'string'},
                                                       'offset': {'minimum': 0, 'type': 'integer'},
                                                       'output_ref': {'type': 'string'}},
                                        'required': ['output_ref'],
                                        'type': 'object'},
                        'name': 'orchestration_read',
                        'type': 'function'}}
# END BASELINE TOOLS

if __name__ == '__main__':
    unittest.main()
