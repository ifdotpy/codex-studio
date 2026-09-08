#!/usr/bin/env python3
"""Exercise the exact guarded live update without user state or model calls."""
import ast
import importlib.util
from pathlib import Path
import subprocess
import sys
import types
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_complaints_update as update
import codex_thread_cache_update as thread_update


@unittest.skipUnless(sys.version_info[:2] == (3, 14), 'Live bytecode targets Python 3.14')
class ComplaintUpdate(unittest.TestCase):
    setUp = f.RuntimeContract.setUp
    tearDown = f.RuntimeContract.tearDown
    lead = f.RuntimeContract.lead

    def legacy(self):
        self.source = subprocess.check_output(['git', 'show', thread_update.NOTIFICATION_REVISION + ':scripts/codex_runtime.py'], cwd=ROOT, text=True)
        self.runtime.notification = thread_update.replacements(self.runtime, self.source)['notification']
        text = subprocess.check_output(['git', 'show', '7461f948d361846f6084099354b0016477e1d4a4:scripts/codex_runtime.py'], cwd=ROOT, text=True)
        tree = ast.parse(text)
        for name, expected in update.BASE.items():
            if name == 'notification':
                continue
            if expected is None:
                setattr(self.runtime, name, None)
                continue
            node = update.method_node(tree, name)
            previous = getattr(self.runtime, name).__func__
            function = types.FunctionType(update.method_code(tree, name), previous.__globals__, name,
                                          tuple(ast.literal_eval(v) for v in node.args.defaults) or None)
            function.__kwdefaults__ = previous.__kwdefaults__
            setattr(self.runtime, name, types.MethodType(function, self.runtime))
        self.assertEqual({n: update.fingerprint(getattr(self.runtime, n, None)) for n in update.BASE}, update.BASE)

    def test_update_preserves_native_identity_and_active_command(self):
        self.legacy()
        lead = self.lead()
        monitor = self.runtime.monitor(lead['id'], {'command': 'fixture-long-command'}, approved=True)
        f.eventually(lambda: any(m == 'command/exec' for m, _ in self.runtime.server.calls))
        objects = {n: getattr(self.runtime, n) for n in ['servers', 'server', 'pool', 'coordination_pool', 'recovery_pool']}
        calls = len(self.runtime.server.calls)
        turn = self.runtime.agent(lead['id'])['turnId']
        self.assertEqual(update.apply(self.runtime, self.source)['status'], 'applied')
        self.assertEqual(update.apply(self.runtime, self.source)['status'], 'already_applied')
        for name, obj in objects.items():
            self.assertIs(getattr(self.runtime, name), obj)
        self.assertEqual(len(self.runtime.server.calls), calls)
        self.assertEqual(self.runtime.agent(lead['id'])['turnId'], turn)
        self.assertFalse(self.runtime.server.gate.is_set())
        self.runtime.cancel_monitor(monitor['id'])
        self.runtime.server.gate.set()
        f.eventually(lambda: self.runtime.snapshot()['monitors'][0]['status'] == 'cancelled')

    def test_unknown_method_rejects_before_any_assignment(self):
        self.legacy()
        self.runtime.chat_message = types.MethodType(lambda *a, **k: None, self.runtime)
        before = {n: getattr(self.runtime, n, None) for n in update.BASE}
        with self.assertRaisesRegex(RuntimeError, 'Unknown runtime methods'):
            update.apply(self.runtime, self.source)
        self.assertEqual({n: getattr(self.runtime, n, None) for n in update.BASE}, before)

    def test_unknown_notification_source_rejects_before_update(self):
        self.legacy()
        before = self.runtime.notification
        with self.assertRaisesRegex(RuntimeError, 'Unknown notification source'):
            update.apply(self.runtime, self.source.replace('thread/tokenUsage/updated', 'thread/unrecognized'))
        self.assertIs(self.runtime.notification, before)


if __name__ == '__main__':
    unittest.main(verbosity=2)
