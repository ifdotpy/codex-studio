#!/usr/bin/env python3
import importlib.util
from pathlib import Path
import sys
import types
import unittest
sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_safety_update import apply
from codex_efficiency_update import fingerprint
spec = importlib.util.spec_from_file_location('safety', Path(__file__).with_name('native-safety-contract.py'))
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)

class Update(m.Safety):
    # Run the behavior suite on the exact functions an update would install.
    def setUp(self):
        super().setUp()
        from codex_safety_update import load_source
        errors = load_source('codex_native_errors')
        scope = self.runtime.notification.__func__.__globals__.copy()
        original = self.runtime.notification.__func__
        # Force fresh function bindings with the same reviewed bytecode.
        scope['consume_native_notification'] = errors.consume_native_notification
        scope['advance_native_status'] = errors.advance_native_status
        self.runtime.notification = types.MethodType(types.FunctionType(original.__code__, scope,
            original.__name__, original.__defaults__), self.runtime)
        self.assertEqual(apply(self.runtime)['status'], 'already_applied')

    def test_unknown_method_is_rejected_before_changes(self):
        original = self.runtime.notification
        self.runtime.native_action = types.MethodType(lambda self, key, action: None, self.runtime)
        with self.assertRaisesRegex(RuntimeError, 'Unreviewed live handler'):
            apply(self.runtime)
        self.assertIs(self.runtime.notification, original)

if __name__ == '__main__': unittest.main()
