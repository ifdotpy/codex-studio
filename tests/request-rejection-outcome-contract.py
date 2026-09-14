#!/usr/bin/env python3
"""Exact validation failures release archive holds without repeating operations."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('reconciliation_fixture',
    Path(__file__).with_name('request-reconciliation-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_tool_requests import request_result_outcome


REJECTIONS = (
    ('orchestration_message', 'Recipient conversation was deleted'),
    ('orchestration_task', 'Supply up to 50 file paths'),
    ('orchestration_result', 'Supply up to 50 file paths'),
)


class RejectionOutcomes(unittest.TestCase):
    setUp = f.Receipts.setUp
    message = f.Receipts.message
    reserve = f.Receipts.reserve
    failed_request = f.Receipts.failed_request
    save_operation_receipt = f.Receipts.save_operation_receipt

    def test_new_exact_rejections_cannot_restart_the_operation(self):
        for index, (tool, error) in enumerate(REJECTIONS):
            row = self.failed_request(str(index), tool, error=error)
            self.assertEqual(row['outcome'], 'not_applied')
            self.assertFalse(self.runtime.begin_tool_request(row['id']))

    def test_historical_reconciliation_preserves_error_time_and_identity(self):
        for index, (tool, error) in enumerate(REJECTIONS):
            row = self.failed_request(str(index), tool, error=error, legacy=True)
            found = self.runtime.request_action('lead', {'action': 'get', 'request_id': row['id']})
            self.assertEqual(found['outcome'], 'not_applied')
            self.assertEqual(found['id'], row['id'])
            self.assertEqual(found['result'], row['result'])
            self.assertEqual(found['finished'], 123.0)
            self.assertFalse(self.runtime.begin_tool_request(row['id']))

    def test_committed_operation_overrides_each_validation_inference(self):
        for index, (tool, error) in enumerate(REJECTIONS):
            row = self.failed_request(str(index), tool, error=error, legacy=True)
            self.save_operation_receipt(row['id'], {'id': 'committed-' + str(index)})
            found = self.runtime.request_action('lead', {'action': 'get', 'request_id': row['id']})
            self.assertEqual(found['outcome'], 'unknown')
            self.assertTrue(found['operationApplied'])
            self.assertEqual(found['result'], row['result'])
            self.assertEqual(found['finished'], 123.0)

    def test_other_tools_and_inexact_messages_remain_unknown(self):
        for tool, error in REJECTIONS:
            for other_tool, other_error in (
                ('orchestration_monitor', error),
                ('orchestration_send', error),
                (tool, error + ': after write'),
                (tool, 'database or disk is full'),
                (tool, 'Native response timed out; outcome unknown'),
            ):
                result = {'success': False, 'contentItems': [{'type': 'inputText', 'text': other_error}]}
                self.assertEqual(request_result_outcome({'tool': other_tool}, result), 'unknown')


class ArchiveRejections(unittest.TestCase):
    setUp = f.Archives.setUp
    call = f.Archives.call
    record = f.Archives.record

    def test_inspect_and_archive_reconcile_only_confirmed_rejections(self):
        self.record('orchestration_message', 'Recipient conversation was deleted')
        self.assertTrue(self.call('inspect')['canArchive'])
        self.assertEqual(self.call('archive')['status'], 'archived')
        with self.rt.db() as db:
            row = self.rt.records(db, 'tool_requests')[0]
            self.assertEqual(row['outcome'], 'not_applied')
            self.assertEqual(row['result']['contentItems'][0]['text'], 'Recipient conversation was deleted')
            self.assertEqual(self.rt.records(db, 'items')[0]['text'], 'Preserved result')
        self.assertEqual(self.rt.calls, [('thread/read', 'worker')])

    def test_monitor_disk_failure_keeps_archive_blocked(self):
        self.record('orchestration_monitor', 'database or disk is full')
        self.assertFalse(self.call('inspect')['canArchive'])
        self.assertEqual(self.call('archive')['status'], 'blocked')
        self.assertEqual(self.rt.calls, [])


if __name__ == '__main__':
    unittest.main()
