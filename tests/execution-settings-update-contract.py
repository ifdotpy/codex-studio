#!/usr/bin/env python3
"""The execution settings update preserves callbacks and rolls back additions."""
from pathlib import Path
import subprocess
import sys
import threading
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_capacity_retry
import codex_account_transfer
import codex_execution_settings_update as update


class ExecutionSettingsUpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old = {name: subprocess.check_output(['git', 'show', '5217bb9:scripts/' + name + '.py'],
                   cwd=ROOT, text=True) for name in ('codex_runtime', 'codex_capacity_retry', 'codex_account_transfer')}

    def setUp(self):
        self.modules = {}
        for name in self.old:
            module = ModuleType(name)
            vars(module).update(vars(sys.modules[name]))
            self.modules[name] = module
        self.capacity = type('CapacityRetryMixin', (), {'__module__': 'codex_capacity_retry'})
        self.owner = type('Runtime', (self.capacity,), {'__module__': 'codex_runtime'})
        self.transfer = type('AccountTransfers', (), {'__module__': 'codex_account_transfer'})
        for name, owner in (('codex_runtime', self.owner), ('codex_capacity_retry', self.capacity),
                            ('codex_account_transfer', self.transfer)):
            setattr(self.modules[name], owner.__name__, owner)
        self.modules['codex_account_transfer'].__dict__.pop('TransferSettingsConflict', None)
        self.runtime = self.owner()
        self.runtime.lock = threading.RLock()
        self.runtime.closed = False
        self.runtime.connections = {'original': object()}
        self.runtime.database = object()
        self.runtime._account_transfers = self.transfer()
        for target, allowed in update.EXPECTED.items():
            module, owner, name = target.split('.')
            function = self.compile(target)
            self.assertEqual(update.signature(function), allowed[0])
            setattr(getattr(self.modules[module], owner), name, function)
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()
        sys.modules.pop('codex_native_action_settings', None)

    def tearDown(self):
        self.module_patch.stop()

    def compile(self, target, *, current=False, namespace=None):
        module, owner, name = target.split('.')
        source = (ROOT / ('scripts/' + module + '.py')).read_text() if current else self.old[module]
        return update.compile_function(source, owner, name,
            vars(self.modules[module]) if namespace is None else namespace, '<execution-update-test>')

    def state(self):
        values = []
        for target in update.EXPECTED:
            module, owner, name = target.split('.')
            function = vars(getattr(self.modules[module], owner))[name]
            values.append((function, function.__code__))
        return (values, dict(vars(self.transfer)),
                self.modules['codex_account_transfer'].__dict__.get('TransferSettingsConflict'),
                self.runtime.connections, self.runtime.database)

    def apply_and_check_callbacks(self):
        before = self.state()
        receipt = update.apply(self.runtime)
        self.assertEqual(receipt, {'status': 'applied', 'baseCommit': '5217bb9', 'methods': list(update.EXPECTED)})
        for (function, _), target in zip(before[0], update.EXPECTED):
            module, owner, name = target.split('.')
            self.assertIs(getattr(getattr(self.modules[module], owner), name), function)
            self.assertEqual(update.signature(function), update.EXPECTED[target][1])
        self.assertEqual(before[3:], self.state()[3:])
        self.assertEqual(update.signature(self.transfer.assert_settings), update.ASSERT_SIGNATURE)
        self.assertTrue(issubclass(self.modules['codex_account_transfer'].TransferSettingsConflict, ValueError))
        after = self.state()
        self.assertEqual(update.apply(self.runtime), {'status': 'already_applied', 'baseCommit': '5217bb9'})
        self.assertEqual(after, self.state())

    def test_known_baseline_preserves_captured_callbacks_and_is_idempotent(self):
        self.apply_and_check_callbacks()

    def test_mixed_current_transfer_and_baseline_runtime_preserves_existing_guard(self):
        for target in update.EXPECTED:
            if target.startswith('codex_account_transfer.'):
                setattr(self.transfer, target.split('.')[-1], self.compile(target, current=True))
        self.transfer.assert_settings = self.compile('codex_account_transfer.AccountTransfers.assert_settings', current=True)
        error = type('TransferSettingsConflict', (ValueError,), {'__module__': 'codex_account_transfer'})
        self.modules['codex_account_transfer'].TransferSettingsConflict = error
        guard = self.transfer.assert_settings
        self.apply_and_check_callbacks()
        self.assertIs(self.transfer.assert_settings, guard)
        self.assertIs(self.modules['codex_account_transfer'].TransferSettingsConflict, error)

    def test_unknown_method_code_defaults_and_namespace_refuse_without_changes(self):
        for target in update.EXPECTED:
            module, owner, name = target.split('.')
            cls = getattr(self.modules[module], owner)
            original = getattr(cls, name)
            for kind in ('code', 'defaults', 'namespace'):
                function = self.compile(target, namespace=dict(vars(self.modules[module])) if kind == 'namespace' else None)
                if kind == 'code':
                    function.__code__ = function.__code__.replace(co_consts=function.__code__.co_consts + ('unreviewed',))
                if kind == 'defaults':
                    function.__defaults__ = ('unreviewed',)
                setattr(cls, name, function)
                before = self.state()
                with self.subTest(target=target, kind=kind), self.assertRaisesRegex(RuntimeError, 'Unknown'):
                    update.apply(self.runtime)
                self.assertEqual(before, self.state())
            setattr(cls, name, original)

    def test_instance_method_overrides_refuse(self):
        for target in update.EXPECTED:
            name = target.split('.')[-1]
            instance = self.runtime._account_transfers if target.startswith('codex_account_transfer.') else self.runtime
            setattr(instance, name, lambda *args: None)
            before = self.state()
            with self.subTest(target=target), self.assertRaisesRegex(RuntimeError, 'override'):
                update.apply(self.runtime)
            self.assertEqual(before, self.state())
            delattr(instance, name)

    def test_added_guard_namespace_and_store_override_refuse(self):
        self.transfer.assert_settings = self.compile('codex_account_transfer.AccountTransfers.assert_settings',
            current=True, namespace=dict(vars(self.modules['codex_account_transfer'])))
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'guard'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())
        delattr(self.transfer, 'assert_settings')
        self.runtime._account_transfers.assert_settings = lambda *args: None
        before = self.state()
        with self.assertRaisesRegex(RuntimeError, 'override'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_source_paths_hashes_and_loaded_helper_refuse(self):
        before = self.state()
        for module in self.modules.values():
            with patch.object(module, '__file__', '/unknown/' + module.__name__ + '.py'), \
                    self.assertRaisesRegex(RuntimeError, 'location'):
                update.apply(self.runtime)
        for constant in ('TRANSFER_SHA', 'ACTION_SHA'):
            with patch.object(update, constant, '0' * 64), self.assertRaisesRegex(RuntimeError, 'Unreviewed'):
                update.apply(self.runtime)
        with patch.dict(sys.modules, {'codex_native_action_settings': ModuleType('codex_native_action_settings')}), \
                self.assertRaisesRegex(RuntimeError, 'loaded before'):
            update.apply(self.runtime)
        self.assertEqual(before, self.state())

    def test_mid_install_failure_restores_callbacks_and_removes_added_names(self):
        before = self.state()
        target = self.owner.native_action
        armed = [True]
        def audit(event, args):
            if armed[0] and event == 'object.__setattr__' and args[0] is target and args[1] == '__code__':
                armed[0] = False
                raise RuntimeError('Controlled execution update failure')
        sys.addaudithook(audit)
        try:
            with self.assertRaisesRegex(RuntimeError, 'Controlled execution'):
                update.apply(self.runtime)
        finally:
            armed[0] = False
        self.assertEqual(before, self.state())

    def test_closed_and_busy_runtime_refuse_and_release_only_acquired_lock(self):
        before = self.state()
        self.runtime.closed = True
        with self.assertRaisesRegex(RuntimeError, 'closed'):
            update.apply(self.runtime)
        self.runtime.closed = False
        runtime = self.runtime
        class Lock:
            close_on_acquire = False
            released = False
            def acquire(self, *, timeout):
                self.timeout = timeout
                runtime.closed = self.close_on_acquire
                return self.close_on_acquire
            def release(self):
                self.released = True
        self.runtime.lock = Lock()
        with self.assertRaisesRegex(RuntimeError, 'busy'):
            update.apply(self.runtime)
        self.assertEqual(self.runtime.lock.timeout, 10)
        self.assertFalse(self.runtime.lock.released)
        self.runtime.lock.close_on_acquire = True
        with self.assertRaisesRegex(RuntimeError, 'closed'):
            update.apply(self.runtime)
        self.assertTrue(self.runtime.lock.released)
        self.assertEqual(before, self.state())


if __name__ == '__main__':
    unittest.main()
