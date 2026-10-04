#!/usr/bin/env python3
"""The task and scheduler patch preserves callbacks and validates before changes."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_runtime
import codex_task_scheduler_update as update
from codex_source import signature, source_function


@contextmanager
def fixture():
    with tempfile.TemporaryDirectory(prefix='studio-task-scheduler-update-') as folder:
        path = Path(folder) / 'codex_runtime.py'
        reviewed = subprocess.check_output(['git', 'show', '7ce4b11c:scripts/codex_runtime.py'], cwd=ROOT)
        path.write_bytes(reviewed)
        module = ModuleType('codex_runtime')
        vars(module).update(vars(codex_runtime))
        module.__file__ = str(path)
        module.Runtime = type('Runtime', (codex_runtime.Runtime,), {})
        # This released patch validates its original dependency code.
        for name in update.DEPENDENCIES:
            guard, _ = source_function(reviewed, ['Runtime', name], vars(module), '<reviewed-7ce4b11c>')
            setattr(module.Runtime, name, guard)
        before = subprocess.check_output(['git', 'show', '36f1143d:scripts/codex_runtime.py'], cwd=ROOT)
        for name in update.FUNCTIONS:
            old, _ = source_function(before, ['Runtime', name], vars(module), '<baseline-36f1143d>')
            setattr(module.Runtime, name, old)
        runtime = module.Runtime.__new__(module.Runtime)
        runtime.lock = threading.RLock()
        runtime.closed = False
        runtime.changed = threading.Event()
        runtime._scheduler_agent_roster = ('obsolete fixture',)
        with (patch.dict(sys.modules, {'codex_runtime': module}),
              patch.object(update, '__file__', str(path.with_name('codex_task_scheduler_update.py')))):
            yield runtime, module, path


class TaskSchedulerUpdateContract(unittest.TestCase):
    def test_retained_callbacks_change_and_cached_roster_is_cleared(self):
        with fixture() as (runtime, module, _):
            callbacks = {name: getattr(runtime, name) for name in update.FUNCTIONS}
            self.assertEqual(update.apply(runtime)['status'], 'applied')
            for name, callback in callbacks.items():
                self.assertIs(callback.__func__, getattr(module.Runtime, name))
                self.assertEqual(signature(callback.__func__), update.FUNCTIONS[name][1])
            self.assertNotIn('_scheduler_agent_roster', vars(runtime))
            self.assertTrue(runtime.changed.is_set())
            self.assertEqual(update.apply(runtime)['status'], 'already_applied')

    def test_changed_guard_rejects_before_any_function_changes(self):
        with fixture() as (runtime, module, _):
            module.Runtime.item = lambda *args, **kwargs: None
            with self.assertRaisesRegex(RuntimeError, 'guard differs: item'):
                update.apply(runtime)
            for name, accepted in update.FUNCTIONS.items():
                self.assertEqual(signature(getattr(module.Runtime, name)), accepted[0])
            self.assertFalse(runtime.changed.is_set())

    def test_source_change_rejects_before_any_function_changes(self):
        with fixture() as (runtime, module, path):
            path.write_bytes(path.read_bytes() + b'\n# changed\n')
            with self.assertRaisesRegex(RuntimeError, 'source differs'):
                update.apply(runtime)
            for name, accepted in update.FUNCTIONS.items():
                self.assertEqual(signature(getattr(module.Runtime, name)), accepted[0])
            self.assertFalse(runtime.changed.is_set())


if __name__ == '__main__':
    unittest.main()
