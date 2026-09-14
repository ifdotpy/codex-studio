#!/usr/bin/env python3
"""Known live baselines update atomically without replacing callbacks."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_context_repair as repair
import codex_efficiency
import codex_role_context_update as update
from codex_active_task_update import compile_function, signature


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.functions = [repair.verified_events, repair.sanitized_rollout, codex_efficiency.EfficiencyMixin.model_known_context]
        self.saved = [(fn.__code__, fn.__kwdefaults__) for fn in self.functions]
        self.addCleanup(self.restore)
        for module, cls, fn in [(repair, None, self.functions[0]), (repair, None, self.functions[1]),
                                (codex_efficiency, 'EfficiencyMixin', self.functions[2])]:
            source = subprocess.check_output(['git','show','b07072e:scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True)
            baseline = compile_function(source, cls, fn.__name__, vars(module))
            fn.__code__ = baseline.__code__
            fn.__kwdefaults__ = baseline.__kwdefaults__

    def restore(self):
        for fn, (code, defaults) in zip(self.functions, self.saved):
            fn.__code__, fn.__kwdefaults__ = code, defaults

    def test_patch_preserves_callbacks_and_keyword_defaults(self):
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        for fn in self.functions:
            self.assertEqual(signature(fn), update.EXPECTED[fn.__module__ + '.' + fn.__name__][1])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_last_function_prevents_partial_update(self):
        before = [(fn.__code__, fn.__kwdefaults__) for fn in self.functions[:-1]]
        self.functions[-1].__code__ = (lambda rt, db, agent: None).__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        for fn, (code, defaults) in zip(self.functions[:-1], before):
            self.assertIs(fn.__code__, code)
            self.assertIs(fn.__kwdefaults__, defaults)


if __name__ == '__main__':
    unittest.main()
