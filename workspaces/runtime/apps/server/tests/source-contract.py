#!/usr/bin/env python3
"""Source inspection must not execute imports, decorators, or defaults."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_source import digest, signature, source_function


class SourceContract(unittest.TestCase):
    def test_module_and_decorator_are_not_executed(self):
        source = '''must_not_execute()
class Current:
    @staticmethod
    def value(argument=3, *, enabled=True):
        return argument if enabled else 0
'''
        function, static = source_function(source, ('Current', 'value'), {})
        self.assertTrue(static)
        self.assertEqual(function(), 3)
        self.assertEqual(function(enabled=False), 0)

    def test_dynamic_defaults_and_unknown_decorators_are_rejected(self):
        for source in ('def value(argument=load_secret()): return argument',
                       '@unsafe\ndef value(): return 1'):
            with self.assertRaises((ValueError, RuntimeError)):
                source_function(source, ('value',), {})

    def test_signature_ignores_filename_and_lines_but_includes_behavior(self):
        source = 'def value(argument=3, *, enabled=True): return argument if enabled else 0'
        first, _ = source_function(source, ('value',), {}, filename='first.py')
        same, _ = source_function('\n\n' + source, ('value',), {}, filename='second.py')
        changed, _ = source_function(source.replace('else 0', 'else 1'), ('value',), {})
        self.assertEqual(signature(first), signature(same))
        self.assertNotEqual(signature(first), signature(changed))
        changed.__code__ = first.__code__
        changed.__kwdefaults__ = {'enabled': False}
        self.assertNotEqual(signature(first), signature(changed))

    def test_nested_function_preserves_supplied_closure(self):
        value = object()
        cell = (lambda: value).__closure__[0]
        function, static = source_function(
            'def owner(value):\n    def result(): return value\n    return result',
            ('owner', 'result'), {}, closure=(cell,))
        self.assertFalse(static)
        self.assertIs(function(), value)

    def test_digest_is_key_order_independent(self):
        self.assertEqual(digest({'a': 1, 'b': 2}), digest({'b': 2, 'a': 1}))


if __name__ == '__main__':
    unittest.main()
