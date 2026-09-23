#!/usr/bin/env python3
"""A spawned worker can own a task board item from its first turn."""
import copy
import subprocess
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('spawn_task_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class SpawnTask(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.rt = f.ControlledRuntime(Path(self.tmp.name), f.f.FakeServer)
        self.rt.catalog = lambda account='default': copy.deepcopy(f.CATALOG)
        self.lead = self.rt.new_lead({'cwd': self.tmp.name})
        self.task = self.rt.work_action(self.lead['id'], {'action': 'create', 'title': 'Count folders'})

    def tearDown(self):
        self.rt.close()
        self.tmp.cleanup()

    def count(self, table):
        with self.rt.db() as db:
            return db.execute('SELECT count(*) FROM runtime_' + table).fetchone()[0]

    def work(self):
        with self.rt.db() as db:
            return next(w for w in self.rt.records(db, 'work') if w['id'] == self.task['id'])

    def initial(self, agent_id):
        with self.rt.db() as db:
            return db.execute('SELECT text FROM runtime_events WHERE id=?', (agent_id + ':initial',)).fetchone()[0]

    def test_spawn_assigns_task_and_worker_can_submit(self):
        value = self.rt.spawn_agents(self.lead, {'agents': [
            {'name': 'Reviewer', 'prompt': 'Count folders', 'role': 'reviewer', 'task_id': self.task['id']}]}, 'spawn-task')
        worker = value['agents'][0]
        self.assertEqual(worker['taskId'], self.task['id'])
        task = self.work()
        self.assertEqual(task['owner'], worker['id'])
        self.assertEqual(self.rt.agent(worker['id'])['prompt'], 'Count folders')
        text = self.initial(worker['id'])
        self.assertIn('[Studio task ' + self.task['id'] + '] Count folders', text)
        self.assertIn('action=submit task_id=' + self.task['id'], text)
        self.rt.work_action(worker['id'], {'action': 'submit', 'task_id': self.task['id'], 'result': '29 folders',
                                           'checks': 'ls', 'revision': 'HEAD'}, actor=worker['id'])
        self.rt.work_action(self.lead['id'], {'action': 'accept', 'task_id': self.task['id'], 'result': 'Verified'})

    def test_invalid_assignment_creates_no_worker(self):
        owned = self.rt.work_action(self.lead['id'], {'action': 'create', 'title': 'Owned', 'owner': self.lead['id']})
        cases = [([{'name': 'A', 'prompt': 'x', 'task_id': 'missing'}], 'Unknown task_id'),
                 ([{'name': 'A', 'prompt': 'x', 'task_id': owned['id']}], 'owned'),
                 ([{'name': 'A', 'prompt': 'x', 'task_id': 7}], 'task_id must'),
                 ([{'name': 'A', 'prompt': 'x', 'task_id': self.task['id']},
                   {'name': 'B', 'prompt': 'y', 'task_id': self.task['id']}], 'one worker')]
        agents, events = self.count('agents'), self.count('events')
        for index, (batch, error) in enumerate(cases):
            with self.assertRaisesRegex(ValueError, error):
                self.rt.spawn_agents(self.lead, {'agents': batch}, 'bad-' + str(index))
        self.assertEqual((self.count('agents'), self.count('events')), (agents, events))
        task = self.work()
        self.assertIsNone(task['owner'])


    def test_only_the_lead_can_create_agents(self):
        worker = self.rt.spawn_agents(self.lead, {'agents': [{'name': 'Worker', 'prompt': 'Work'}]}, 'lead-spawn')['agents'][0]
        worker = self.rt.agent(worker['id'])
        agents, events = self.count('agents'), self.count('events')
        with self.assertRaisesRegex(ValueError, 'Only the lead can create agents'):
            self.rt.spawn_agents(worker, {'agents': [{'name': 'Nested', 'prompt': 'Work'}]}, 'nested-spawn')
        self.assertEqual((self.count('agents'), self.count('events')), (agents, events))
        names = lambda actor: {d['name'] for d in self.rt.tool_definitions(actor)}
        self.assertNotIn('orchestration_spawn', names(worker))
        self.assertIn('orchestration_spawn', names(self.rt.agent(self.lead['id'])))


    def spawn(self, key, **spec):
        return self.rt.spawn_agents(self.lead, {'agents': [{'name': 'W', 'prompt': 'Work', **spec}]}, key)['agents'][0]

    def test_worker_outside_git_works_in_place_with_warning(self):
        plain = Path(self.tmp.name) / 'plain'
        plain.mkdir()
        value = self.spawn('plain', cwd=str(plain))
        self.assertEqual((value['cwd'], value['worktree']), (str(plain.resolve()), False))
        self.assertIn('not in a git repository', value['warning'])
        stored = self.rt.agent(value['id'])
        self.assertFalse(stored['worktree'])
        self.assertEqual(stored['worktreeWarning'], value['warning'])

    def test_worker_in_git_subfolder_gets_worktree_without_warning(self):
        repo = Path(self.tmp.name) / 'repo'
        (repo / 'sub').mkdir(parents=True)
        subprocess.run(['git', 'init', '-q', str(repo)], check=True)
        value = self.spawn('repo', cwd='repo/sub')
        self.assertEqual(value['cwd'], str((repo / 'sub').resolve()))
        self.assertTrue(value['worktree'])
        self.assertNotIn('warning', value)
        reviewer = self.spawn('reviewer', cwd=str(Path(self.tmp.name) / 'repo'), role='reviewer')
        self.assertFalse(reviewer['worktree'])
        self.assertNotIn('warning', reviewer)

    def test_default_folder_is_the_lead_folder_and_missing_folder_creates_nothing(self):
        self.assertEqual(self.spawn('default')['cwd'], str(Path(self.lead['cwd']).resolve()))
        agents = self.count('agents')
        for index, bad in enumerate(['missing', '', 7]):
            with self.assertRaisesRegex(ValueError, 'cwd must be'):
                self.spawn('bad-cwd-' + str(index), cwd=bad)
        self.assertEqual(self.count('agents'), agents)

    def test_failed_or_deleted_worker_releases_unfinished_work(self):
        second = self.rt.work_action(self.lead['id'], {'action': 'create', 'title': 'Second'})
        first = self.spawn('owner-a', task_id=self.task['id'])
        other = self.spawn('owner-b', task_id=second['id'])
        self.rt.work_action(other['id'], {'action': 'submit', 'task_id': second['id'], 'result': 'Done',
                                          'checks': 'ran', 'revision': 'HEAD'}, actor=other['id'])
        with self.rt.lock, self.rt.db() as db:
            for agent_id in (first['id'], other['id']):
                a = self.rt.agent(agent_id, db)
                a.update(status='failed', error='Native process exited')
                self.rt.put(db, 'agents', a)
            released = self.rt.release_failed_work(db, self.rt.records(db, 'agents'))
            again = self.rt.release_failed_work(db, self.rt.records(db, 'agents'))
        self.assertEqual((released, again), ([self.task['id']], []))
        task = self.work()
        self.assertEqual((task['owner'], task['status']), (None, 'ready'))
        self.assertEqual(task['releases'][0]['reason'], 'Native process exited')
        with self.rt.db() as db:
            reviewed = next(w for w in self.rt.records(db, 'work') if w['id'] == second['id'])
            events = [r[0] for r in db.execute("SELECT kind FROM runtime_events WHERE agent=? AND kind='work_released'",
                                              (self.lead['id'],))]
        self.assertEqual((reviewed['owner'], reviewed['status']), (other['id'], 'review'))
        self.assertEqual(events, ['work_released'])
        claimed = self.spawn('owner-c', task_id=self.task['id'])
        self.assertEqual(self.work()['owner'], claimed['id'])


    def test_prepare_works_in_place_when_the_folder_left_git(self):
        repo = Path(self.tmp.name) / 'gone'
        repo.mkdir()
        subprocess.run(['git', 'init', '-q', str(repo)], check=True)
        value = self.spawn('gone', cwd=str(repo))
        self.assertTrue(value['worktree'])
        subprocess.run(['rm', '-rf', str(repo / '.git')], check=True)
        prepared = self.rt.prepare(self.rt.agent(value['id']))
        stored = self.rt.agent(value['id'])
        self.assertTrue(prepared)
        self.assertFalse(stored['worktree'])
        self.assertEqual(stored['cwd'], str(repo.resolve()))
        self.assertIn('not in a git repository', stored['worktreeWarning'])


if __name__ == '__main__':
    unittest.main(verbosity=2)
