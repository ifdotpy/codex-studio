#!/usr/bin/env python3
"""Worker base selection and stale-base warnings use isolated Git repos."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()


import copy
import importlib.util
import subprocess
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    'worker_base_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class WorkerBase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='worker-base-')
        self.root = Path(self.tmp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.test')
        self.commit('one')
        self.first = self.git('rev-parse', 'HEAD')
        self.git('tag', 'base-v1')
        self.commit('two')
        self.commit('three')
        self.latest = self.git('rev-parse', 'HEAD')
        self.runtime = f.ControlledRuntime(self.root, f.f.FakeServer)
        self.runtime.catalog = lambda account='default': copy.deepcopy(f.CATALOG)
        self.lead = self.runtime.new_lead({'cwd': str(self.root)})

    def tearDown(self):
        self.runtime.close()
        self.tmp.cleanup()

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True,
                              capture_output=True, text=True, timeout=30).stdout.strip()

    def commit(self, name):
        (self.repo / 'tracked.txt').write_text(name + '\n')
        self.git('add', 'tracked.txt')
        self.git('commit', '-qm', name)

    def spawn(self, key, **spec):
        return self.runtime.spawn_agents(self.lead, {'agents': [{
            'name': key, 'prompt': 'Inspect the repository', 'cwd': str(self.repo), **spec,
        }]}, key)['agents'][0]

    def initial(self, worker):
        with self.runtime.db() as db:
            return db.execute('SELECT text FROM runtime_events WHERE id=?',
                               (worker + ':initial',)).fetchone()[0]

    def test_explicit_base_ref_resolves_to_immutable_commit(self):
        receipt = self.spawn('explicit', base_ref='main')
        self.assertEqual(receipt['baseRef'], 'main')
        self.assertEqual(receipt['baseCommit'], self.latest)
        worker = self.runtime.agent(receipt['id'])
        self.assertEqual(worker['workerBaseCommit'], self.latest)
        commit_receipt = self.spawn('explicit-commit', base_ref=self.first)
        self.assertEqual(commit_receipt['baseCommit'], self.first)

    def test_unset_project_setting_uses_repository_head(self):
        receipt = self.spawn('head-default')
        self.assertEqual((receipt['baseRef'], receipt['baseCommit']), ('HEAD', self.latest))
        self.assertNotIn('baseBehindMain', receipt)

    def test_project_default_warns_in_receipt_and_first_input(self):
        saved = self.runtime.projects({'action': 'set_worker_base', 'path': str(self.repo),
                                       'base_ref': 'base-v1', 'expected_revision': 0})
        self.assertEqual(saved['workerBaseRef'], 'base-v1')
        receipt = self.spawn('project-default')
        self.assertEqual((receipt['baseRef'], receipt['baseCommit']), ('base-v1', self.first))
        self.assertEqual(receipt['baseBehindMain'], 2)
        self.assertEqual(receipt['baseMainRef'], 'main')
        self.assertIn('2 commits behind main', receipt['baseWarning'])
        text = self.initial(receipt['id'])
        self.assertIn('Commit ' + self.first, text)
        self.assertIn('2 commits behind main', text)

    def test_invalid_base_ref_rejects_batch_without_workers(self):
        before = len(self.runtime.team(self.lead['id'])['agents'])
        with self.assertRaisesRegex(ValueError, 'does not resolve to a commit'):
            self.spawn('invalid', base_ref='missing/branch')
        self.assertEqual(len(self.runtime.team(self.lead['id'])['agents']), before)


if __name__ == '__main__':
    unittest.main(verbosity=2)
