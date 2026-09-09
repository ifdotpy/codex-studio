#!/usr/bin/env python3
"""Native command controls preserve process ownership and response uncertainty."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location('fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)


class NativeControls(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead
    agent_update = f.WorkspaceContract.agent_update

    def command(self):
        a = self.agent_update(self.lead(), threadId='thread-owned', accountKey='account-owned')
        task = {'id': a['id'] + ':command', 'agent': a['id'], 'itemId': 'command',
                'processId': '17', 'status': 'running', 'kind': 'command'}
        with self.runtime.db() as db:
            self.runtime.put(db, 'tasks', task)
        return task

    def test_stop_uses_exact_account_thread_and_item_without_model_input(self):
        task = self.command()
        server = Mock()
        server.call.side_effect = [
            {'data': [{'itemId': 'other', 'processId': '17'}], 'nextCursor': 'page-2'},
            {'data': [{'itemId': 'command', 'processId': '17'}], 'nextCursor': None},
            {'terminated': True},
        ]
        with patch.object(self.runtime, 'connect', return_value=server) as connect, \
                patch.object(self.runtime, 'send', side_effect=AssertionError('Unexpected model input')):
            self.assertEqual(self.runtime.native_command_action({'id': task['id'], 'action': 'cancel'}),
                             {'terminated': True})
        connect.assert_called_once_with('account-owned')
        self.assertEqual(server.call.call_args.args, ('thread/backgroundTerminals/terminate',
            {'threadId': 'thread-owned', 'processId': '17'}))
        # The completion event owns the exit code and terminal status.
        self.assertEqual(self.runtime.task_detail(task['id'])['status'], 'running')

    def test_missing_or_reused_process_id_never_terminates_another_item(self):
        task = self.command()
        for terminals in ([], [{'itemId': 'other', 'processId': '17'}],
                          [{'itemId': 'command', 'processId': '18'}]):
            with self.subTest(terminals=terminals):
                server = Mock()
                server.call.return_value = {'data': terminals, 'nextCursor': None}
                with patch.object(self.runtime, 'connect', return_value=server):
                    with self.assertRaisesRegex(ValueError, 'no longer'):
                        self.runtime.native_command_action({'id': task['id'], 'action': 'cancel'})
                self.assertEqual(server.call.call_count, 1)

    def test_unconfirmed_stop_does_not_claim_success_or_ask_model_to_retry(self):
        task = self.command()
        for result in ({'terminated': False}, TimeoutError('outcome unknown')):
            with self.subTest(result=result):
                server = Mock()
                server.call.side_effect = [{'data': [{'itemId': 'command', 'processId': '17'}]}, result]
                with patch.object(self.runtime, 'connect', return_value=server), \
                        patch.object(self.runtime, 'send', side_effect=AssertionError('Unexpected retry')):
                    with self.assertRaises((ValueError, TimeoutError)):
                        self.runtime.native_command_action({'id': task['id'], 'action': 'cancel'})
                self.assertEqual(server.call.call_count, 2)
                self.assertEqual(self.runtime.task_detail(task['id'])['status'], 'running')

    def test_input_keeps_supported_agent_delivery(self):
        task = self.command()
        with patch.object(self.runtime, 'send', return_value={'status': 'queued'}) as send:
            self.runtime.native_command_action({'id': task['id'], 'action': 'input', 'text': 'hello\n'})
        self.assertIn('write_stdin tool for session 17', send.call_args.args[1])
        self.assertIn('"hello\\n"', send.call_args.args[1])


if __name__ == '__main__':
    unittest.main(verbosity=2)
