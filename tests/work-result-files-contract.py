#!/usr/bin/env python3
"""Submitted task results have stable Markdown archives and bounded event delivery."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import shutil
import unittest

spec = importlib.util.spec_from_file_location('spawn_task_fixture', Path(__file__).with_name('worker-defaults-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_efficiency import EfficiencyMixin


class WorkResultFiles(unittest.TestCase):
    def setUp(self):
        self.tmp = f.tempfile.TemporaryDirectory()
        self.rt = f.ControlledRuntime(Path(self.tmp.name), f.f.FakeServer)
        self.rt.catalog = lambda account='default': __import__('copy').deepcopy(f.CATALOG)
        self.project = Path(self.tmp.name) / 'project'
        self.project.mkdir()
        self.lead = self.rt.new_lead({'cwd': str(self.project)})
        self.task = self.rt.work_action(self.lead['id'], {'action': 'create', 'title': 'Review result'})
        self.worker = self.rt.spawn_agents(self.lead, {'agents': [{'name': 'Worker', 'prompt': 'Review',
            'role': 'implementer', 'task_id': self.task['id']}]}, 'spawn-worker')['agents'][0]

    def tearDown(self):
        self.rt.close()
        self.tmp.cleanup()

    def submit(self, text, key='submit-once'):
        return self.rt.work_action(self.worker['id'], {'action': 'submit', 'task_id': self.task['id'],
            'result': text, 'checks': 'unit: pass', 'revision': 'abc1234',
            'files': ['src/app.py']}, key, actor=self.worker['id'])

    def test_submit_archives_committed_record_and_exact_retry_reuses_one_file(self):
        text = 'Complete result 🚀\n' + 'evidence ' * 2800
        first = self.submit(text)
        path = Path(first['results'][-1]['resultFile'])
        self.assertEqual(path, Path(self.rt.root) / 'results' / self.task['id'] / (first['results'][-1]['id'] + '.md'))
        body = path.read_text(encoding='utf-8')
        for value in (first['results'][-1]['text'], 'unit: pass', 'abc1234', 'src/app.py'):
            self.assertIn(value, body)
        self.assertEqual(self.submit(text), first)
        self.assertEqual(list(path.parent.glob('*.md')), [path])
        with self.rt.db() as db:
            saved = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?',
                                           (self.task['id'],)).fetchone()[0])
            event = db.execute("SELECT text FROM runtime_events WHERE id=?",
                ('work-result:' + first['results'][-1]['id'],)).fetchone()
        self.assertEqual(saved['results'][-1]['resultFile'], str(path))
        with self.rt.db() as db:
            entity = json.loads(db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.worker['id'],)).fetchone()[0])['value']
        self.assertEqual(entity['overview']['resultFile'], str(path))
        self.assertEqual(json.loads(event[0])['result']['text'], first['results'][-1]['text'])
        self.assertEqual(json.loads(event[0])['result']['resultFile'], str(path))
        with self.assertRaisesRegex(ValueError, 'different content'):
            self.submit(text + 'changed')
        # The archive lives under runtime state, so removing the worker folder cannot remove it.
        shutil.rmtree(self.worker['cwd'], ignore_errors=True)
        self.assertEqual(path.read_text(encoding='utf-8'), body)

    def test_event_projection_keeps_small_result_inline_and_points_large_result_to_file(self):
        row = {'id': 'event-small', 'kind': 'work_review'}
        value = {'task': 'task', 'title': 'Review', 'result': {
            'id': 'r1', 'text': 'small evidence ' * 700, 'resultFile': '/state/results/task/r1.md'}}
        inline = EfficiencyMixin.bounded_event(row, json.dumps(value), 20000)
        self.assertIn(value['result']['text'], json.loads(inline)['result']['text'])
        projected_inline = self.rt.model_event_text([{
            **row, 'text': json.dumps(value, ensure_ascii=False)}])
        self.assertIn(value['result']['text'], projected_inline)

        value['result']['text'] = 'large evidence ' * 2000
        projected = json.loads(EfficiencyMixin.bounded_event(row, json.dumps(value), 20000))
        self.assertTrue(projected['result']['textTruncated'])
        self.assertEqual(projected['result']['resultFile'], '/state/results/task/r1.md')
        self.assertLess(len(projected['result']['text']), 1000)

        child = {'id': 'event-child', 'kind': 'child_result'}
        child_value = {'agent_id': 'worker', 'name': 'Worker', 'status': 'completed',
                       'result': 'child evidence ' * 1000,
                       'resultFile': '/state/results/task/r2.md'}
        child_inline = self.rt.model_event_text([{
            **child, 'text': json.dumps(child_value, ensure_ascii=False)}])
        self.assertIn(child_value['result'], child_inline)
        child_value['result'] = 'large child evidence ' * 2000
        child_large = json.loads(self.rt.model_event_text([{
            **child, 'text': json.dumps(child_value, ensure_ascii=False)}]).split('\n', 1)[1])
        self.assertTrue(child_large['resultTruncated'])
        self.assertEqual(child_large['resultFile'], '/state/results/task/r2.md')
        self.assertLess(len(child_large['result']), 1000)
        self.assertLess(len(child_inline.encode()), 32000)


if __name__ == '__main__':
    unittest.main(verbosity=2)
