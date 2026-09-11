#!/usr/bin/env python3
"""Independent checks for workspace recovery and current source state."""
import concurrent.futures
import importlib.util
from pathlib import Path
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('workspace_fixture', Path(__file__).with_name('workspace-contract.py'))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_native_errors import NativeRpcError

class WorkspaceReviewContract(unittest.TestCase):
    setUp = fixture.WorkspaceContract.setUp
    tearDown = fixture.WorkspaceContract.tearDown
    lead = fixture.WorkspaceContract.lead
    worker = fixture.WorkspaceContract.worker
    start = fixture.WorkspaceContract.start
    agent_update = fixture.WorkspaceContract.agent_update
    git = fixture.WorkspaceContract.git
    git_project = fixture.WorkspaceContract.git_project
    isolated_worker = fixture.WorkspaceContract.isolated_worker

    def branch_point(self):
        lead = self.start(self.lead())
        self.runtime.notification({'method': 'item/completed', 'params': {
            'threadId': lead['threadId'], 'turnId': lead['turnId'],
            'item': {'id': 'branch-point', 'type': 'agentMessage', 'text': 'Branch point'}}})
        self.runtime.server.complete(lead['threadId'], lead['turnId'])
        message = next(item for item in self.runtime.transcript(lead['id'])['items'] if item['text'] == 'Branch point')
        return lead, {'id': 'branch-review', 'message_id': message['id']}

    def test_fork_does_not_restore_completed_source_to_running(self):
        lead, request = self.branch_point()
        current = self.start(lead, 'Independent second turn')
        original = self.runtime.server.call
        def complete_during_fork(method, params, timeout=60):
            if method == 'thread/fork':
                self.runtime.server.complete(current['threadId'], current['turnId'])
            return original(method, params, timeout)
        with patch.object(self.runtime.server, 'call', complete_during_fork):
            self.runtime.branch_conversation(lead['id'], request)
        source = self.runtime.agent(lead['id'])
        self.assertEqual(source['status'], 'completed')
        self.assertFalse(source['inFlight'])
        self.assertIsNone(source['turnId'])

    def test_rejected_fork_retry_reserves_unknown_attempt(self):
        lead, request = self.branch_point()
        self.runtime.server.fork_error = NativeRpcError({'code': -32602, 'message': 'Invalid input'})
        with self.assertRaises(NativeRpcError):
            self.runtime.branch_conversation(lead['id'], request)
        self.runtime.server.fork_error = RuntimeError('response lost; outcome unknown')
        with self.assertRaisesRegex(RuntimeError, 'outcome unknown'):
            self.runtime.branch_conversation(lead['id'], request)
        with self.assertRaisesRegex(ValueError, 'recovery'):
            self.runtime.branch_conversation(lead['id'], request)
        self.assertEqual(sum(method == 'thread/fork' for method, _ in self.runtime.server.calls), 2)

    def test_legacy_rule_change_during_fork_preserves_one_branch(self):
        lead, request = self.branch_point()
        original = self.runtime.server.call
        def change_legacy_rules(method, params, timeout=60):
            result = original(method, params, timeout)
            if method == 'thread/fork':
                self.runtime.accounts.data['accounts']['default']['projectRules'] = {
                    'allowedProjects': [], 'revision': 1,
                }
            return result
        with patch.object(self.runtime.server, 'call', change_legacy_rules):
            result = self.runtime.branch_conversation(lead['id'], request)
        repeated = self.runtime.branch_conversation(lead['id'], request)
        self.assertEqual(result['id'], repeated['id'])
        self.assertEqual(result['forkedFrom'], lead['id'])
        self.assertEqual(result['accountKey'], lead['accountKey'])
        self.assertEqual(len(self.runtime.snapshot()['agents']), 2)
        self.assertEqual(sum(method == 'thread/fork' for method, _ in self.runtime.server.calls), 1)

    def test_concurrent_restore_uses_one_native_result(self):
        worker, path = self.isolated_worker()
        self.agent_update(worker, threadId='original-thread')
        checkpoint = self.runtime.checkpoint_capture(worker['id'], turn_id='original-turn')
        (path / 'tracked.txt').write_text('edit\n')
        preview = self.runtime.checkpoint_preview(worker['id'], checkpoint['id'])
        request = {'checkpoint_id': checkpoint['id'], 'expectedTree': preview['expectedTree']}
        server = self.runtime.connect()
        original = server.call
        entered, release = threading.Event(), threading.Event()
        calls = []
        def gated(method, params, timeout=60):
            if method == 'thread/fork':
                calls.append(params)
                entered.set()
                if not release.wait(3): raise RuntimeError('test gate expired')
            return original(method, params, timeout)
        with patch.object(server, 'call', gated), concurrent.futures.ThreadPoolExecutor(2) as pool:
            first = pool.submit(self.runtime.restore_checkpoint, worker['id'], request)
            self.assertTrue(entered.wait(2))
            second = pool.submit(self.runtime.restore_checkpoint, worker['id'], request)
            release.set()
            self.assertEqual(first.result(5)['status'], 'restored')
            self.assertEqual(second.result(5)['status'], 'restored')
        self.assertEqual(len(calls), 1)

    def test_same_checkpoint_can_restore_a_later_edit(self):
        worker, path = self.isolated_worker()
        self.agent_update(worker, threadId='original-thread')
        checkpoint = self.runtime.checkpoint_capture(worker['id'], turn_id='original-turn')
        original = (path / 'tracked.txt').read_text()
        for content in ('first edit\n', 'different edit\n'):
            (path / 'tracked.txt').write_text(content)
            preview = self.runtime.checkpoint_preview(worker['id'], checkpoint['id'])
            result = self.runtime.restore_checkpoint(worker['id'], {
                'checkpoint_id': checkpoint['id'], 'expectedTree': preview['expectedTree']})
            self.assertEqual(result['status'], 'restored')
            self.assertEqual((path / 'tracked.txt').read_text(), original)
        self.assertEqual(sum(method == 'thread/fork' for method, _ in self.runtime.server.calls), 2)

if __name__ == '__main__': unittest.main()
