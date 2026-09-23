#!/usr/bin/env python3
"""A spawned worker can own a task board item from its first turn."""
import copy
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


if __name__ == '__main__':
    unittest.main(verbosity=2)
