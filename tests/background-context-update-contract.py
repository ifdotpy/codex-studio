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
import codex_background_context_update as update
from codex_active_task_update import compile_function, signature


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.functions = [getattr(repair, name) for name in update.EXPECTED]
        self.saved = [(fn.__code__, fn.__kwdefaults__) for fn in self.functions]
        self.addCleanup(self.restore)
        source = subprocess.check_output(['git', 'show', 'e796276:scripts/codex_context_repair.py'], cwd=ROOT, text=True)
        for fn in self.functions:
            baseline = compile_function(source, None, fn.__name__, vars(repair))
            fn.__code__ = baseline.__code__
            fn.__kwdefaults__ = baseline.__kwdefaults__

    def restore(self):
        for fn, (code, defaults) in zip(self.functions, self.saved):
            fn.__code__, fn.__kwdefaults__ = code, defaults

    def test_patch_preserves_callbacks_and_keyword_defaults(self):
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        for fn in self.functions:
            self.assertIs(fn, getattr(repair, fn.__name__))
            self.assertEqual(signature(fn), update.EXPECTED[fn.__name__][1])
        self.assertEqual(repair._local_idle.__kwdefaults__, {'allow_background_work':False})
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
