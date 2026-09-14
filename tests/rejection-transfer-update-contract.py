#!/usr/bin/env python3
"""Live update preserves function identities and rejects unsupported baselines."""
from pathlib import Path
import subprocess
import sys
import threading
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_tool_requests
import codex_account_transfer
import codex_rejection_transfer_update as update
from codex_active_task_update import compile_function


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.functions = [codex_tool_requests.request_result_outcome,
                          codex_account_transfer.AccountTransfers.local_blocker]
        self.codes = [fn.__code__ for fn in self.functions]
        self.addCleanup(self.restore)

    def restore(self):
        for fn, code in zip(self.functions, self.codes):
            fn.__code__ = code

    def install_baseline(self):
        for module, cls, name, fn in (
            (codex_tool_requests, None, 'request_result_outcome', self.functions[0]),
            (codex_account_transfer, 'AccountTransfers', 'local_blocker', self.functions[1]),
        ):
            source = subprocess.check_output(['git', 'show', '80f0f3d:scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True)
            fn.__code__ = compile_function(source, cls, name, vars(module)).__code__

    def test_baseline_patch_is_idempotent_and_keeps_callbacks(self):
        self.install_baseline()
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(self.functions[0], codex_tool_requests.request_result_outcome)
        self.assertIs(self.functions[1], codex_account_transfer.AccountTransfers.local_blocker)
        result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': 'This record belongs to another team'}]}
        self.assertEqual(self.functions[0]({'tool': 'orchestration_message'}, result), 'not_applied')
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_second_function_does_not_partially_apply(self):
        self.install_baseline()
        before = self.functions[0].__code__
        self.functions[1].__code__ = (lambda self, db, a: None).__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        self.assertIs(self.functions[0].__code__, before)

    def test_instance_override_is_rejected(self):
        self.runtime._account_transfers = object.__new__(codex_account_transfer.AccountTransfers)
        self.runtime._account_transfers.rt = self.runtime
        self.runtime._account_transfers.local_blocker = lambda db, a: None
        with self.assertRaisesRegex(RuntimeError, 'Unknown account transfer'):
            update.apply(self.runtime)


if __name__ == '__main__':
    unittest.main()
