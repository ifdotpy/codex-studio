#!/usr/bin/env python3
"""Guarded live method replacement against exact reviewed Python 3.14 bytecode."""
import ast
import importlib.util
from pathlib import Path
import subprocess
import sys
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('update_fixture', Path(__file__).with_name('token-efficiency-contract.py'))
f = importlib.util.module_from_spec(spec); spec.loader.exec_module(f)
import codex_efficiency_update as update


@unittest.skipUnless(sys.version_info[:3] == (3, 14, 7), 'Live bytecode guard targets Python 3.14.7')
class UpdateContract(unittest.TestCase):
    setUp = f.EfficiencyContract.setUp
    tearDown = f.EfficiencyContract.tearDown
    lead = f.EfficiencyContract.lead
    start = f.EfficiencyContract.start
    events = f.EfficiencyContract.events
    tool = f.EfficiencyContract.tool

    def legacy(self):
        originals = {}
        baseline_globals = self.runtime.dynamic.__func__.__globals__.copy()
        baseline_globals['TOOLS'] = [d for d in baseline_globals['TOOLS'] if d['name'] not in {'orchestration_read', 'orchestration_context'}]
        for revision in ['30acccdb5fd7aecbc92d6ff0ef3ea2d1dcbf8160', '568abb63e3167eb655aab918d5f8ba7c33b39796']:
            text = subprocess.check_output(['git', 'show', revision + ':scripts/codex_runtime.py'], cwd=Path(__file__).resolve().parents[1], text=True)
            module = compile(text, '<reviewed-baseline>', 'exec', dont_inherit=True)
            cls = next(c for c in module.co_consts if isinstance(c, types.CodeType) and c.co_name == 'Runtime')
            nodes = next(c for c in ast.parse(text).body if isinstance(c, ast.ClassDef) and c.name == 'Runtime')
            for name in update.BASE.keys() - {'transcript_tool_result'}:
                if name == 'transcript' and revision.startswith('30ac'):
                    continue
                if name != 'transcript' and not revision.startswith('30ac'):
                    continue
                code = next(c for c in cls.co_consts if isinstance(c, types.CodeType) and c.co_name == name)
                node = next(n for n in nodes.body if isinstance(n, ast.FunctionDef) and n.name == name)
                fn = types.FunctionType(code, baseline_globals, name, tuple(ast.literal_eval(v) for v in node.args.defaults) or None)
                fn.__kwdefaults__ = {arg.arg: ast.literal_eval(v) for arg, v in zip(node.args.kwonlyargs, node.args.kw_defaults) if v is not None}
                originals[name] = types.MethodType(fn, self.runtime)
        originals['transcript_tool_result'] = None
        for name, method in originals.items():
            setattr(self.runtime, name, method)
        self.assertEqual({n: update.fingerprint(getattr(self.runtime, n)) for n in update.BASE}, update.BASE)

    def test_update_preserves_active_turn_command_connections_and_search_fix(self):
        self.legacy()
        lead = self.start(self.lead())
        monitor = self.runtime.monitor(lead['id'], {'command': 'fixture-long-command'}, approved=True)
        f.f.eventually(lambda: any(method == 'command/exec' for method, _ in self.runtime.server.calls))
        identities = {name: getattr(self.runtime, name) for name in ['servers', 'server', 'pool', 'coordination_pool', 'recovery_pool', 'index_item']}
        turn = self.runtime.agent(lead['id'])['turnId']
        result = update.apply(self.runtime)
        self.assertEqual(result['status'], 'applied')
        for name, value in identities.items():
            self.assertEqual(getattr(self.runtime, name), value)
        self.assertEqual(self.runtime.agent(lead['id'])['turnId'], turn)
        self.assertFalse(self.runtime.server.gate.is_set())
        fresh = self.start(self.lead(name='New lead'))
        params = [p for method, p in self.runtime.server.calls if method == 'thread/start'][-1]
        names = {d['name'] for d in params['dynamicTools']}
        self.assertTrue({'orchestration_read', 'orchestration_context'} <= names)
        response = self.tool(lead, 'orchestration_task', {'action': 'list'})
        self.assertTrue(response['success'], response)
        self.assertIn('apiVersion', response['contentItems'][0]['text'])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
        self.runtime.server.gate.set()
        f.f.eventually(lambda: len(self.events(lead, 'monitor_exit')) == 1)

    def test_unknown_method_rejects_entire_update(self):
        self.legacy()
        self.runtime.monitor = types.MethodType(lambda *args, **kwargs: None, self.runtime)
        before = dict(self.runtime.__dict__)
        with self.assertRaisesRegex(RuntimeError, 'Unknown or mixed'):
            update.apply(self.runtime)
        self.assertEqual(self.runtime.__dict__, before)

    def test_installed_source_is_idempotent(self):
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')


if __name__ == '__main__':
    unittest.main(verbosity=2)
