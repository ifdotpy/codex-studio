#!/usr/bin/env python3
"""Review pair live updates preserve callbacks and reject unknown code atomically."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_chat_reviews as reviews
import codex_team_isolation as isolation
import codex_review_pair_update as update
from codex_active_task_update import compile_function, signature
from codex_progress_update import source_instructions, digest
from codex_team_isolation_update import _tools


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.targets = [(reviews, None, 'review_schedule'), (reviews, None, 'review_tick'),
                        (codex_runtime, 'Runtime', 'chat_rooms'),
                        (codex_runtime, 'Runtime', 'chat_message'),
                        (isolation, None, 'validate_event')]
        self.functions = [getattr(getattr(m, c) if c else m, n) for m, c, n in self.targets]
        self.saved = [fn.__code__ for fn in self.functions]
        self.helper = reviews.review_pair_allowed
        self.instructions = codex_runtime.INSTRUCTIONS
        self.tools = list(codex_runtime.TOOLS)
        self.addCleanup(self.restore)
        for (module, cls, name), fn in zip(self.targets, self.functions):
            source = subprocess.check_output(
                ['git', 'show', 'c0fcaa5:scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True)
            fn.__code__ = compile_function(source, cls, name, vars(module)).__code__
            if module is codex_runtime:
                codex_runtime.INSTRUCTIONS = source_instructions(source)
                codex_runtime.TOOLS[:] = _tools(source, self.tools)
        del reviews.review_pair_allowed

    def restore(self):
        for fn, code in zip(self.functions, self.saved):
            fn.__code__ = code
        reviews.review_pair_allowed = self.helper
        codex_runtime.INSTRUCTIONS = self.instructions
        codex_runtime.TOOLS[:] = self.tools

    def test_patch_adds_helper_preserves_callbacks_and_is_idempotent(self):
        tool_list = codex_runtime.TOOLS
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        for (module, cls, name), fn in zip(self.targets, self.functions):
            self.assertIs(getattr(getattr(module, cls) if cls else module, name), fn)
            self.assertEqual(signature(fn), update.EXPECTED[module.__name__ + '.' + name][1])
        self.assertEqual(signature(reviews.review_pair_allowed),
                         update.EXPECTED['codex_chat_reviews.review_pair_allowed'][1])
        self.assertEqual(digest(codex_runtime.INSTRUCTIONS), update.INSTRUCTIONS[1])
        self.assertEqual(digest(codex_runtime.TOOLS), update.TOOLS[1])
        self.assertIs(codex_runtime.TOOLS, tool_list)
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_last_function_prevents_all_mutations(self):
        before = [fn.__code__ for fn in self.functions[:-1]]
        instructions = codex_runtime.INSTRUCTIONS
        tools = list(codex_runtime.TOOLS)
        self.functions[-1].__code__ = (lambda runtime, db, recipient, event: None).__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        for fn, code in zip(self.functions[:-1], before):
            self.assertIs(fn.__code__, code)
        self.assertFalse(hasattr(reviews, 'review_pair_allowed'))
        self.assertEqual(codex_runtime.INSTRUCTIONS, instructions)
        self.assertEqual(codex_runtime.TOOLS, tools)


if __name__ == '__main__':
    unittest.main()
