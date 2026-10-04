#!/usr/bin/env python3
"""Archive live updates reject changed guards and preserve imported callers."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import importlib.util
from pathlib import Path
import sys
import tempfile
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
import codex_agent_archive_update as update
from codex_source import signature, source_function

spec = importlib.util.spec_from_file_location('archive_contract', ROOT / 'tests/agent-management-contract.py')
contract = importlib.util.module_from_spec(spec)
spec.loader.exec_module(contract)


@contextmanager
def reviewed_fixture():
    with tempfile.TemporaryDirectory(prefix='studio-archive-update-') as folder:
        scripts = Path(folder)
        management_path = scripts / 'codex_agent_management.py'
        management_path.write_bytes((ROOT / 'scripts/codex_agent_management.py').read_bytes())
        management = ModuleType('codex_agent_management')
        management.__file__ = str(management_path)
        exec(compile(management_path.read_bytes(), str(management_path), 'exec'), vars(management))
        for name in update.BEFORE:
            old_source = f"def {name}(*args, **kwargs):\n    return 'old'\n"
            old, _ = source_function(old_source, [name], vars(management))
            current = getattr(management, name)
            current.__code__ = old.__code__
            current.__defaults__ = old.__defaults__
            current.__kwdefaults__ = old.__kwdefaults__
        before = {name: signature(getattr(management, name)) for name in update.BEFORE}
        del management._archive_record
        runtime_module = ModuleType('codex_runtime')
        runtime_module.__file__ = str(scripts / 'codex_runtime.py')
        runtime_module.Runtime = type('Runtime', (contract.Store,), {})
        runtime = runtime_module.Runtime(str(scripts / 'state.sqlite'))
        with runtime.db() as db:
            worker = runtime.agent('worker', db)
            worker.update(threadId=None, worktreeReady=False)
            runtime.put(db, 'agents', worker)
        modules = {'codex_runtime': runtime_module, 'codex_agent_management': management}
        with (patch.dict(sys.modules, modules),
              patch.object(update, '__file__', str(scripts / 'codex_agent_archive_update.py')),
              patch.object(update, 'BEFORE', before)):
            yield runtime, management, management_path


class ArchiveUpdateContract(unittest.TestCase):
    def test_imported_caller_archives_and_replays_after_update(self):
        with reviewed_fixture() as (runtime, management, _):
            caller = management.manage_agent
            self.assertEqual(caller(None, None, {}), 'old')
            self.assertEqual(update.apply(runtime)['status'], 'applied')
            self.assertIs(caller, management.manage_agent)
            args = {'action':'archive', 'agent_id':'worker', 'reason':'Reviewed'}
            result = caller(runtime, 'lead', args, 1)
            self.assertEqual(result['status'], 'archived')
            self.assertFalse(result['agent']['agentArchive']['cleanupPending'])
            self.assertTrue(caller(runtime, 'lead', args, 1)['replayed'])
            self.assertEqual(runtime.calls, [])
            self.assertEqual(update.apply(runtime)['status'], 'already_applied')

    def test_changed_guard_rejects_update_before_any_function_changes(self):
        with reviewed_fixture() as (runtime, management, _):
            management._authorize = lambda *args: None
            with self.assertRaisesRegex(RuntimeError, 'archive guard differs: _authorize'):
                update.apply(runtime)
            self.assertEqual(management.manage_agent(None, None, {}), 'old')
            self.assertFalse(hasattr(management, '_archive_record'))

    def test_changed_source_rejects_update_before_any_function_changes(self):
        with reviewed_fixture() as (runtime, management, path):
            path.write_bytes(path.read_bytes() + b'\n# changed\n')
            with self.assertRaisesRegex(RuntimeError, 'reviewed archive source differs'):
                update.apply(runtime)
            self.assertEqual(management.manage_agent(None, None, {}), 'old')
            self.assertFalse(hasattr(management, '_archive_record'))


if __name__ == '__main__':
    unittest.main()
