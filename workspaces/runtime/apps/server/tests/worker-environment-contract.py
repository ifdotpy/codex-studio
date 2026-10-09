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

    def test_host_default_and_explicit_override(self):
        self.assertEqual(select(self.runtime, {}, str(self.project)), 'host')
        self.save('linux')
        self.assertEqual(select(self.runtime, {}, str(self.nested)), 'linux')
        self.assertEqual(select(self.runtime, {'environment': 'host'}, str(self.nested)), 'host')
        self.assertEqual(select(self.runtime, {'environment': 'linux'}, str(self.root)), 'linux')

    def test_nearest_project_can_override_linux_with_host(self):
        self.save('linux')
        self.save('host', path=self.nested)
        self.assertEqual(select(self.runtime, {}, str(self.nested)), 'host')

    def test_reviewer_keeps_host_and_rejects_explicit_linux(self):
        self.save('linux')
        self.assertEqual(select(self.runtime, {'role': 'reviewer'}, str(self.project)), 'host')
        with self.assertRaisesRegex(ValueError, 'Reviewers must use the host'):
            select(self.runtime, {'role': 'reviewer', 'environment': 'linux'}, str(self.project))

    def test_invalid_values_leave_projects_unchanged(self):
        for environment in (None, '', 'darwin', False, [], {}):
            with self.assertRaises(ValueError):
                self.save(environment)
        self.assertEqual(self.runtime.projects()['items'], [])
        for revision in (None, -1, False, '0'):
            with self.assertRaises(ValueError):
                self.save('linux', revision)
        self.assertEqual(self.runtime.projects()['items'], [])

    def test_retry_is_exact_and_stale_change_is_rejected(self):
        saved = self.save('linux')
        self.assertEqual(saved['workerEnvironmentRevision'], 1)
        self.assertEqual(self.save('linux'), saved)
        with self.assertRaisesRegex(ValueError, 'changed'):
            self.save('host')
        self.assertEqual(self.save('host', 1)['workerEnvironmentRevision'], 2)

    def test_settings_are_projected_and_preserved_by_account_write(self):
        saved = self.save('linux')
        projected = project_entity('project', saved)
        self.assertEqual(projected['workerEnvironment'], 'linux')
        self.assertEqual(projected['workerEnvironmentRevision'], 1)
        changed = self.runtime.projects({'action': 'set_account', 'path': str(self.project),
                                        'account_key': 'default', 'expected_revision': 0})
        self.assertEqual(changed['workerEnvironment'], 'linux')


if __name__ == '__main__':
    unittest.main(verbosity=2)
