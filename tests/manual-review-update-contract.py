#!/usr/bin/env python3
"""Manual review live updates preserve callbacks and reject unknown code atomically."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_chat_reviews as reviews
import codex_manual_review_update as update
from codex_active_task_update import compile_function, signature


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.targets = [(reviews, None, 'review_schedule'), (reviews, None, '_prompt')]
        self.functions = [getattr(getattr(m, c) if c else m, n) for m, c, n in self.targets]
        self.saved = [fn.__code__ for fn in self.functions]
        self.helper = reviews.review_now
        self.addCleanup(self.restore)
        for (module, cls, name), fn in zip(self.targets, self.functions):
            source = subprocess.check_output(
                ['git', 'show', '39b00fe:scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True)
            fn.__code__ = compile_function(source, cls, name, vars(module)).__code__
        del reviews.review_now

    def restore(self):
        for fn, code in zip(self.functions, self.saved):
            fn.__code__ = code
        reviews.review_now = self.helper

    def test_patch_adds_helper_preserves_callbacks_and_is_idempotent(self):
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        for (module, cls, name), fn in zip(self.targets, self.functions):
            self.assertIs(getattr(getattr(module, cls) if cls else module, name), fn)
            self.assertEqual(signature(fn), update.EXPECTED[module.__name__ + '.' + name][1])
        self.assertEqual(signature(reviews.review_now),
                         update.EXPECTED['codex_chat_reviews.review_now'][1])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_last_function_prevents_all_mutations(self):
        before = [fn.__code__ for fn in self.functions[:-1]]
        self.functions[-1].__code__ = (lambda runtime, db, recipient, event: None).__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        for fn, code in zip(self.functions[:-1], before):
            self.assertIs(fn.__code__, code)
        self.assertFalse(hasattr(reviews, 'review_now'))


if __name__ == '__main__':
    unittest.main()
