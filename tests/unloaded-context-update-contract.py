#!/usr/bin/env python3
"""The live patch preserves function identity and accepts only reviewed code."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_context_repair as repair
import codex_unloaded_context_update as update
from codex_active_task_update import compile_function, signature


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.function = repair._native_idle
        self.code = self.function.__code__
        self.addCleanup(setattr, self.function, '__code__', self.code)
        source = subprocess.check_output(['git', 'show',
            'fe65888b67a10d1b77446d4386028e338c7efb00:scripts/codex_context_repair.py'],
            cwd=ROOT, text=True)
        baseline = compile_function(source, None, '_native_idle', vars(repair))
        self.function.__code__ = baseline.__code__

    def test_update_preserves_function_and_repeated_apply_does_nothing(self):
        self.assertEqual(signature(self.function), update.BASE)
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(repair._native_idle, self.function)
        self.assertEqual(signature(self.function), update.TARGET)
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_code_is_not_replaced(self):
        self.function.__code__ = (lambda *args, **kwargs: None).__code__
        before = self.function.__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        self.assertIs(self.function.__code__, before)

    def test_closed_runtime_is_not_changed(self):
        self.runtime.closed = True
        with self.assertRaisesRegex(RuntimeError, 'Runtime is closed'):
            update.apply(self.runtime)
        self.assertEqual(signature(self.function), update.BASE)


if __name__ == '__main__':
    unittest.main()
