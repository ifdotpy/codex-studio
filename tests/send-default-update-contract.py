#!/usr/bin/env python3
"""The delivery update preserves callbacks and refuses unknown code or tool schemas."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_send_default_update as update
from codex_active_task_update import compile_function, signature


class SendDefaultUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.function = codex_runtime.Runtime.dynamic
        self.saved = (self.function.__code__, self.function.__defaults__, self.function.__kwdefaults__)
        self.tool = next(tool for tool in codex_runtime.TOOLS if tool['name'] == 'orchestration_send')
        self.description = self.tool['description']
        source = subprocess.check_output(['git', 'show', '236776a:scripts/codex_runtime.py'], cwd=ROOT, text=True)
        old = compile_function(source, 'Runtime', 'dynamic', vars(codex_runtime))
        self.function.__code__, self.function.__defaults__, self.function.__kwdefaults__ = (
            old.__code__, old.__defaults__, old.__kwdefaults__)
        self.tool['description'] = update.send_definition(source, vars(codex_runtime))['description']

    def tearDown(self):
        self.function.__code__, self.function.__defaults__, self.function.__kwdefaults__ = self.saved
        self.tool['description'] = self.description

    def test_repeated_update_preserves_callback_and_updates_description(self):
        callback = self.runtime.dynamic
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(callback.__func__, self.function)
        self.assertEqual(signature(self.function), update.EXPECTED['dynamic'][1])
        self.assertEqual(update.definition_hash(self.tool), update.EXPECTED['tool'][1])
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_description_prevents_code_change(self):
        before = signature(self.function)
        self.tool['description'] = 'Unreviewed description'
        with self.assertRaisesRegex(RuntimeError, 'Unknown live send'):
            update.apply(self.runtime)
        self.assertEqual(signature(self.function), before)

    def test_unknown_code_preserves_description(self):
        self.function.__code__ = (lambda *args: None).__code__
        before = self.tool['description']
        with self.assertRaisesRegex(RuntimeError, 'Unknown live send'):
            update.apply(self.runtime)
        self.assertEqual(self.tool['description'], before)


if __name__ == '__main__':
    unittest.main()
