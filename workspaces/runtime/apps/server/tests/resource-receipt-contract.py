#!/usr/bin/env python3
"""Removed compatibility routes fail conservatively without changing the board."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
from pathlib import Path
import unittest
from unittest.mock import patch
import uuid

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

    def set_limit(self, lead, limit):
        current = self.runtime.agent(lead['id'])
        return self.runtime.conversation_settings(lead['id'], {
            'subagent_concurrency': limit,
            'expected_mode_revision': current['agentModeRevision'],
            'request_id': str(uuid.uuid4())})

    def test_removed_resource_routes_fail_without_board_writes(self):
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
                expected = 'Unknown orchestration tool' if name == 'orchestration_resource' else 'Unknown managed agent'
                self.assertEqual(response['contentItems'][0]['text'], expected)
                with self.runtime.db() as db:
                    receipts = self.runtime.records(db, 'tool_requests')
                saved = receipts[-1]
                # 56e34c9 removed the retirement bridge. Runtime marks this
                # direct rejection as not applied; an unsupported legacy send
                # route keeps the conservative unknown outcome.
                expected_outcome = 'not_applied' if name == 'orchestration_resource' else 'unknown'
                self.assertEqual(saved['outcome'], expected_outcome)
                self.assertEqual(saved['stage'], 'failed')
                self.assertEqual(board.read_text(), original)

    def test_removed_resource_result_preserves_uncertainty(self):
        from codex_tool_requests import request_result_outcome
        record = {'tool': 'orchestration_resource'}
        for message in ('Unknown orchestration tool', 'write failed'):
            result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': message}]}
            self.assertEqual(request_result_outcome(record, result), 'unknown')
        self.assertEqual(request_result_outcome(record, {'success': True}), 'applied')
        self.assertEqual(request_result_outcome(record, {}), 'unknown')

    def test_status_exposes_actual_limits_and_block_reasons(self):
        lead = self.lead()
        worker = self.worker(lead)
        self.set_limit(lead, 1)
        self.agent_update(lead, status='running', inFlight=True)
        self.agent_update(worker, status='queued', autoWake=True)
        with patch.dict(os.environ, {'CODEX_CANVAS_CONCURRENCY': '1'}):
            first = self.runtime.model_directory(lead['id'], 'orchestration_status', {})
        capacity = first['capacity']
        self.assertEqual(capacity['teamLimit'], 1)
        self.assertEqual(capacity['globalLimit'], 1)
        self.assertEqual(capacity['teamActive'], 1)
        self.assertEqual(capacity['queued'][0]['reasons'], ['global_concurrency'])
        self.set_limit(lead, 3)
        with patch.dict(os.environ, {'CODEX_CANVAS_CONCURRENCY': '4'}):
            second = self.runtime.model_directory(lead['id'], 'orchestration_status', {'since_revision': first['revision']})
        self.assertFalse(second['unchanged'])
        self.assertEqual(second['capacity']['queued'][0]['reasons'], ['awaiting_dispatch'])


if __name__ == '__main__':
    unittest.main()
