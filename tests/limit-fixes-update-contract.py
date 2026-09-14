#!/usr/bin/env python3
"""Guard exact baseline code, callback identity, active frames and rollback."""
import ast
import copy
import hashlib
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
        cls.prior_sources = {commit: {name: subprocess.check_output(
            ['git', 'show', commit + ':scripts/' + name + '.py'], cwd=ROOT)
            for name in cls.old} for commit in ('7415ada', 'f496684', 'a50ac70', '7f77579', '94b72e2', '116ed9c')}
        cls.helper_old = {}
        cls.helper_versions = {}
        for name, versions in update.HELPER_BASELINES.items():
            cls.helper_versions[name] = {}
            for commit, expected in versions.items():
                raw = subprocess.check_output(['git', 'show', commit + ':scripts/' + name + '.py'], cwd=ROOT)
                if hashlib.sha256(raw).hexdigest() != expected:
                    raise AssertionError('Unknown previous helper source: ' + name)
                cls.helper_versions[name][commit] = raw
            cls.helper_old[name] = next(iter(cls.helper_versions[name].values()))
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
        for target, allowed in update.CONSTANTS.items():
            name, key = target.split('.')
            setattr(self.modules[name], key, copy.deepcopy(allowed[0]))
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
            for source, expected in zip((self.old[name], (ROOT / 'scripts' / (name + '.py')).read_bytes()), (allowed[0], allowed[-1])):
                node = next(node for node in ast.parse(source).body if isinstance(node, ast.Assign)
                            and any(isinstance(value, ast.Name) and value.id == key for value in node.targets))
                self.assertEqual(ast.literal_eval(node.value), expected)
            for sources in self.prior_sources.values():
                node = next(node for node in ast.parse(sources[name]).body if isinstance(node, ast.Assign)
                            and any(isinstance(value, ast.Name) and value.id == key for value in node.targets))
                self.assertIn(ast.literal_eval(node.value), allowed)

    def test_preserves_callbacks_pending_requests_streams_and_inflight(self):
        before = self.state()
        functions = self.functions()
        tools = self.modules['codex_runtime'].TOOLS
        self.assertEqual(self.apply()['status'], 'applied')
        self.assertIs(tools, self.modules['codex_runtime'].TOOLS)
        for target, previous, current in zip(update.EXPECTED, functions, self.functions()):
            if previous is not update.MISSING:
                self.assertIs(previous, current)
            self.assertEqual(update.signature(current), update.EXPECTED[target][-1])
        self.assertEqual(before[6:-1], self.state()[6:-1])
        for (previous, _), (current, value) in zip(before[-1], self.state()[-1]):
            self.assertIs(previous, current)
        for target, allowed in update.CONSTANTS.items():
            name, key = target.split('.')
            self.assertEqual(getattr(self.modules[name], key), allowed[-1])
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
        for name, values in update.GLOBAL_IMPORTS.items():
            for key in values:
                with patch.object(self.modules[name], key, object(), create=True):
                    with self.subTest(module=name, key=key), self.assertRaisesRegex(RuntimeError, 'Unknown limit-fix import'):
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

    def test_reviewed_constant_versions_restore_after_late_failure(self):
        self.apply()
        for index in range(max(map(len, update.CONSTANTS.values())) - 1):
            for target, allowed in update.CONSTANTS.items():
                name, key = target.split('.')
                live = getattr(self.modules[name], key)
                previous = allowed[min(index, len(allowed) - 2)]
                live.symmetric_difference_update(live ^ previous)
            before = self.state()
            with patch.object(update, '_active_frames', side_effect=[None, RuntimeError('Controlled constant rollback')]):
                with self.assertRaisesRegex(RuntimeError, 'Controlled constant rollback'):
                    self.apply()
            self.assertEqual(before, self.state())
            self.assertEqual(self.apply()['status'], 'applied')
            after = self.state()
            self.assertEqual(self.apply()['status'], 'already_applied')
            self.assertEqual(after, self.state())
            for target, allowed in update.CONSTANTS.items():
                name, key = target.split('.')
                self.assertEqual(getattr(self.modules[name], key), allowed[-1])

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
                    self.assertEqual(update.signature(value), allowed[-1])
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

    def legacy_context_helper(self, commit='7415ada'):
        self.apply()
        name = 'codex_context_repair'
        helper = update._stage_module(name, self.helper_versions[name][commit], ROOT / 'scripts')
        sys.modules[name] = helper
        return helper

    def helper_state(self, helper):
        return tuple((name, value, value.__code__, value.__defaults__, value.__kwdefaults__)
                     for name, value in vars(helper).items()
                     if isinstance(value, FunctionType) and value.__module__ == helper.__name__)

    def test_helper_upgrade_manifest_covers_exact_previous_source_delta(self):
        changed = set()
        for name, versions in self.helper_versions.items():
            current = (ROOT / 'scripts' / (name + '.py')).read_bytes()
            desired = {node.name: node for node in ast.parse(current).body if isinstance(node, ast.FunctionDef)}
            for commit, old in versions.items():
                previous = {node.name: node for node in ast.parse(old).body if isinstance(node, ast.FunctionDef)}
                self.assertFalse(previous.keys() - desired.keys())
                self.assertEqual([ast.dump(node) for node in ast.parse(old).body if not isinstance(node, ast.FunctionDef)],
                                 [ast.dump(node) for node in ast.parse(current).body if not isinstance(node, ast.FunctionDef)])
                for method, node in desired.items():
                    if method not in previous or ast.dump(node) != ast.dump(previous[method]):
                        target = name + '.' + method
                        changed.add(target)
                        expected = update.HELPER_UPGRADES[target]
                        prior = update.signature(update.source_function(old, (method,), {'__name__': name})[0]) if method in previous else None
                        self.assertIn(prior, expected)
                        final = update.source_function(current, (method,), {'__name__': name})[0]
                        self.assertEqual(update.signature(final), expected[-1])
        self.assertEqual(changed, set(update.HELPER_UPGRADES))

    def previous_implementation_upgrades(self, commit):
        helper = self.legacy_context_helper(commit)
        helper_callbacks = self.helper_state(helper)
        callbacks = {}
        for target in update.EXPECTED:
            name, *path = target.split('.')
            owner, method = self.target(target)
            live = getattr(owner, method)
            try:
                prior, _ = update.source_function(self.prior_sources[commit][name], tuple(path), vars(self.modules[name]),
                                                  closure=live.__closure__)
            except RuntimeError as error:
                self.assertIn('source structure', str(error))
                self.assertIsNone(update.EXPECTED[target][0])
                delattr(owner, method)
                continue
            self.assertIn(update.signature(prior), update.EXPECTED[target])
            live.__code__, live.__defaults__, live.__kwdefaults__ = prior.__code__, prior.__defaults__, prior.__kwdefaults__
            callbacks[target] = live
        constants = {}
        for target, allowed in update.CONSTANTS.items():
            name, key = target.split('.')
            node = next(node for node in ast.parse(self.prior_sources[commit][name]).body if isinstance(node, ast.Assign)
                        and any(isinstance(value, ast.Name) and value.id == key for value in node.targets))
            prior = ast.literal_eval(node.value)
            self.assertIn(prior, allowed)
            constants[target] = live = getattr(self.modules[name], key)
            live.symmetric_difference_update(live ^ prior)
        imports = []
        for name, values in update.GLOBAL_IMPORTS.items():
            prior_imports = {value.asname or value.name for node in ast.parse(self.prior_sources[commit][name]).body
                             if isinstance(node, (ast.Import, ast.ImportFrom)) for value in node.names}
            for key, (provider, attribute) in values.items():
                if key not in prior_imports:
                    vars(self.modules[name]).pop(key, None)
                previous = vars(self.modules[name]).get(key, update.MISSING)
                imports.append((name, key, provider, attribute, previous))
        before = self.state()
        self.assertEqual(self.apply()['status'], 'applied')
        after = self.state()
        self.assertEqual((before[1], before[3:-1]), (after[1], after[3:-1]))
        for name, key, provider, attribute, previous in imports:
            expected = getattr(sys.modules[provider], attribute) if attribute else sys.modules[provider]
            self.assertIs(vars(self.modules[name])[key], expected)
            if previous is not update.MISSING:
                self.assertIs(vars(self.modules[name])[key], previous)
        for target, previous in constants.items():
            name, key = target.split('.')
            self.assertIs(getattr(self.modules[name], key), previous)
            self.assertEqual(previous, update.CONSTANTS[target][-1])
        for target, previous in callbacks.items():
            self.assertIs(getattr(*self.target(target)), previous)
            self.assertEqual(update.signature(previous), update.EXPECTED[target][-1])
        for target, expected in update.HELPER_UPGRADES.items():
            self.assertEqual(update.signature(getattr(helper, target.split('.')[-1])), expected[-1])
        for name, previous, code, defaults, kwdefaults in helper_callbacks:
            current = getattr(helper, name)
            self.assertIs(current, previous)
            if helper.__name__ + '.' + name not in update.HELPER_UPGRADES:
                self.assertEqual((current.__code__, current.__defaults__, current.__kwdefaults__),
                                 (code, defaults, kwdefaults))
        self.assertEqual(self.apply()['status'], 'already_applied')

    def test_exact_7415_implementation_upgrades_with_existing_callbacks(self):
        self.previous_implementation_upgrades('7415ada')

    def test_exact_f496_implementation_upgrades_with_existing_callbacks(self):
        self.previous_implementation_upgrades('f496684')

    def test_exact_a50_implementation_upgrades_with_existing_callbacks(self):
        self.previous_implementation_upgrades('a50ac70')

    def test_exact_7f_implementation_upgrades_with_existing_callbacks(self):
        self.previous_implementation_upgrades('7f77579')

    def test_exact_94b_implementation_upgrades_with_existing_callbacks(self):
        self.previous_implementation_upgrades('94b72e2')

    def test_exact_116_implementation_upgrades_with_existing_callbacks(self):
        self.previous_implementation_upgrades('116ed9c')

    def test_94b_active_event_read_and_late_failure_preserve_helper(self):
        helper = self.legacy_context_helper('94b72e2')
        before = self.state(), self.helper_state(helper)
        with patch.object(update, '_active_frames', side_effect=[None, RuntimeError('Controlled late frame failure')]):
            with self.assertRaisesRegex(RuntimeError, 'Controlled late frame failure'):
                self.apply()
        self.assertEqual(before, (self.state(), self.helper_state(helper)))
        entered, release = threading.Event(), threading.Event()
        results = []
        class PendingDB:
            def execute(self, *args):
                entered.set()
                if not release.wait(15):
                    raise AssertionError('Fixture did not release the event read')
                return []
        def read():
            results.append(helper.verified_events(PendingDB(), {'id': 'agent'}))
        thread = threading.Thread(target=read)
        thread.start()
        try:
            self.assertTrue(entered.wait(5))
            with self.assertRaisesRegex(RuntimeError, 'earlier call.*verified_events'):
                self.apply()
            self.assertEqual(before, (self.state(), self.helper_state(helper)))
        finally:
            release.set()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(results, [[]])
        self.assertEqual(self.apply()['status'], 'applied')
        self.assertEqual(self.apply()['status'], 'already_applied')

    def test_a50_recovery_guards_and_rollback(self):
        helper = self.legacy_context_helper('a50ac70')
        target = helper.recover_context_failures
        original = target.__code__, target.__defaults__
        for kind in ('code', 'defaults'):
            if kind == 'code':
                target.__code__ = target.__code__.replace(co_consts=target.__code__.co_consts + ('unknown',))
            else:
                target.__defaults__ = ('unknown',)
            before = self.state(), self.helper_state(helper)
            with self.subTest(kind=kind), self.assertRaisesRegex(RuntimeError, 'helper member'):
                self.apply()
            self.assertEqual(before, (self.state(), self.helper_state(helper)))
            target.__code__, target.__defaults__ = original
        before = self.state(), self.helper_state(helper)
        with patch.object(update, '_active_frames', side_effect=[None, RuntimeError('Controlled late frame failure')]):
            with self.assertRaisesRegex(RuntimeError, 'Controlled late frame failure'):
                self.apply()
        self.assertEqual(before, (self.state(), self.helper_state(helper)))
        entered, release = threading.Event(), threading.Event()
        def agents():
            entered.set()
            if not release.wait(15):
                raise AssertionError('Fixture did not release recovery')
            return
            yield
        thread = threading.Thread(target=target, args=(None, None, agents()))
        thread.start()
        try:
            self.assertTrue(entered.wait(5))
            with self.assertRaisesRegex(RuntimeError, 'earlier call.*recover_context_failures'):
                self.apply()
            self.assertEqual(before, (self.state(), self.helper_state(helper)))
        finally:
            release.set()
            thread.join(5)
        self.assertFalse(thread.is_alive())
        self.assertEqual(self.apply()['status'], 'applied')
        self.assertIs(helper.recover_context_failures, target)
        target(None, None, [{'status': 'failed', 'error': {'type': 'usageLimitExceeded'}}])
        self.assertEqual(self.apply()['status'], 'already_applied')

    def test_previous_live_helper_callbacks_upgrade_without_state_changes(self):
        helper = self.legacy_context_helper()
        before, callbacks = self.state(), self.helper_state(helper)
        self.assertEqual(self.apply()['status'], 'applied')
        self.assertEqual(before, self.state())
        for name, previous, code, defaults, kwdefaults in callbacks:
            current = getattr(helper, name)
            self.assertIs(current, previous)
            target = helper.__name__ + '.' + name
            if target in update.HELPER_UPGRADES:
                self.assertEqual(update.signature(current), update.HELPER_UPGRADES[target][-1])
            else:
                self.assertEqual((current.__code__, current.__defaults__, current.__kwdefaults__),
                                 (code, defaults, kwdefaults))
        after = self.helper_state(helper)
        self.assertEqual(self.apply()['status'], 'already_applied')
        self.assertEqual(after, self.helper_state(helper))

    def test_previous_helper_unknown_code_or_defaults_reject(self):
        helper = self.legacy_context_helper()
        for target in update.HELPER_UPGRADES:
            method = target.split('.')[-1]
            function = getattr(helper, method, update.MISSING)
            if function is update.MISSING:
                setattr(helper, method, lambda: None)
                before = self.state(), self.helper_state(helper)
                with self.subTest(target=target), self.assertRaisesRegex(RuntimeError, 'helper member'):
                    self.apply()
                self.assertEqual(before, (self.state(), self.helper_state(helper)))
                delattr(helper, method)
                continue
            original = function.__code__, function.__defaults__
            for kind in ('code', 'defaults'):
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unknown',))
                else:
                    function.__defaults__ = ('unknown',)
                before = self.state(), self.helper_state(helper)
                with self.subTest(target=target, kind=kind), self.assertRaisesRegex(RuntimeError, 'helper member'):
                    self.apply()
                self.assertEqual(before, (self.state(), self.helper_state(helper)))
                function.__code__, function.__defaults__ = original

    def test_late_frame_refusal_restores_functions_globals_and_new_helper_members(self):
        helper = update._stage_module('codex_context_repair',
            self.helper_versions['codex_context_repair']['f496684'], ROOT / 'scripts')
        sys.modules['codex_context_repair'] = helper
        before = self.state(), self.helper_state(helper)
        with patch.object(update, '_active_frames', side_effect=[None, RuntimeError('Controlled late frame failure')]):
            with self.assertRaisesRegex(RuntimeError, 'Controlled late frame failure'):
                self.apply()
        self.assertEqual(before, (self.state(), self.helper_state(helper)))
        self.assertEqual(self.apply()['status'], 'applied')

    def test_previous_helper_partial_assignment_restores_exact_callbacks(self):
        helper = self.legacy_context_helper()
        target, armed = helper._repair, [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled helper assignment failure')
        sys.addaudithook(audit)
        before = self.state(), self.helper_state(helper)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled helper assignment'):
                self.apply()
        finally:
            armed[0] = False
        self.assertEqual(before, (self.state(), self.helper_state(helper)))
        self.assertEqual(self.apply()['status'], 'applied')

    def test_each_previous_helper_active_frame_blocks_cutover(self):
        for method in ('_native_idle', '_repair'):
            helper = self.legacy_context_helper()
            entered, release = threading.Event(), threading.Event()
            errors = []
            def pending():
                entered.set()
                if not release.wait(15):
                    raise AssertionError('Fixture did not release the helper')
                raise ValueError('Previous helper call settled')
            class PendingServer:
                def call(self, *args, **kwargs):
                    return pending()
            class PendingRuntime:
                @property
                def lock(self):
                    return pending()
            def call():
                try:
                    if method == '_native_idle':
                        helper._native_idle(PendingServer(), 'thread')
                    else:
                        helper._repair(PendingRuntime(), 'agent', None)
                except ValueError as error:
                    errors.append(str(error))
            thread = threading.Thread(target=call)
            thread.start()
            try:
                self.assertTrue(entered.wait(5))
                before = self.state(), self.helper_state(helper)
                with self.subTest(method=method), self.assertRaisesRegex(RuntimeError, 'earlier call.*' + method):
                    self.apply()
                self.assertEqual(before, (self.state(), self.helper_state(helper)))
            finally:
                release.set()
                thread.join(5)
            self.assertFalse(thread.is_alive())
            self.assertEqual(errors, ['Previous helper call settled'])
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
