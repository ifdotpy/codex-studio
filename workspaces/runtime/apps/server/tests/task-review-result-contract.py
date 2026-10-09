#!/usr/bin/env python3
"""Review errors name the result field and retain exact old rejection receipts."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('task_review_fixture',
    Path(__file__).with_name('task-completion-recovery-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_runtime import TOOLS
from codex_tool_requests import request_result_outcome


class ReviewResult(unittest.TestCase):
    setUp = f.TaskCompletionRecovery.setUp
    tearDown = f.TaskCompletionRecovery.tearDown
    lead = f.TaskCompletionRecovery.lead
    agent_update = f.TaskCompletionRecovery.agent_update
    review_task = f.TaskCompletionRecovery.review_task
    message = f.TaskCompletionRecovery.message
    response = f.TaskCompletionRecovery.response
    value = f.TaskCompletionRecovery.value

    def test_missing_result_names_exact_field_without_mutation_or_replay(self):
        for action, field in (('accept', 'reason'), ('accept', 'review'),
                              ('reject', 'reason'), ('reject', 'review')):
            task = self.review_task()
            call = 'missing-result-' + action + '-' + field
            args = {'action': action, 'task_id': task['id'], field: 'Review passed'}
            message = self.message(call, 'orchestration_task', args)
            self.runtime.dynamic(message)
            result = self.response(call)
            self.assertFalse(result['success'])
            self.assertEqual(result['contentItems'][0]['text'],
                             'Supply result with 1 to 32000 characters for accept or reject')
            receipt = self.runtime.request_action(self.actor['id'], {'action': 'get', 'request_id': call})
            self.assertEqual(receipt['outcome'], 'not_applied')
            self.assertFalse(self.runtime.begin_tool_request(receipt['id']))
            stored = self.runtime.model_work(self.actor['id'], {'action': 'get', 'task_id': task['id']})
            self.assertEqual(stored['status'], 'review')
            with self.runtime.db() as db:
                record = json.loads(db.execute('SELECT record FROM runtime_work WHERE id=?',
                                               (task['id'],)).fetchone()[0])
            self.assertEqual(record['decisions'], [])

    def test_legacy_and_new_errors_are_exact_prewrite_rejections(self):
        for error in ('Supply a review decision with 1 to 32000 characters',
                      'Supply result with 1 to 32000 characters for accept or reject'):
            result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': error}]}
            self.assertEqual(request_result_outcome({'tool': 'orchestration_task'}, result), 'not_applied')
            result['contentItems'][0]['text'] += ' after write'
            self.assertEqual(request_result_outcome({'tool': 'orchestration_task'}, result), 'unknown')

    def test_tool_schema_explains_result_for_both_review_actions(self):
        tool = next(item for item in TOOLS if item['name'] == 'orchestration_task')
        description = tool['inputSchema']['properties']['result']['description']
        self.assertIn('accept', description)
        self.assertIn('reject', description)
        self.assertIn('required', description.lower())
        self.assertNotIn('result', tool['inputSchema']['required'])


if __name__ == '__main__':
    unittest.main()
