#!/usr/bin/env python3
"""Exercise the review live patch against the exact installed baseline."""
import ast
import copy
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('agent_review_patch_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_agent_review
import codex_agent_review_update as update
import codex_runtime
import codex_tool_requests
from codex_active_task_update import signature
from codex_progress_update import source_function, source_instructions


def source_tools(raw):
    nodes = [node for node in ast.parse(raw).body if
             (isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == 'TOOLS' for t in node.targets))
             or (isinstance(node, ast.AugAssign) and isinstance(node.target, ast.Name) and node.target.id == 'TOOLS')
             or (isinstance(node, ast.For) and isinstance(node.iter, ast.Name) and node.iter.id == 'TOOLS')]
    namespace = vars(codex_runtime).copy()
    exec(compile(ast.Module(body=nodes, type_ignores=[]), '<fixture tools>', 'exec'), namespace)
    return namespace['TOOLS']


class AgentReviewPatch(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        with patch.object(f.Runtime, 'schedule', lambda self: None):
            self.rt = f.Runtime(Path(self.temp.name), f.FakeServer)
        self.addCleanup(self.rt.close)
        self.server = self.rt.connect()
        self.saved = []
        self.tools = list(codex_runtime.TOOLS)
        self.instructions = codex_runtime.INSTRUCTIONS
        self.imported = codex_runtime.review_tools
        self.addCleanup(self.restore)

    def owner(self, target):
        module_name, *path = target.split('.')
        module = sys.modules[module_name]
        return module, path, getattr(module, path[0]) if len(path) > 1 else module

    def restore(self):
        for owner, name, descriptor, state in reversed(self.saved):
            setattr(owner, name, descriptor)
            function = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            function.__code__, function.__defaults__, function.__kwdefaults__ = state
        codex_runtime.TOOLS[:] = self.tools
        codex_runtime.INSTRUCTIONS = self.instructions
        codex_runtime.review_tools = self.imported

    def baseline(self):
        sources = {}
        for target, allowed in update.EXPECTED.items():
            module, path, owner = self.owner(target)
            descriptor = vars(owner)[path[-1]]
            function = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            self.saved.append((owner, path[-1], descriptor, (function.__code__, function.__defaults__, function.__kwdefaults__)))
            if allowed[0] is None:
                delattr(owner, path[-1])
            else:
                if module.__name__ not in sources:
                    sources[module.__name__] = subprocess.check_output(['git', 'show', update.BASE_COMMIT + ':scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True)
                old, _ = source_function(sources[module.__name__], path, vars(module))
                self.assertEqual(signature(old), allowed[0], target)
                function.__code__, function.__defaults__, function.__kwdefaults__ = old.__code__, old.__defaults__, old.__kwdefaults__
        codex_runtime.TOOLS[:] = source_tools(sources['codex_runtime'])
        codex_runtime.INSTRUCTIONS = source_instructions(sources['codex_runtime'])
        del codex_runtime.review_tools

    def signatures(self):
        return {target: signature(getattr(self.owner(target)[2], self.owner(target)[1][-1]))
                if self.owner(target)[1][-1] in vars(self.owner(target)[2]) else None for target in update.EXPECTED}

    def test_baseline_repeat_preserves_active_connections_and_existing_tool_objects(self):
        self.baseline()
        functions = {target: getattr(self.owner(target)[2], self.owner(target)[1][-1])
                     for target, allowed in update.EXPECTED.items() if allowed[0] is not None}
        tools = codex_runtime.TOOLS
        entries = list(tools)
        serialized = json.dumps(tools)
        native_snapshot = copy.deepcopy(tools)
        servers, connections = self.rt.servers, dict(self.rt.connection_ids)
        callbacks = (self.server.notify, self.server.request, self.server.died)
        self.assertEqual(update.apply(self.rt)['status'], 'applied')
        self.assertEqual(update.apply(self.rt)['status'], 'already_applied')
        self.assertIs(codex_runtime.TOOLS, tools)
        self.assertEqual(json.dumps(tools[:-1]), serialized)
        self.assertEqual(json.dumps(native_snapshot), serialized)
        self.assertTrue(all(a is b for a, b in zip(entries, tools)))
        self.assertEqual(tools[-1]['name'], 'orchestration_review')
        self.assertIs(self.rt.servers, servers)
        self.assertIs(self.rt.servers['default'], self.server)
        self.assertEqual(self.rt.connection_ids, connections)
        self.assertEqual(callbacks, (self.server.notify, self.server.request, self.server.died))
        self.assertFalse(self.server.closed)
        for target, allowed in update.EXPECTED.items():
            live = getattr(self.owner(target)[2], self.owner(target)[1][-1])
            self.assertEqual(signature(live), allowed[1], target)
            if target in functions:
                self.assertIs(live, functions[target])

    def test_unknown_live_method_rejected_before_mutation(self):
        self.baseline()
        before = self.signatures()
        with patch.object(codex_tool_requests.RequestMixin, 'request_action', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'Unknown live agent review method'):
                update.apply(self.rt)
        self.assertEqual(self.signatures(), before)
        self.assertNotIn('review_tools', vars(codex_runtime))

    def test_unknown_source_rejected(self):
        target = next(iter(update.EXPECTED))
        with patch.dict(update.EXPECTED, {target: [update.EXPECTED[target][0], '0' * 64]}):
            with self.assertRaisesRegex(RuntimeError, 'Unreviewed agent review replacement'):
                update.apply(self.rt)
        with patch.object(update, 'REVIEW_SHA256', '0' * 64):
            with self.assertRaisesRegex(RuntimeError, 'Unreviewed agent review module source'):
                update.apply(self.rt)
        with patch.object(update, 'INSTRUCTIONS', (update.INSTRUCTIONS[0], '0' * 64)):
            with self.assertRaisesRegex(RuntimeError, 'Unreviewed agent review instructions'):
                update.apply(self.rt)

    def test_loaded_module_provenance_rejected(self):
        with patch.object(codex_agent_review, 'request', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'Unknown loaded agent review function'):
                update.apply(self.rt)
        with patch.object(codex_agent_review, '__file__', '/tmp/foreign.py'):
            with self.assertRaisesRegex(RuntimeError, 'Unknown agent review module origin'):
                update.apply(self.rt)
        with patch.object(codex_agent_review, 'uuid', object()):
            with self.assertRaisesRegex(RuntimeError, 'Unknown agent review import'):
                update.apply(self.rt)
        with patch.object(codex_runtime, 'review_tools', lambda *args: []):
            with self.assertRaisesRegex(RuntimeError, 'Unknown agent review tools import'):
                update.apply(self.rt)

    def test_unknown_tool_or_instructions_rejected_before_mutation(self):
        self.baseline()
        before = self.signatures()
        with patch.object(codex_runtime, 'TOOLS', [*codex_runtime.TOOLS, {'name': 'unknown'}]):
            with self.assertRaisesRegex(RuntimeError, 'Unknown live agent review tools'):
                update.apply(self.rt)
        with patch.object(codex_runtime, 'INSTRUCTIONS', 'unknown'):
            with self.assertRaisesRegex(RuntimeError, 'Unknown live agent review instructions'):
                update.apply(self.rt)
        self.assertEqual(self.signatures(), before)

    def test_instance_and_class_request_overrides_rejected(self):
        with patch.object(self.rt, 'dynamic', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'runtime override'):
                update.apply(self.rt)
        with patch.object(codex_runtime.Runtime, 'request_action', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'request override'):
                update.apply(self.rt)

    def test_install_failure_restores_code_tools_instructions_and_imports(self):
        self.baseline()
        before, entries, instructions = self.signatures(), list(codex_runtime.TOOLS), codex_runtime.INSTRUCTIONS
        module = sys.modules.pop('codex_agent_review')
        self.addCleanup(sys.modules.__setitem__, 'codex_agent_review', module)
        real, calls = update._install, 0
        def fail(live, desired):
            nonlocal calls
            real(live, desired)
            calls += 1
            if calls == 3:
                raise RuntimeError('fixture install failure')
        with patch.object(update, '_install', side_effect=fail):
            with self.assertRaisesRegex(RuntimeError, 'fixture install failure'):
                update.apply(self.rt)
        self.assertEqual(self.signatures(), before)
        self.assertEqual(codex_runtime.TOOLS, entries)
        self.assertTrue(all(a is b for a, b in zip(entries, codex_runtime.TOOLS)))
        self.assertEqual(codex_runtime.INSTRUCTIONS, instructions)
        self.assertNotIn('review_tools', vars(codex_runtime))
        self.assertNotIn('codex_agent_review', sys.modules)
        self.assertFalse(self.server.closed)

    def test_failure_after_tool_append_restores_full_transaction(self):
        self.baseline()
        before, entries = self.signatures(), list(codex_runtime.TOOLS)
        module_class = type(codex_runtime)
        class FailOnce(ModuleType):
            fail_next = True
            def __setattr__(self, name, value):
                if name == 'INSTRUCTIONS' and self.fail_next:
                    self.fail_next = False
                    raise RuntimeError('fixture instructions failure')
                return super().__setattr__(name, value)
        codex_runtime.__class__ = FailOnce
        try:
            with self.assertRaisesRegex(RuntimeError, 'fixture instructions failure'):
                update.apply(self.rt)
        finally:
            codex_runtime.__class__ = module_class
            vars(codex_runtime).pop('fail_next', None)
        self.assertEqual(self.signatures(), before)
        self.assertEqual(codex_runtime.TOOLS, entries)
        self.assertNotIn('review_tools', vars(codex_runtime))
        self.assertFalse(self.server.closed)

    def test_start_lock_failure_leaves_every_method_unchanged(self):
        class Busy:
            def acquire(self, **kwargs): return False
        self.baseline()
        before = self.signatures()
        with patch.object(self.rt, 'start_lock', Busy()):
            with self.assertRaisesRegex(RuntimeError, 'Native connection is busy'):
                update.apply(self.rt)
        self.assertEqual(self.signatures(), before)


if __name__ == '__main__':
    unittest.main()
