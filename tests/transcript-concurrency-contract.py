#!/usr/bin/env python3
"""Chat reads use one database snapshot without waiting for agent execution."""
import base64
from concurrent.futures import ThreadPoolExecutor, TimeoutError
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'workspace_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
import codex_transcript_history


class TranscriptConcurrency(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead

    def test_history_and_pending_attachment_do_not_wait_for_runtime_lock(self):
        actor = self.lead()
        asset = self.runtime.upload_asset({'agent': actor['id'], 'name': 'note.txt',
            'base64': base64.b64encode(b'attachment').decode()})
        self.runtime.send(actor['id'], 'Pending input', 'pending-input', assets=[asset['id']])
        with ThreadPoolExecutor(max_workers=1) as pool:
            with self.runtime.lock:
                future = pool.submit(self.runtime.transcript, actor['id'])
                try:
                    result = future.result(timeout=2)
                except TimeoutError:
                    self.fail('Chat read waits for the runtime execution lock')
        item = next(item for item in result['items'] if item.get('clientMessageId') == 'pending-input')
        self.assertEqual(item['text'], 'Pending input')
        self.assertEqual(item['assets'][0]['id'], asset['id'])
        self.assertNotIn('path', item['assets'][0])

    def test_items_and_agent_use_the_same_snapshot(self):
        actor = self.lead()
        with self.runtime.db() as db:
            self.runtime.item(db, actor['id'], 'answer', 'assistant', 'Before', 'Agent')
        read_rows = codex_transcript_history.history_rows
        def read_then_commit(db, *args, **kwargs):
            rows = read_rows(db, *args, **kwargs)
            # A separate connection commits while the transcript snapshot stays open.
            with self.runtime.db() as writer:
                current = self.runtime.agent(actor['id'], writer)
                current['status'] = 'paused'
                self.runtime.put(writer, 'agents', current)
                self.runtime.item(writer, actor['id'], 'answer', 'assistant', 'After', 'Agent')
            return rows
        with patch.object(codex_transcript_history, 'history_rows', side_effect=read_then_commit):
            before = self.runtime.transcript(actor['id'])
        self.assertEqual(before['items'][-1]['text'], 'Before')
        self.assertEqual(before['agent']['status'], actor['status'])
        after = self.runtime.transcript(actor['id'])
        self.assertEqual(after['items'][-1]['text'], 'After')
        self.assertEqual(after['agent']['status'], 'paused')


if __name__ == '__main__':
    unittest.main()
