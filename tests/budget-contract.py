#!/usr/bin/env python3
"""Durable budget fixtures. No native server or model calls."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'scripts'))
from codex_budget import budget_admission, budget_capture, budget_migrate, budget_status
from codex_analytics import AnalyticsMixin
from codex_analytics_history import rollout_actions, inherited_usage_threads, repair_terminal_errors


class Runtime(AnalyticsMixin):
    def __init__(self, path):
        self.db = sqlite3.connect(path)
        self.db.row_factory = sqlite3.Row
        self.db.execute('CREATE TABLE IF NOT EXISTS runtime_agents (id TEXT PRIMARY KEY,record TEXT)')
        self.analytics_init(self.db)
        self.db.execute('CREATE TABLE IF NOT EXISTS analytics_history (id TEXT PRIMARY KEY,agent TEXT,record TEXT)')

    def agent(self, key, db):
        return json.loads(db.execute('SELECT record FROM runtime_agents WHERE id=?', (key,)).fetchone()[0])

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute('SELECT record FROM runtime_' + table)]


class BudgetTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / 'db'
        self.r = Runtime(self.path)
        self.a = {'id': 'a', 'rootId': 'a', 'threadId': 'thread', 'turnId': 'turn',
                  'accountKey': 'account', 'tokensUsed': 0, 'tokenBudget': None, 'created': 0}
        self.put(self.a)

    def tearDown(self):
        self.r.db.close()
        self.temp.cleanup()

    def put(self, a):
        self.r.db.execute('INSERT OR REPLACE INTO runtime_agents VALUES (?,?)', (a['id'], json.dumps(a)))

    def notice(self, total, last, at=None, **kw):
        return budget_capture(self.r.db, self.a, {'threadId': 'thread', 'turnId': 'turn',
            'tokenUsage': {'total': {'totalTokens': total}, 'last': {'totalTokens': last}}, **kw}, at=at)

    def response(self, rid, amount, at=None, **kw):
        return budget_capture(self.r.db, self.a, {'threadId': 'thread', 'turnId': 'turn', 'responseId': rid,
            'rawTokenUsageRecord': {'response_id': rid}, 'requestUsage': {'totalTokens': amount},
            'tokenUsage': {'total': {'totalTokens': amount}, 'last': {'totalTokens': amount}}, **kw},
            at=at, source='rollout')

    def complete_history(self):
        path = Path(self.temp.name) / 'thread.jsonl'
        path.write_text('{}\n')
        info = path.stat()
        record = {'status': 'current', 'coverage': 'availableRecords', 'path': str(path),
                  'offset': info.st_size, 'identity': [info.st_dev, info.st_ino],
                  'context': {'requestUsageAvailable': True}}
        self.r.db.execute('INSERT OR REPLACE INTO analytics_history VALUES (?,?,?)',
                          ('a:account:thread', 'a', json.dumps(record)))
        return path

    def test_counter_reset_replay_and_restart_preserve_spending(self):
        self.assertEqual(self.notice(150, 150), 150)
        self.assertEqual(self.notice(10, 10), 160)
        self.assertEqual(self.notice(10, 10), 160)
        self.r.db.commit()
        self.r.db.close()
        self.r = Runtime(self.path)
        self.a = self.r.agent('a', self.r.db)
        self.assertEqual(self.notice(20, 10), 170)
        self.a['tokenBudget'] = 100
        self.put(self.a)
        with self.assertRaisesRegex(ValueError, 'budget reached'):
            budget_admission(self.r, self.r.db, self.a)

    def test_responses_count_compaction_once_in_any_arrival_order(self):
        self.notice(150, 150)
        self.assertEqual(self.response('ordinary', 150), 150)
        self.assertEqual(self.response('compaction', 75), 225)
        self.assertEqual(self.response('compaction', 75), 225)
        self.notice(10, 10)
        self.assertEqual(self.response('later', 10), 235)
        self.assertEqual(self.response('old', 20, at=1), 255)
        self.assertEqual(self.notice(150, 150), 255)

    def test_compacted_rollout_latest_usage_counts_once(self):
        self.response('ordinary', 150)
        record = {'type': 'compacted', 'payload': {'latest_token_usage_record': {
            'thread_id': 'thread', 'turn_id': 'turn', 'response_id': 'compaction',
            'usage': {'total_tokens': 75}, 'thread_token_usage': {'total_tokens': 225}}}}
        for _ in range(2):
            for action, method, p, at in rollout_actions(record, {'threadId': 'thread', 'turnId': 'turn'}, 'snapshot', time.time()):
                if method == 'thread/tokenUsage/updated':
                    budget_capture(self.r.db, self.a, p, at=at, source='rollout')
        self.assertEqual(self.r.agent('a', self.r.db)['tokensUsed'], 225)

    def test_migration_floor_does_not_add_historical_usage_twice(self):
        self.a['tokensUsed'] = 1000
        self.put(self.a)
        budget_status(self.r, self.r.db, self.a)
        self.assertEqual(self.response('old', 1000, at=1), 1000)
        self.assertEqual(self.response('new', 20), 1020)
        self.assertEqual(self.notice(1020, 20), 1020)
        self.assertEqual(self.response('missed-old-compaction', 50, at=2), 1070)

    def test_migration_is_bounded_and_resumable(self):
        for i in range(130):
            record = {'threadId': 'thread', 'turnId': 'old', 'at': 1, 'responseId': str(i),
                      'rawTokenUsageRecord': {'response_id': str(i)}, 'last': {'totalTokens': 1}}
            self.r.db.execute('INSERT INTO analytics_usage(id,agent,at,record) VALUES (?,?,?,?)', (str(i), 'a', 1, json.dumps(record)))
        budget_status(self.r, self.r.db, self.a)
        self.assertEqual(self.r.agent('a', self.r.db)['tokensUsed'], 0)
        budget_migrate(self.r.db, self.a)
        self.assertEqual(self.r.agent('a', self.r.db)['tokensUsed'], 64)
        budget_migrate(self.r.db, self.a)
        self.assertEqual(self.r.agent('a', self.r.db)['tokensUsed'], 128)
        budget_migrate(self.r.db, self.a)
        budget_migrate(self.r.db, self.a)
        self.assertEqual(self.r.agent('a', self.r.db)['tokensUsed'], 130)

    def test_out_of_order_notices_do_not_create_reset_charges(self):
        self.notice(150, 150, at=20)
        self.assertEqual(self.notice(100, 100, at=10), 150)
        self.assertEqual(self.response('earlier', 100, at=10), 150)
        self.assertEqual(self.response('later', 50, at=20), 150)

    def test_reset_repeated_response_and_equal_size_new_response(self):
        self.assertEqual(self.notice(9000, 100), 100)
        self.assertEqual(self.notice(100, 100), 100)
        self.assertEqual(self.response('same', 100), 100)
        self.assertEqual(self.response('same', 100), 100)
        # The history association proves which response the reset notice repeats.
        p = {'threadId': 'thread', 'turnId': 'turn', 'responseId': 'same',
             'tokenUsage': {'total': {'totalTokens': 100}, 'last': {'totalTokens': 100}}}
        budget_capture(self.r.db, self.a, p, source='rollout')
        self.a['tokenBudget'] = 1000
        self.put(self.a)
        self.complete_history()
        self.assertEqual(budget_admission(self.r, self.r.db, self.a)['tokensUsed'], 100)
        self.assertEqual(self.response('equal-sized-new', 100), 200)

    def test_admission_refreshes_the_callers_stale_projection(self):
        stale = dict(self.a)
        self.response('new', 100)
        current = self.r.agent('a', self.r.db)
        current['tokenBudget'] = 1
        self.put(current)
        with self.assertRaisesRegex(ValueError, 'budget reached'):
            budget_admission(self.r, self.r.db, stale)
        self.assertEqual(stale['tokensUsed'], 100)

    def test_history_only_associated_reset_notices_are_one_charge(self):
        self.response('same', 100)
        for total in (9000, 100):
            p = {'threadId': 'thread', 'turnId': 'turn', 'responseId': 'same',
                 'tokenUsage': {'total': {'totalTokens': total}, 'last': {'totalTokens': 100}}}
            self.assertEqual(budget_capture(self.r.db, self.a, p, source='rollout'), 100)
        self.a['tokenBudget'] = 1000
        self.put(self.a)
        self.complete_history()
        self.assertEqual(budget_admission(self.r, self.r.db, self.a)['tokensUsed'], 100)

    def test_old_terminal_failure_repair_is_bounded_and_exact(self):
        self.r.db.execute('CREATE TABLE runtime_items (id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)')
        for turn in ('bad', 'ambiguous', 'new'):
            self.r.analytics_event(self.r.db, self.a, 'turn/completed', {'threadId': 'thread', 'turnId': turn,
                'turn': {'id': turn, 'status': 'completed'}}, at=1, source='rollout')
        self.r.analytics_event(self.r.db, self.a, 'turn/completed', {'threadId': 'another', 'turnId': 'ambiguous',
            'turn': {'id': 'ambiguous', 'status': 'completed'}}, at=1, source='rollout')
        for turn in ('bad', 'ambiguous'):
            item = {'turnId': turn, 'nativeError': {'message': 'Usage limit reached'}, 'at': 2}
            self.r.db.execute('INSERT INTO runtime_items VALUES (?,?,?,?)',
                ('a:native-notice:error:' + turn, 'a', json.dumps(item), 2))
        repair_terminal_errors(self.r.db, limit=1)
        state = json.loads(self.r.db.execute("SELECT value FROM analytics_meta WHERE key='terminalErrorRepairV1'").fetchone()[0])
        self.assertEqual(state['cursor'], 1)
        for _ in range(5):
            repair_terminal_errors(self.r.db, limit=1)
        rows = {r[0]: json.loads(r[1]) for r in self.r.db.execute('SELECT id,record FROM analytics_turns')}
        self.assertEqual(rows['a:thread:bad']['status'], 'failed')
        self.assertEqual(rows['a:thread:bad']['terminalSource'], 'liveNoticeRepair')
        self.assertEqual(rows['a:thread:new']['status'], 'completed')
        self.assertEqual(rows['a:another:ambiguous']['status'], 'completed')
        self.assertEqual(rows['a:thread:ambiguous']['status'], 'completed')

    def test_inherited_usage_requires_exact_completed_repair_chain(self):
        a = {**self.a, 'threadId': 'fork', 'contextRepair': {'agent': 'a', 'phase': 'completed', 'newThreadId': 'fork',
             'source': {'id': 'a', 'accountKey': 'account', 'threadId': 'thread'}, 'snapshot': {'importThreadId': 'copy'}}}
        self.assertEqual(inherited_usage_threads(a), ['copy', 'thread'])
        context = {'threadId': 'fork', 'allowedSourceThreadIds': inherited_usage_threads(a)}
        record = {'type': 'token_usage_record', 'payload': {'thread_id': 'thread', 'response_id': 'same', 'usage': {'total_tokens': 10}}}
        self.assertEqual(rollout_actions(record, context, 'id', 1)[0][0], 'event')
        a['contextRepair']['source']['accountKey'] = 'another'
        self.assertEqual(inherited_usage_threads(a), [])
        self.assertEqual(rollout_actions(record, {'threadId': 'fork'}, 'id', 1)[0][0], 'coverage')

    def test_account_transfer_preserves_exact_response_identity(self):
        self.response('same', 100)
        self.a['accountKey'] = 'other-account'
        self.put(self.a)
        self.assertEqual(self.response('same', 100), 100)
        self.assertEqual(self.response('new', 10), 110)

    def test_same_agent_fork_keeps_response_identity(self):
        self.response('same', 100)
        self.a['threadId'] = 'fork'
        self.put(self.a)
        self.assertEqual(self.response('same', 100, threadId='fork'), 100)
        self.assertEqual(self.response('new', 10, threadId='fork'), 110)

    def test_new_branch_does_not_acquire_historical_charges(self):
        self.a.update(created=time.time(), threadId='fork')
        self.put(self.a)
        self.assertEqual(self.response('inherited', 1000, at=1, threadId='fork'), 0)
        self.assertEqual(self.response('new', 10, threadId='fork'), 10)
        self.assertEqual(self.notice(1010, 10, threadId='fork'), 10)

    def test_unlimited_admission_does_not_initialize_team(self):
        budget_admission(self.r, self.r.db, self.a)
        self.assertIsNone(self.r.db.execute("SELECT 1 FROM sqlite_master WHERE name='runtime_budget'").fetchone())

    def test_complete_usage_admits_again_despite_appended_settings(self):
        self.a.update(tokenBudget=1000, turnId=None)
        self.put(self.a)
        budget_admission(self.r, self.r.db, self.a)
        self.response('first', 100)
        path = self.complete_history()
        with path.open('a') as handle:
            handle.write(json.dumps({'type': 'turn_context', 'payload': {'model': 'model'}}) + '\n')
        self.assertFalse(budget_admission(self.r, self.r.db, self.a)['reached'])
        self.response('second', 100)
        self.assertEqual(budget_admission(self.r, self.r.db, self.a)['tokensUsed'], 200)
        with path.open('a') as handle:
            handle.write(json.dumps({'type': 'token_usage_record', 'payload': {}}) + '\n')
        with self.assertRaisesRegex(ValueError, 'not caught up'):
            budget_admission(self.r, self.r.db, self.a)

    def test_incomplete_usage_cannot_authorize_finite_budget(self):
        self.a['tokenBudget'] = 1000
        self.put(self.a)
        self.notice(100, 100)
        with self.assertRaisesRegex(ValueError, 'cannot be verified'):
            budget_admission(self.r, self.r.db, self.a)

    def test_conflicting_response_is_not_another_charge(self):
        self.response('same', 100)
        self.assertEqual(self.response('same', 200), 100)
        self.a['tokenBudget'] = 1000
        self.put(self.a)
        with self.assertRaisesRegex(ValueError, 'conflictingResponseUsage'):
            budget_admission(self.r, self.r.db, self.a)

    def test_error_history_does_not_erase_exact_live_failure(self):
        error = {'message': 'Usage limit reached', 'codexErrorInfo': 'usageLimitExceeded'}
        p = {'threadId': 'thread', 'turnId': 'old', 'turn': {'id': 'old', 'status': 'failed', 'error': error}}
        self.r.analytics_event(self.r.db, self.a, 'turn/completed', p, at=1)
        context = {'threadId': 'thread', 'turnId': 'old'}
        for payload in ({'type': 'task_complete', 'turn_id': 'old'},
                        {'type': 'task_complete', 'turn_id': 'old', 'error': {'message': 'Less precise error'}}):
            _, method, body, at = rollout_actions({'type': 'event_msg', 'payload': payload}, context, 'id', 2)[0]
            self.r.analytics_event(self.r.db, self.a, method, body, at=at, source='rollout')
        record = json.loads(self.r.db.execute("SELECT record FROM analytics_turns WHERE id='a:thread:old'").fetchone()[0])
        self.assertEqual(record['status'], 'failed')
        self.assertEqual(record['error'], error)
        self.assertEqual(record['terminalSource'], 'live')
        self.r.analytics_event(self.r.db, self.a, 'turn/completed', {
            'threadId': 'thread', 'turnId': 'new', 'turn': {'id': 'new', 'status': 'completed'}}, at=3)
        record = json.loads(self.r.db.execute("SELECT record FROM analytics_turns WHERE id='a:thread:new'").fetchone()[0])
        self.assertEqual(record['status'], 'completed')
        self.assertIsNone(record['error'])

    def test_error_bearing_task_complete_is_failed_without_live_notice(self):
        error = {'message': 'failure'}
        action = rollout_actions({'type': 'event_msg', 'payload': {'type': 'task_complete', 'turn_id': 't', 'error': error}},
                                 {'threadId': 'thread'}, 'id', 1)[0]
        self.assertEqual(action[2]['turn'], {'id': 't', 'status': 'failed', 'error': error})


if __name__ == '__main__':
    unittest.main()
