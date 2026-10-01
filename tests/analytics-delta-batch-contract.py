#!/usr/bin/env python3
"""Differential contract for aggregated live assistant-delta analytics."""
import json
from pathlib import Path
import sqlite3
import sys
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_analytics import AnalyticsMixin


class Store(AnalyticsMixin):
    def records(self, _db, _kind):
        return []


AGENT = {'id': 'agent', 'name': 'Agent', 'rootId': 'root', 'threadId': 'thread',
         'accountKey': 'default', 'model': 'model', 'effort': None, 'fastMode': None}
SAMPLES = [
    {'threadId': 'thread', 'turnId': 'turn', 'itemId': 'item', 'delta': ''},
    {'threadId': 'thread', 'turnId': 'turn', 'itemId': 'item', 'delta': 'é\n'},
    {'threadId': 'thread', 'turnId': 'turn', 'itemId': 'item', 'delta': '漢'},
    {'threadId': 'thread', 'turnId': 'turn', 'itemId': 'item', 'delta': ''},
]
TIMES = [7199, 7201, 3601, 3602]  # Out of order; first output is the second sample.


def make_store(item_state, *, fail=False):
    store = Store()
    db = sqlite3.connect(':memory:')
    with patch('codex_analytics.time.time', return_value=9000):
        store.analytics_init(db)
        store.analytics_event(db, AGENT, 'turn/started', {'threadId': 'thread',
                            'turn': {'id': 'turn'}}, at=1000)
        if item_state in {'open', 'finished'}:
            store.analytics_event(db, AGENT, 'item/started', {'threadId': 'thread',
                'turnId': 'turn', 'item': {'id': 'item', 'type': 'agentMessage'}}, at=1100)
        if item_state == 'finished':
            store.analytics_event(db, AGENT, 'item/completed', {'threadId': 'thread',
                'turnId': 'turn', 'item': {'id': 'item', 'type': 'agentMessage', 'text': 'old'}}, at=1200)
        if fail:
            db.execute("CREATE TRIGGER reject_delta BEFORE INSERT ON analytics_notifications "
                       "WHEN NEW.method='item/agentMessage/delta' BEGIN SELECT RAISE(ABORT,'injected'); END")
    return store, db


def capture_rows(db):
    result = {}
    for table in ('analytics_agents', 'analytics_notifications', 'analytics_turns', 'analytics_items', 'analytics_meta'):
        result[table] = db.execute(f'SELECT * FROM {table} ORDER BY 1').fetchall()
    return result


class DeltaBatchContract(unittest.TestCase):
    def compare_case(self, item_state, *, fail=False):
        sequential, seq_db = make_store(item_state, fail=fail)
        aggregated, agg_db = make_store(item_state, fail=fail)
        with patch('codex_analytics.time.time', return_value=9000):
            for sample, at in zip(SAMPLES, TIMES):
                sequential.analytics_safe(seq_db, sequential.analytics_event, AGENT,
                                          'item/agentMessage/delta', sample, at=at)
            aggregated.analytics_delta_batch_safe(agg_db, AGENT, SAMPLES, at_values=TIMES)
            final = {'threadId': 'thread', 'turnId': 'turn',
                     'item': {'id': 'item', 'type': 'agentMessage', 'text': 'authoritative final'}}
            sequential.analytics_safe(seq_db, sequential.analytics_event, AGENT,
                                      'item/completed', final, at=8000)
            aggregated.analytics_safe(agg_db, aggregated.analytics_event, AGENT,
                                      'item/completed', final, at=8000)
        self.assertEqual(capture_rows(seq_db), capture_rows(agg_db))
        turn = json.loads(agg_db.execute('SELECT record FROM analytics_turns').fetchone()[0])
        expected_first_output = 8000 if fail else 1200 if item_state == 'finished' else 7201
        self.assertEqual(turn['firstOutputAt'], expected_first_output)
        item = json.loads(agg_db.execute('SELECT record FROM analytics_items').fetchone()[0])
        self.assertEqual(item['output']['bytes'], len('authoritative final'.encode('utf-8')))
        if item_state == 'open' and not fail:
            self.assertEqual(item['stream'], {'bytes': 6, 'chars': 3, 'lines': 1, 'deltas': 4})
        if fail:
            errors = json.loads(agg_db.execute("SELECT value FROM analytics_meta WHERE key='captureErrors'").fetchone()[0])
            self.assertEqual(errors['count'], len(SAMPLES))
        seq_db.close()
        agg_db.close()

    def test_unicode_empty_hour_boundaries_and_out_of_order_timestamps(self):
        for state in ('missing', 'open', 'finished'):
            with self.subTest(item_state=state):
                self.compare_case(state)

    def test_fast_path_failure_rolls_back_before_per_sample_fallback(self):
        self.compare_case('open', fail=True)


if __name__ == '__main__':
    unittest.main()
