#!/usr/bin/env python3
"""Known live code updates preserve captured callbacks and reject unknown state."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
from types import FunctionType, ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_active_task_update as update
import codex_runtime
import codex_workspace
import codex_agent_management

spec = importlib.util.spec_from_file_location('active_fixture', Path(__file__).with_name('active-task-history-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class UpdateContract(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.old_source = {name: subprocess.run(
            ['git', 'show', update._BASE_COMMIT + ':scripts/' + name + '.py'], cwd=ROOT,
            check=True, text=True, capture_output=True,
        ).stdout for name in {entry[0] for entry in update._TARGETS}}

    def setUp(self):
        self.modules = {}
        for name in self.old_source:
            module = ModuleType(name)
            module.__dict__.update(vars(sys.modules[name]))
            module.__dict__.pop('active_task_records', None)
            self.modules[name] = module
        class Live(fixture.Fixture):
            pass
        self.runtime = Live()
        self.methods = {}
        for module_name, class_name, name in update._TARGETS:
            module = self.modules[module_name]
            function = update.compile_function(self.old_source[module_name], class_name, name,
                                               vars(module), '/installed/old/' + module_name + '.py')
            self.assertEqual(update.signature(function), update._EXPECTED[name]['old'])
            self.methods[name] = function
            if class_name:
                setattr(Live, name, function)
            else:
                setattr(module, name, function)
        self.module_patch = patch.dict(sys.modules, self.modules)
        self.module_patch.start()

    def tearDown(self):
        self.module_patch.stop()
        self.runtime.connection.close()

    def codes(self):
        return {name: function.__code__ for name, function in self.methods.items()}

    def test_real_baseline_updates_captured_bound_methods_and_imported_callers(self):
        captured_disconnect = self.runtime.disconnected
        captured_team = self.runtime.team
        imported_blockers = self.modules['codex_agent_management']._blockers
        previous_init = type(self.runtime).__init__
        previous_connection = self.runtime.connection
        sql = []
        self.runtime.connection.set_trace_callback(sql.append)
        result = update.apply(self.runtime)
        self.assertEqual(result['status'], 'applied')
        self.assertEqual(sql, [], 'Applying function code must not write or query runtime state')
        self.assertIs(type(self.runtime).__init__, previous_init)
        self.assertIs(self.runtime.connection, previous_connection)
        for _, _, name in update._TARGETS:
            self.assertEqual(update.signature(self.methods[name]), update._EXPECTED[name]['new'])
        self.assertIs(captured_disconnect.__func__, self.methods['disconnected'])
        self.assertIs(imported_blockers, self.modules['codex_agent_management']._blockers)
        helper = vars(self.modules['codex_workspace'])['active_task_records']
        self.assertIs(helper, vars(self.modules['codex_runtime'])['active_task_records'])
        self.assertIs(helper.__globals__, vars(self.modules['codex_workspace']))
        original_loads = json.loads
        def decode(value, *args, **kwargs):
            if isinstance(value, str) and 'historical-marker' in value:
                raise AssertionError('Captured old callback still decoded historical tasks')
            return original_loads(value, *args, **kwargs)
        with patch('json.loads', side_effect=decode):
            captured_disconnect('one', 'connection-one')
            rows = helper(self.runtime.connection, ('running', 'lost'))
            self.assertEqual({row['id']: row['status'] for row in rows},
                             {'running-first': 'lost', 'running-second': 'running'})
            blockers = imported_blockers(self.runtime, self.runtime.connection,
                                          self.runtime.agent('first', self.runtime.connection))
            self.assertEqual(next(row for row in blockers if row['kind'] == 'background_tasks')['count'], 3)
        self.runtime.snapshot = lambda *, include_work=True: (
            (_ for _ in ()).throw(AssertionError('Unused work loaded')) if include_work else
            {'agents': [{'id': 'first', 'rootId': 'first'}], 'monitors': []})
        self.runtime.agent = lambda key: {'id': key}
        self.runtime.worker_defaults = lambda agent: {'model': 'same'}
        self.assertEqual(captured_team('first')['workerDefaults'], {'model': 'same'})
        codes = self.codes()
        self.assertEqual(update.apply(self.runtime)['status'], 'already_applied')
        self.assertEqual(codes, self.codes())

    def test_unknown_live_code_refuses_before_any_change(self):
        def unknown(self, root):
            return {'unknown': root}
        type(self.runtime).team = unknown
        before = self.codes()
        with self.assertRaisesRegex(RuntimeError, 'namespace|Unknown live'):
            update.apply(self.runtime)
        self.assertEqual(before, self.codes())
        self.assertTrue(all('active_task_records' not in vars(module) for module in self.modules.values()))

    def test_unknown_helper_refuses_before_any_change(self):
        self.modules['codex_workspace'].active_task_records = lambda db: []
        before = self.codes()
        with self.assertRaisesRegex(RuntimeError, 'Unknown live task helper'):
            update.apply(self.runtime)
        self.assertEqual(before, self.codes())
        self.assertNotIn('active_task_records', vars(self.modules['codex_runtime']))

    def test_lock_timeout_is_bounded_and_preserves_functions(self):
        calls = []
        class Busy:
            def acquire(self, *, timeout):
                calls.append(timeout)
                return False
            def release(self):
                raise AssertionError('Unowned lock released')
        self.runtime.lock = Busy()
        before = self.codes()
        with self.assertRaisesRegex(RuntimeError, 'Runtime remains busy'):
            update.apply(self.runtime)
        self.assertEqual(calls, [10])
        self.assertEqual(before, self.codes())
        self.assertNotIn('active_task_records', vars(self.modules['codex_runtime']))

    def test_unreviewed_replacement_source_refuses_before_any_change(self):
        original = Path.read_text
        def changed(path, *args, **kwargs):
            source = original(path, *args, **kwargs)
            if path.name == 'codex_runtime.py':
                source = source.replace('state = self.snapshot(include_work=False)', 'state = self.snapshot(include_work=True)')
            return source
        before = self.codes()
        with patch.object(Path, 'read_text', changed), self.assertRaisesRegex(RuntimeError, 'Unreviewed replacement source'):
            update.apply(self.runtime)
        self.assertEqual(before, self.codes())

    def test_filename_and_line_numbers_do_not_change_signatures(self):
        source = self.old_source['codex_runtime']
        first = update.compile_function(source, 'Runtime', 'conversation_settings', {}, '/one.py')
        second = update.compile_function('\n\n' + source, 'Runtime', 'conversation_settings', {}, '/two.py')
        self.assertEqual(update.signature(first), update.signature(second))
        self.assertNotEqual(first.__code__.co_firstlineno, second.__code__.co_firstlineno)
        # Canonical ordering also covers compiler-created set constants and nested code.
        self.assertEqual(update.signature(first), update._EXPECTED['conversation_settings']['old'])


if __name__ == '__main__':
    unittest.main()
