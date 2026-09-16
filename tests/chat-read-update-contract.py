#!/usr/bin/env python3
"""The live chat fix preserves callbacks and refuses unknown implementations."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_workspace
import codex_chat_read_update as update
from codex_active_task_update import compile_function, signature


class ChatReadUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.targets = [(codex_workspace, 'WorkspaceMixin', 'asset_record'),
                        (codex_runtime, 'Runtime', 'transcript')]
        self.functions = [getattr(getattr(m, c), n) for m, c, n in self.targets]
        self.saved = [(fn.__code__, fn.__defaults__, fn.__kwdefaults__) for fn in self.functions]
        self.addCleanup(self.restore)
        for (module, cls, name), fn in zip(self.targets, self.functions):
            source = subprocess.check_output(['git', 'show',
                'ddf18164ea8884fb4969970f82a29b78c66bb95b:scripts/' + module.__name__ + '.py'],
                cwd=ROOT, text=True)
            old = compile_function(source, cls, name, vars(module))
            fn.__code__, fn.__defaults__, fn.__kwdefaults__ = old.__code__, old.__defaults__, old.__kwdefaults__

    def restore(self):
        for fn, saved in zip(self.functions, self.saved):
            fn.__code__, fn.__defaults__, fn.__kwdefaults__ = saved

    def test_preserves_callbacks_and_can_apply_twice(self):
        callbacks = [self.runtime.asset_record, self.runtime.transcript]
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        for (module, cls, name), fn, callback in zip(self.targets, self.functions, callbacks):
            self.assertIs(callback.__func__, fn)
            self.assertEqual(signature(fn), update.EXPECTED[module.__name__ + '.' + name][1])
        self.assertEqual(self.runtime.asset_record.__defaults__, (None,))
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_caller_prevents_partial_update(self):
        before = signature(self.functions[0])
        self.functions[-1].__code__ = (lambda *args: None).__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        self.assertEqual(signature(self.functions[0]), before)


if __name__ == '__main__':
    unittest.main()
