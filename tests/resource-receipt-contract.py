#!/usr/bin/env python3
"""A rejected resource claim cannot appear as a successful tool operation."""
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

    def test_denied_claim_and_release_have_failed_outer_receipts(self):
        lead = self.lead()
        first, second = self.worker(lead), self.worker(lead, 'second')
        self.runtime.resource_action({'action': 'claim', 'resource': 'fixture-slot'}, first['id'])
        for action in ('claim', 'renew', 'release'):
            response = self.tool(second, 'orchestration_resource', {'action': action, 'resource': 'fixture-slot'})
            self.assertFalse(response['success'], response)
            value = json.loads(response['contentItems'][0]['text'])
            self.assertFalse(value['ok'])
            self.assertFalse(value['ownsResource'])
            self.assertEqual(value['holder'], first['id'])
            self.assertEqual(value['outcome'], 'not_applied')
            with self.runtime.db() as db:
                receipts = self.runtime.records(db, 'tool_requests')
            saved = [r for r in receipts if r['tool'] == 'orchestration_resource'][-1]
            self.assertEqual(saved['outcome'], 'not_applied')
            self.assertEqual(saved['stage'], 'failed')
        owned = self.tool(first, 'orchestration_resource', {'action': 'renew', 'resource': 'fixture-slot'})
        self.assertTrue(owned['success'])
        self.assertTrue(json.loads(owned['contentItems'][0]['text'])['ownsResource'])

    def test_process_failure_keeps_unknown_outcome(self):
        lead = self.lead()
        import subprocess
        with patch('codex_rules.subprocess.run', return_value=subprocess.CompletedProcess([], 2, '', 'write failed')):
            response = self.tool(lead, 'orchestration_resource', {'action': 'claim', 'resource': 'fixture-slot'})
        self.assertFalse(response['success'])
        self.assertEqual(json.loads(response['contentItems'][0]['text'])['outcome'], 'unknown')

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
