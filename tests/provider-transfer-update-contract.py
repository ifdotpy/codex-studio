#!/usr/bin/env python3
"""Guarded provider transfer update against its exact installed baseline."""
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
spec = importlib.util.spec_from_file_location('provider_transfer_fixture', Path(__file__).with_name('runtime-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_account_transfer
import codex_claude_controls
import codex_portable_history
import codex_provider_transfer_update as update
from codex_progress_update import source_function
from codex_active_task_update import signature


class ProviderTransferPatch(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        with patch.object(f.Runtime, 'schedule', lambda self: None):
            self.rt = f.Runtime(Path(self.temp.name), f.FakeServer)
        self.addCleanup(self.rt.close)
        self.server = self.rt.connect()
        self.transfers = codex_account_transfer.transfer_store(self.rt)
        self.saved = []
        self.addCleanup(self.restore)

    def owner(self, target):
        module_name, *path = target.split('.')
        module = sys.modules[module_name]
        return module, path, getattr(module, path[0]) if len(path) > 1 else module

    def restore(self):
        for owner, name, descriptor, state in reversed(self.saved):
            setattr(owner, name, descriptor)
            function = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            function.__code__, function.__defaults__, function.__kwdefaults__ = state

    def baseline(self):
        sources = {}
        for target, allowed in update.EXPECTED.items():
            module, path, owner = self.owner(target)
            descriptor = vars(owner)[path[-1]]
            function = descriptor.__func__ if isinstance(descriptor, staticmethod) else descriptor
            self.saved.append((owner, path[-1], descriptor, (function.__code__, function.__defaults__, function.__kwdefaults__)))
            if allowed[0] is None:
                delattr(owner, path[-1])
            else:
                if module.__name__ not in sources:
                    sources[module.__name__] = subprocess.check_output(['git', 'show', update.BASE_COMMIT + ':scripts/' + module.__name__ + '.py'], cwd=ROOT, text=True)
                old, _ = source_function(sources[module.__name__], path, vars(module))
                self.assertEqual(signature(old), allowed[0], target)
                function.__code__, function.__defaults__, function.__kwdefaults__ = old.__code__, old.__defaults__, old.__kwdefaults__

    def signatures(self):
        return {target: signature(getattr(self.owner(target)[2], self.owner(target)[1][-1])) if self.owner(target)[1][-1] in vars(self.owner(target)[2]) else None for target in update.EXPECTED}

    def test_baseline_repeat_preserves_functions_connections_and_callbacks(self):
        self.baseline()
        functions = {target: getattr(self.owner(target)[2], self.owner(target)[1][-1]) for target, allowed in update.EXPECTED.items() if allowed[0] is not None}
        connections = dict(self.rt.connection_ids)
        servers = self.rt.servers
        callbacks = (self.server.notify, self.server.request, self.server.died)
        self.assertEqual(update.apply(self.rt)['status'], 'applied')
        self.assertEqual(update.apply(self.rt)['status'], 'already_applied')
        self.assertIs(self.rt.servers, servers)
        self.assertIs(self.rt.servers['default'], self.server)
        self.assertEqual(self.rt.connection_ids, connections)
        self.assertFalse(self.server.closed)
        self.assertEqual(callbacks, (self.server.notify, self.server.request, self.server.died))
        for target, allowed in update.EXPECTED.items():
            live = getattr(self.owner(target)[2], self.owner(target)[1][-1])
            self.assertEqual(signature(live), allowed[1], target)
            if target in functions:
                self.assertIs(live, functions[target])

    def test_unknown_live_method_rejected_before_mutation(self):
        self.baseline()
        before = self.signatures()
        with patch.object(codex_account_transfer.AccountTransfers, 'run', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'Unknown live provider transfer method'):
                update.apply(self.rt)
        self.assertEqual(self.signatures(), before)

    def test_unknown_source_rejected(self):
        target = next(iter(update.EXPECTED))
        with patch.dict(update.EXPECTED, {target: [update.EXPECTED[target][0], '0' * 64]}):
            with self.assertRaisesRegex(RuntimeError, 'Unreviewed provider transfer source'):
                update.apply(self.rt)
        with patch.object(update, 'PORTABLE_SHA256', '0' * 64):
            with self.assertRaisesRegex(RuntimeError, 'Unreviewed portable history source'):
                update.apply(self.rt)

    def test_loaded_history_provenance_rejected(self):
        with patch.object(codex_portable_history, '_source', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'Unknown loaded portable history function'):
                update.apply(self.rt)
        with patch.object(codex_portable_history._Recent, 'add', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'Unknown loaded portable history method'):
                update.apply(self.rt)
        with patch.object(codex_portable_history, 'CONTEXT_CHARS', 1):
            with self.assertRaisesRegex(RuntimeError, 'Unknown portable history constant'):
                update.apply(self.rt)
        with patch.object(codex_portable_history, '__file__', '/tmp/foreign.py'):
            with self.assertRaisesRegex(RuntimeError, 'Unknown portable history module origin'):
                update.apply(self.rt)

    def test_active_transfers_and_durable_submissions_reject_patch(self):
        self.baseline()
        before = self.signatures()
        for name in ('running', 'workers', 'futures'):
            with self.subTest(name=name), patch.object(self.transfers, name, {'active'}):
                with self.assertRaisesRegex(RuntimeError, 'account transfer is active'):
                    update.apply(self.rt)
        with self.rt.db() as db:
            self.rt.put(db, 'account_transfers', {'id': 'pending', 'status': 'pending', 'members': {'agent': {'phase': 'submitted'}}})
        with self.assertRaisesRegex(RuntimeError, 'account transfer is active'):
            update.apply(self.rt)
        self.assertEqual(self.signatures(), before)

    def test_instance_override_rejected(self):
        with patch.object(self.transfers, 'run', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'Unknown account transfer instance'):
                update.apply(self.rt)
        with patch.object(self.rt, 'new_thread_params', lambda *args: None):
            with self.assertRaisesRegex(RuntimeError, 'runtime override'):
                update.apply(self.rt)

    def test_install_failure_restores_code_and_removes_added_methods(self):
        self.baseline()
        before = self.signatures()
        real, calls = update._install, 0
        def fail(live, desired):
            nonlocal calls
            real(live, desired)
            calls += 1
            if calls == 7:
                raise RuntimeError('fixture install failure')
        with patch.object(update, '_install', side_effect=fail):
            with self.assertRaisesRegex(RuntimeError, 'fixture install failure'):
                update.apply(self.rt)
        self.assertEqual(self.signatures(), before)
        self.assertFalse(self.server.closed)

    def test_start_lock_failure_leaves_every_method_unchanged(self):
        class Busy:
            def acquire(self, **kwargs): return False
        self.baseline()
        before = self.signatures()
        with patch.object(self.rt, 'start_lock', Busy()):
            with self.assertRaisesRegex(RuntimeError, 'Native connection is busy'):
                update.apply(self.rt)
        self.assertEqual(self.signatures(), before)


if __name__ == '__main__':
    unittest.main()
