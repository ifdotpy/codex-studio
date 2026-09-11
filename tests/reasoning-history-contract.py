#!/usr/bin/env python3
"""Measured reasoning history, pagination, and missing end boundaries."""
import json
from pathlib import Path
import sqlite3
import sys
import unittest
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_reasoning_history import reasoning_history


class History(unittest.TestCase):
    def setUp(self):
        self.db = sqlite3.connect(':memory:')
        self.db.row_factory = sqlite3.Row
        self.db.executescript('CREATE TABLE runtime_items(id TEXT,agent TEXT,record TEXT,created REAL);'
                             'CREATE TABLE analytics_items(id TEXT,agent TEXT,at REAL,type TEXT,record TEXT);')
        self.actor = {'id': 'a', 'status': 'running', 'inFlight': True, 'turnId': 't',
                      'activity': {'phase': 'thinking'}}
        self.messages = []
        self.message('question', 0, 'user')
        self.message('tool', 10, 'output')

    def tearDown(self):
        self.db.close()

    def message(self, key, at, role='assistant', turn='t'):
        item = {'id': key, 'role': role, 'turnId': turn, 'text': 'Visible text'}
        self.db.execute('INSERT INTO runtime_items VALUES (?,?,?,?)', (key, 'a', json.dumps(item), at))
        self.messages.append(item)

    def reasoning(self, key, start, end=None, turn='t', agent='a'):
        record = {'id': key, 'turnId': turn, 'startedAt': start, 'finishedAt': end,
                  'content': 'Must never reach the transcript'}
        self.db.execute('INSERT INTO analytics_items VALUES (?,?,?,?,?)',
                        (key, agent, start or 0, 'reasoning', json.dumps(record)))

    def result(self, messages=None, now=1000):
        messages = self.messages if messages is None else messages
        rows = [self.db.execute('SELECT * FROM runtime_items WHERE id=?', (m['id'],)).fetchone() for m in messages]
        return reasoning_history(self.db, self.actor, rows, messages, now)

    def test_eight_minutes_survive_in_history_without_reasoning_text(self):
        self.reasoning('r1', 20, 260)
        self.reasoning('r2', 280, 520)
        self.message('answer', 530)
        result = self.result()
        self.assertEqual([m['id'] for m in result], ['question', 'tool', 'reasoning:r1', 'answer'])
        self.assertEqual(result[2]['reasoningMs'], 480000)
        self.assertNotIn('Must never', json.dumps(result))

    def test_overlapping_intervals_are_not_counted_twice(self):
        self.reasoning('r1', 20, 70)
        self.reasoning('r2', 40, 80)
        self.assertEqual(self.result()[-1]['reasoningMs'], 60000)

    def test_timer_keeps_identity_when_completed(self):
        self.reasoning('r1', 20)
        active = self.result()[-1]
        self.assertEqual(active['reasoningSince'], 20)
        self.db.execute("UPDATE analytics_items SET record=json_set(record,'$.finishedAt',500)")
        done = self.result()[-1]
        self.assertEqual(active['id'], done['id'])
        self.assertIsNone(done['reasoningSince'])

    def test_disconnect_does_not_invent_an_end(self):
        self.reasoning('r1', 20)
        self.actor['status'] = 'interrupted'
        self.assertEqual(self.result(), self.messages)

    def test_tool_wait_is_not_reasoning(self):
        self.assertEqual(self.result(now=1000000), self.messages)

    def test_missing_completion_before_new_activity_is_wait_not_reasoning(self):
        self.reasoning('r1', 20)
        self.reasoning('r2', 500, 520)
        self.message('answer', 530)
        result = self.result()[2]
        self.assertEqual(result['reasoningMs'], 500000)
        self.assertTrue(result['observedWait'])
        self.assertIsNone(result['reasoningSince'])

    def test_page_boundaries_do_not_repeat_reasoning(self):
        self.reasoning('r1', 20, 80)
        self.message('answer', 100)
        older = self.result(self.messages[:2])
        newer = self.result(self.messages[2:])
        self.assertEqual(sum(m['role'] == 'reasoning' for m in older + newer), 1)

    def test_other_turns_agents_and_missing_start_are_excluded(self):
        self.reasoning('r1', 20, 80, turn='restored-away')
        self.reasoning('r2', 20, 80, agent='other')
        self.reasoning('r3', None, 80)
        self.assertEqual(self.result(), self.messages)


if __name__ == '__main__':
    unittest.main()
