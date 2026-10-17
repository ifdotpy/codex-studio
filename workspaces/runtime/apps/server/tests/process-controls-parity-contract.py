#!/usr/bin/env python3
"""Cross-backend witnesses for process generations and retained output matching."""
from __future__ import annotations
import json
from pathlib import Path
import unittest
from codex_layout import REPOSITORY_ROOT
from codex_process_controls import _record_process
from codex_monitor_output import append_output, pending_output_event, flush_utf8_tail, output_match_summary

class ProcessParity(unittest.TestCase):
    def test_shared_process_identities(self):
        fixture = json.loads((REPOSITORY_ROOT / 'workspaces/runtime/packages/runtime-services/tests/fixtures/process-controls.json').read_text())
        for case in fixture['processes']:
            self.assertEqual(_record_process(case['source'], case['raw'], case['owner'], case['connection']), case['expected'])

    def test_shared_output_restart_state(self):
        fixture = json.loads((REPOSITORY_ROOT / 'workspaces/runtime/packages/runtime-services/tests/fixtures/process-controls.json').read_text())['output']
        record = fixture['initial']
        for step in fixture['steps']:
            append_output(record, step['stream'], bytes(step['bytes']))
            self.assertEqual(pending_output_event(record, now=step['now']), step['event'])
            self.assertEqual(record, step['record'])
            record = json.loads(json.dumps(record))
        flush_utf8_tail(record)
        self.assertEqual(pending_output_event(record, final=True, now=11.4), fixture['final'])
        self.assertEqual(output_match_summary(record), fixture['summary'])

if __name__ == '__main__':
    unittest.main()
