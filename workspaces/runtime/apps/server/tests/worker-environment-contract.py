#!/usr/bin/env python3
"""Worker environment settings use private state and never start a VM."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    'worker_environment_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)

from codex_worker_environment import select
from codex_sync_entities import project as project_entity


class WorkerEnvironment(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='worker-environment-')
        self.root = Path(self.tmp.name)
        self.project = self.root / 'project'
        self.nested = self.project / 'nested'
        self.nested.mkdir(parents=True)
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.runtime.catalog = lambda account='default': copy.deepcopy(f.CATALOG)

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def save(self, environment, revision=0, path=None):
        return self.runtime.projects({
            'action': 'set_worker_environment', 'path': str(path or self.project),
            'environment': environment, 'expected_revision': revision})

    def test_native_environment_and_removed_linux_override(self):
        self.assertEqual(select(self.runtime, {}, str(self.project)), 'host')
        self.assertEqual(select(self.runtime, {'environment': 'host'}, str(self.nested)), 'host')
        for role in ('implementer', 'reviewer'):
            with self.assertRaisesRegex(ValueError, 'Choose a layr chat'):
                select(self.runtime, {'role': role, 'environment': 'linux'}, str(self.project))
        with self.assertRaisesRegex(ValueError, 'Choose a layr chat'):
            self.save('linux')
        self.assertEqual(self.runtime.projects()['items'], [])

    def test_stored_linux_project_setting_does_not_route_native_chat(self):
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, 'projects', {'id': str(self.project), 'path': str(self.project),
                'name': 'Project', 'workerEnvironment': 'linux'})
        self.assertEqual(select(self.runtime, {}, str(self.nested)), 'host')

    def test_invalid_values_leave_projects_unchanged(self):
        for environment in (None, '', 'darwin', False, [], {}):
            with self.assertRaises(ValueError):
                self.save(environment)
        self.assertEqual(self.runtime.projects()['items'], [])
        for revision in (None, -1, False, '0'):
            with self.assertRaises(ValueError):
                self.save('linux', revision)
        self.assertEqual(self.runtime.projects()['items'], [])

    def test_legacy_host_setting_receipt_is_still_exact(self):
        saved = self.save('host')
        self.assertEqual(saved['workerEnvironmentRevision'], 1)
        self.assertEqual(self.save('host'), saved)
        with self.assertRaisesRegex(ValueError, 'changed'):
            self.save('host', 3)
        projected = project_entity('project', saved)
        self.assertEqual(projected['workerEnvironment'], 'host')


if __name__ == '__main__':
    unittest.main(verbosity=2)
