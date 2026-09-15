#!/usr/bin/env python3
"""Review brief live updates preserve callbacks and reject unknown code atomically."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_chat_reviews as reviews
import codex_review_brief_update as update
from codex_active_task_update import compile_function, signature


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.targets = [(reviews, None, name) for name in ['review_now', 'review_tick', '_outcome_text', '_snapshot', '_prompt']]
        self.functions = [getattr(getattr(m, c) if c else m, n) for m, c, n in self.targets]
        self.saved = [(fn.__code__, fn.__defaults__, fn.__kwdefaults__) for fn in self.functions]
        self.helper = reviews._review_baseline
        self.addCleanup(self.restore)
        for (module, cls, name), fn in zip(self.targets, self.functions):
            source = subprocess.check_output(
                ['git', 'show', 'db3004c:scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True)
            baseline = compile_function(source, cls, name, vars(module))
            fn.__code__, fn.__defaults__, fn.__kwdefaults__ = baseline.__code__, baseline.__defaults__, baseline.__kwdefaults__
        del reviews._review_baseline

    def restore(self):
        for fn, (code, defaults, kwdefaults) in zip(self.functions, self.saved):
            fn.__code__, fn.__defaults__, fn.__kwdefaults__ = code, defaults, kwdefaults
        reviews._review_baseline = self.helper

    def test_patch_adds_helper_preserves_callbacks_and_is_idempotent(self):
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        for (module, cls, name), fn in zip(self.targets, self.functions):
            self.assertIs(getattr(getattr(module, cls) if cls else module, name), fn)
            self.assertEqual(signature(fn), update.EXPECTED[module.__name__ + '.' + name][1])
        self.assertEqual(signature(reviews._review_baseline),
                         update.EXPECTED['codex_chat_reviews._review_baseline'][1])
        self.assertEqual(reviews._snapshot.__defaults__, (None, None))
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_last_function_prevents_all_mutations(self):
        before = [fn.__code__ for fn in self.functions[:-1]]
        self.functions[-1].__code__ = (lambda runtime, db, recipient, event: None).__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        for fn, code in zip(self.functions[:-1], before):
            self.assertIs(fn.__code__, code)
        self.assertFalse(hasattr(reviews, '_review_baseline'))


if __name__ == '__main__':
    unittest.main()
