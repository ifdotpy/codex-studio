#!/usr/bin/env python3
"""Removed review assignments cannot schedule work or grant chat access."""
import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('removed_review_fixture', Path(__file__).with_name('workspace-contract.py'))
f = importlib.util.module_from_spec(spec)
spec.loader.exec_module(f)
from codex_state_cleanup import remove_review_assignments
from codex_team_isolation import validate_event

class RemovedReviews(unittest.TestCase):
    setUp = f.WorkspaceContract.setUp
    tearDown = f.WorkspaceContract.tearDown
    lead = f.WorkspaceContract.lead

    def test_cleanup_is_idempotent_and_preserves_submitted_history(self):
        target, reviewer = self.lead('Target'), self.lead('Reviewer')
        with self.runtime.lock, self.runtime.db() as db:
            target['reviewSchedules'] = [{'reviewerId': reviewer['id'], 'enabled': True, 'nextAt': 0}]
            self.runtime.put(db, 'agents', target)
            room = {'id': 'retired-room', 'kind': 'private', 'members': [target['id'], reviewer['id']], 'reviewTargets': [target['id']], 'updated': 1}
            self.runtime.put(db, 'rooms', room)
            for status in ('pending', 'reserved', 'delivered', 'uncertain'):
                self.runtime.enqueue(db, reviewer, 'chat_review', 'Saved review input', status)
                db.execute('UPDATE runtime_events SET status=? WHERE id=?', (status, status))
            self.assertEqual(remove_review_assignments(self.runtime, db), {'agents': 1, 'rooms': 1, 'pending': 1})
            self.assertEqual(remove_review_assignments(self.runtime, db), {'agents': 0, 'rooms': 0, 'pending': 0})
            self.assertNotIn('reviewSchedules', self.runtime.agent(target['id'], db))
            for status in ('pending', 'reserved', 'delivered', 'uncertain'):
                row = db.execute('SELECT * FROM runtime_events WHERE id=?', (status,)).fetchone()
                self.assertEqual(row['text'], 'Saved review input')
                self.assertEqual(row['status'], 'cancelled' if status == 'pending' else status)
                self.assertIsNotNone(validate_event(self.runtime, db, reviewer, row))
            self.assertNotIn('reviewTargets', json.loads(db.execute("SELECT record FROM runtime_rooms WHERE id='retired-room'").fetchone()[0]))
        self.runtime.rules_tick()
        with self.assertRaises(ValueError): self.runtime.chat_read('retired-room', reviewer['id'])
        with self.assertRaises(ValueError): self.runtime.chat_message(target['id'], reviewer['id'], 'Forbidden', 'removed-grant')

    def test_removed_settings_rejected_and_native_code_review_remains(self):
        target = self.lead('Target')
        with self.assertRaisesRegex(ValueError, 'Unknown chat organization field'):
            self.runtime.chat_organization(target['id'], {'id': target['id'], 'review_schedule': {}})
        definitions = {tool['name']: tool for tool in self.runtime.tool_definitions()}
        self.assertIn('orchestration_review', definitions)
        props = definitions['orchestration_message']['inputSchema']['properties']
        self.assertNotIn('review_event_id', props)
        self.assertNotIn('review_outcome', props)

if __name__ == '__main__': unittest.main()
