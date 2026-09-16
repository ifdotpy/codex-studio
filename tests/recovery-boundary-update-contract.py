#!/usr/bin/env python3
"""The live update preserves callbacks and rejects partial or unknown baselines."""
from pathlib import Path
import subprocess
import sys
import threading
from types import SimpleNamespace
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_turn_recovery as turns
import codex_connection_recovery as connections
import codex_account_transfer as transfers
import codex_recovery_boundary_update as update
from codex_active_task_update import compile_function, signature


class LiveUpdate(unittest.TestCase):
    def setUp(self):
        self.runtime = object.__new__(codex_runtime.Runtime)
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.targets = [(turns, 'TurnRecoveryMixin', 'reconcile_turn'),
                        (connections, None, 'recover'),
                        (transfers, 'AccountTransfers', 'run')]
        self.functions = [getattr(getattr(m, c) if c else m, n) for m, c, n in self.targets]
        self.saved = [fn.__code__ for fn in self.functions]
        self.helpers = [(turns, 'read_native_turn', turns.read_native_turn),
                        (transfers.AccountTransfers, 'wait_for_native_queue',
                         transfers.AccountTransfers.wait_for_native_queue)]
        self.addCleanup(self.restore)
        for (module, cls, name), fn in zip(self.targets, self.functions):
            source = subprocess.check_output(['git', 'show',
                'b860f627a2e7ceaf3ad34d108322f3dd933cb88a:scripts/' + module.__name__ + '.py'],
                cwd=ROOT, text=True)
            fn.__code__ = compile_function(source, cls, name, vars(module)).__code__
        for owner, name, _ in self.helpers:
            delattr(owner, name)

    def restore(self):
        for fn, code in zip(self.functions, self.saved):
            fn.__code__ = code
        for owner, name, fn in self.helpers:
            setattr(owner, name, fn)

    def test_apply_keeps_callbacks_and_is_idempotent(self):
        callback = self.runtime.reconcile_turn
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')
        self.assertIs(callback.__func__, self.runtime.reconcile_turn.__func__)
        for (module, cls, name), fn in zip(self.targets, self.functions):
            self.assertIs(getattr(getattr(module, cls) if cls else module, name), fn)
            self.assertEqual(signature(fn), update.EXPECTED[module.__name__ + '.' + name][1])
        self.assertTrue(hasattr(turns, 'read_native_turn'))
        self.assertTrue(hasattr(transfers.AccountTransfers, 'wait_for_native_queue'))
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')

    def test_unknown_last_function_prevents_any_replacement(self):
        before = [fn.__code__ for fn in self.functions]
        self.functions[-1].__code__ = (lambda *args: None).__code__
        with self.assertRaisesRegex(RuntimeError, 'Unknown live'):
            update.apply(self.runtime)
        for fn, code in zip(self.functions[:-1], before[:-1]):
            self.assertIs(fn.__code__, code)
        self.assertFalse(hasattr(turns, 'read_native_turn'))
        self.assertFalse(hasattr(transfers.AccountTransfers, 'wait_for_native_queue'))

    def test_active_transfer_defers_the_patch_without_stopping_work(self):
        self.runtime._account_transfers = SimpleNamespace(running={'active-agent'})
        before = [fn.__code__ for fn in self.functions]
        with self.assertRaisesRegex(RuntimeError, 'account transfer is active'):
            update.apply(self.runtime)
        self.assertEqual(self.runtime._account_transfers.running, {'active-agent'})
        self.assertEqual([fn.__code__ for fn in self.functions], before)
        self.assertFalse(hasattr(turns, 'read_native_turn'))
        self.runtime._account_transfers.running.clear()
        self.assertEqual(update.apply(self.runtime)['status'], 'applied')


if __name__ == '__main__':
    unittest.main()
