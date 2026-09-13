#!/usr/bin/env python3
"""Retired resource calls cannot mutate state or leave uncertain receipts."""
import importlib.util
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('resource_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class ResourceReceiptContract(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    worker = f.WorkspaceContract.worker
    tool = f.WorkspaceContract.tool
    agent_update = f.WorkspaceContract.agent_update

    def test_retired_resource_calls_fail_without_writes_or_unknown_receipts(self):
        lead = self.lead()
        board = self.root / 'board' / 'codex-board.json'
        board.parent.mkdir()
        original = '{"claims":{"old":{"worker":"old-worker","at":1}}}'
        board.write_text(original)
        self.assertNotIn('orchestration_resource', {t['name'] for t in self.runtime.tool_definitions(lead)})
        for action in ('show', 'claim', 'renew', 'release'):
            args = {'action': action, 'resource': 'old'}
            for name, payload in (
                ('orchestration_resource', args),
                ('orchestration_send', {'agent_id': 'workspace', 'text': json.dumps({'tool': 'orchestration_resource', 'arguments': args})}),
            ):
                response = self.tool(lead, name, payload)
                self.assertFalse(response['success'], response)
                self.assertIn('reservations were removed', response['contentItems'][0]['text'])
                with self.runtime.db() as db:
                    receipts = self.runtime.records(db, 'tool_requests')
                saved = receipts[-1]
                self.assertEqual(saved['outcome'], 'not_applied')
                self.assertEqual(saved['stage'], 'failed')
                self.assertEqual(board.read_text(), original)

    def test_retired_result_classification_preserves_other_uncertainty(self):
        from codex_tool_requests import request_result_outcome
        record = {'tool': 'orchestration_resource'}
        for message, expected in (
            ('Resource reservations were removed. Continue without a board claim.', 'not_applied'),
            ('Unknown workspace tool', 'not_applied'),
            ('write failed', 'unknown'),
        ):
            result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': message}]}
            self.assertEqual(request_result_outcome(record, result), expected)
        self.assertEqual(request_result_outcome(record, {'success': True}), 'applied')
        self.assertEqual(request_result_outcome(record, {}), 'unknown')

    def test_status_exposes_actual_limits_and_block_reasons(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.runtime.configure(lead['id'], {'concurrency': 1})
        self.agent_update(lead, status='running', inFlight=True)
        self.agent_update(worker, status='queued', autoWake=True)
        with patch.dict(os.environ, {'CODEX_CANVAS_CONCURRENCY': '1'}):
            first = self.runtime.model_directory(lead['id'], 'orchestration_status', {})
        capacity = first['capacity']
        self.assertEqual(capacity['teamLimit'], 1)
        self.assertEqual(capacity['globalLimit'], 1)
        self.assertEqual(capacity['teamActive'], 1)
        self.assertEqual(capacity['queued'][0]['reasons'], ['global_concurrency', 'team_concurrency'])
        self.runtime.configure(lead['id'], {'concurrency': 3})
        with patch.dict(os.environ, {'CODEX_CANVAS_CONCURRENCY': '4'}):
            second = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': first['revision']})
        self.assertFalse(second['unchanged'])
        self.assertEqual(second['capacity']['queued'][0]['reasons'], ['awaiting_dispatch'])


if __name__ == '__main__':
    unittest.main()
