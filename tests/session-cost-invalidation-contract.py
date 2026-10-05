#!/usr/bin/env python3
"""Duplicate notices must not invalidate the complete session cost history."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import copy
import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / 'scripts'))
from codex_analytics import AnalyticsMixin
from codex_session_costs import SessionCostReader

spec = importlib.util.spec_from_file_location('cost_fixture', ROOT / 'tests/pricing-session-cost-contract.py')
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Capture(AnalyticsMixin):
    def records(self, _db, _table):
        return []


class SessionCostInvalidationContract(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='studio-cost-invalidation-')
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / 'canvas.sqlite3'
        self.db = sqlite3.connect(self.path)
        self.db.row_factory = sqlite3.Row
        self.addCleanup(self.db.close)
        self.capture = Capture()
        self.capture.analytics_init(self.db)
        self.db.execute('CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL)')
        self.agent = {'id': 'lead', 'rootId': 'lead', 'accountKey': 'default', 'provider': 'codex',
                      'threadId': 'thread', 'turnId': 'turn', 'model': 'gpt-6-luna', 'created': 0}
        self.db.execute('INSERT INTO runtime_agents VALUES (?,?)', ('lead', json.dumps(self.agent)))
        self.db.commit()
        self.reader = SessionCostReader(self.path, fixture.FixedPricing())
        self.projections = []
        project = self.reader._cost_usage_groups
        def observed(db, root):
            self.projections.append(root)
            return project(db, root)
        self.reader._cost_usage_groups = observed

    @staticmethod
    def notice(**changes):
        return {'threadId': 'thread', 'turnId': 'turn', 'tokenUsage': {
            'total': {'inputTokens': 1000, 'outputTokens': 100, 'totalTokens': 1100},
            'last': {'inputTokens': 1000, 'cachedInputTokens': 0, 'cacheWriteInputTokens': 0,
                     'outputTokens': 100, 'totalTokens': 1100}}, **changes}

    def event(self, params=None, *, at=100, **options):
        with self.db:
            return self.capture.analytics_event(self.db, self.agent, 'thread/tokenUsage/updated',
                                               params or self.notice(), at=at, **options)

    def estimate(self):
        return self.reader._compute_shared('lead', 'lead', refresh=True)

    def state(self):
        return self.reader._usage_state(self.db, 'lead')

    def usage(self):
        return [tuple(row) for row in self.db.execute('SELECT seq,record FROM analytics_usage ORDER BY seq')]

    def claude_notice(self):
        self.agent.update(provider='claude', model='claude-opus-5-5')
        return self.notice(responseId='response', usageSource='claudeResponse',
                           requestUsage={'inputTokens': 100, 'cachedInputTokens': 20,
                                         'cacheWriteInputTokens': 30, 'outputTokens': 10},
                           model='claude-opus-5-5')

    def test_exact_claude_duplicate_preserves_timestamp_budget_and_cost_projection(self):
        notice = self.claude_notice()
        spent = self.event(notice)
        initial, original = self.state(), self.usage()
        first = self.estimate()
        duplicate_spent = self.event(copy.deepcopy(notice), at=200)
        second = self.estimate()
        self.assertEqual(self.usage(), original)
        self.assertEqual(self.state(), initial)
        self.assertEqual(self.projections, ['lead'])
        self.assertEqual(second['totalUSD'], first['totalUSD'])
        self.assertEqual((spent, duplicate_spent), (160, 160))
        self.assertEqual(self.db.execute('SELECT at FROM analytics_usage').fetchone()[0], 100)
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)
        self.assertEqual(self.db.execute('SELECT sum(count) FROM analytics_notifications').fetchone()[0], 2)

    def test_claude_usage_and_model_corrections_invalidate_the_same_sequence(self):
        notice = self.claude_notice()
        self.event(notice)
        first = self.estimate()
        notice['requestUsage']['cachedInputTokens'] = 200
        self.event(notice, at=101)
        corrected = self.estimate()
        self.assertGreater(corrected['totalUSD'], first['totalUSD'])
        notice['model'] = 'gpt-6-luna'
        self.event(notice, at=102)
        remapped = self.estimate()
        self.assertNotEqual(remapped['totalUSD'], corrected['totalUSD'])
        self.assertEqual(self.state(), {'maxSeq': 1, 'generation': 3})
        self.assertEqual(self.projections, ['lead', 'lead', 'lead'])
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)

    def test_claude_duplicate_after_notice_observations_preserves_the_saved_receipt(self):
        notice = self.claude_notice()
        self.event(notice)
        first = self.estimate()
        for index in range(3):
            observed = self.notice(responseId='response')
            observed['tokenUsage']['total']['totalTokens'] += index * 100
            observed_spent = self.event(observed, at=101 + index * 2)
            original, initial = self.usage(), self.state()
            budget = [tuple(row) for row in self.db.execute('SELECT * FROM runtime_budget_usage ORDER BY id')]
            self.assertEqual(self.event(notice, at=102 + index * 2), observed_spent)
            second = self.estimate()
            self.assertEqual(self.usage(), original)
            self.assertEqual(self.state(), initial)
            self.assertEqual([tuple(row) for row in self.db.execute('SELECT * FROM runtime_budget_usage ORDER BY id')], budget)
            self.assertEqual(second['totalUSD'], first['totalUSD'])
        self.assertEqual(self.projections, ['lead'])
        self.assertEqual(self.db.execute('SELECT at FROM analytics_usage').fetchone()[0], 100)
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 4)
        self.assertEqual(self.db.execute('SELECT sum(count) FROM analytics_notifications').fetchone()[0], 7)

    def test_claude_duplicate_preserves_window_from_two_ordinary_notices(self):
        notice = self.claude_notice()
        self.event(notice)
        first = self.estimate()
        for index in range(2):
            observed = self.notice(responseId='response')
            observed['tokenUsage']['modelContextWindow'] = 200000
            self.event(observed, at=101 + index * 2)
            original, initial = self.usage(), self.state()
            self.event(notice, at=102 + index * 2)
            self.assertEqual(self.usage(), original)
            self.assertEqual(self.state(), initial)
            self.assertEqual(json.loads(self.usage()[0][1])['modelContextWindow'], 200000)
            self.assertEqual(self.estimate()['totalUSD'], first['totalUSD'])
        original = self.usage()
        for window in (200000, None):
            notice['tokenUsage']['modelContextWindow'] = window
            self.event(notice, at=106)
            self.assertEqual(self.usage(), original)
            self.assertEqual(self.estimate()['totalUSD'], first['totalUSD'])
        self.assertEqual(self.projections, ['lead'])
        self.assertEqual(self.db.execute('SELECT at FROM analytics_usage').fetchone()[0], 100)

    def test_claude_missing_raw_window_preserves_known_window_and_changed_window_updates(self):
        notice = self.claude_notice()
        notice['tokenUsage']['modelContextWindow'] = 200000
        self.event(notice)
        first = self.estimate()
        original = self.usage()
        notice['tokenUsage'].pop('modelContextWindow')
        self.event(notice, at=101)
        self.assertNotIn('modelContextWindow', notice['tokenUsage'])
        self.assertEqual(self.usage(), original)
        self.assertEqual(self.state(), {'maxSeq': 1, 'generation': 1})
        self.assertEqual(self.estimate()['totalUSD'], first['totalUSD'])
        notice['tokenUsage']['modelContextWindow'] = 300000
        self.event(notice, at=102)
        self.estimate()
        record = json.loads(self.usage()[0][1])
        self.assertEqual(record['modelContextWindow'], 300000)
        self.assertEqual(record['raw']['modelContextWindow'], 300000)
        self.assertEqual(self.state(), {'maxSeq': 1, 'generation': 2})
        notice['tokenUsage'].pop('modelContextWindow')
        original = self.usage()
        self.event(notice, at=103)
        self.assertEqual(self.usage(), original)
        self.estimate()
        self.assertEqual(self.projections, ['lead', 'lead'])

    def test_claude_account_turn_and_thread_changes_keep_exact_scope(self):
        notice = self.claude_notice()
        self.event(notice)
        self.estimate()
        self.agent['accountKey'] = 'other-account'
        self.event(notice, at=101)
        self.estimate()
        self.assertEqual(json.loads(self.usage()[0][1])['accountKey'], 'other-account')
        notice['turnId'] = 'other-turn'
        self.event(notice, at=102)
        self.estimate()
        self.assertEqual(self.db.execute('SELECT turn FROM analytics_usage').fetchone()[0], 'other-turn')
        notice['threadId'] = 'other-thread'
        self.event(notice, at=103)
        self.estimate()
        self.assertEqual(self.state()['generation'], 4)
        self.assertGreater(self.state()['maxSeq'], 1)
        self.assertEqual(len(self.usage()), 2)
        self.assertEqual(self.projections, ['lead'] * 4)
        self.assertEqual([tuple(row) for row in self.db.execute('SELECT thread FROM analytics_usage ORDER BY seq')],
                         [('thread',), ('other-thread',)])

    def test_claude_root_move_invalidates_both_roots_once(self):
        notice = self.claude_notice()
        self.event(notice)
        self.estimate()
        self.agent['rootId'] = 'other-root'
        self.event(notice, at=101)
        self.assertEqual(self.state(), {'maxSeq': 0, 'generation': 2})
        self.assertEqual(self.reader._usage_state(self.db, 'other-root'), {'maxSeq': 1, 'generation': 1})
        self.assertIsNone(self.estimate()['totalUSD'])
        moved = self.reader._compute_shared('lead', 'other-root', refresh=True)
        self.assertEqual(moved['pricedSamples'], 1)
        original = self.usage()
        self.event(notice, at=102)
        self.assertEqual(self.usage(), original)
        self.assertEqual(self.state()['generation'], 2)
        self.assertEqual(self.reader._usage_state(self.db, 'other-root')['generation'], 1)
        self.assertEqual(self.db.execute("SELECT value FROM analytics_meta WHERE key='usageGeneration'").fetchone()[0], '2')

    def test_claude_legacy_record_and_source_enrichment_invalidate(self):
        notice = self.claude_notice()
        self.event(notice)
        record = json.loads(self.usage()[0][1])
        record.pop('requestUsage')
        with self.db:
            self.db.execute('UPDATE analytics_usage SET record=?', (json.dumps(record),))
        self.estimate()
        self.event(notice, at=101)
        self.estimate()
        self.assertEqual(json.loads(self.usage()[0][1])['requestUsage'], notice['requestUsage'])
        self.event(notice, at=102, source='rollout')
        self.estimate()
        self.assertEqual(json.loads(self.usage()[0][1])['source'], 'rollout')
        self.assertEqual(self.state(), {'maxSeq': 1, 'generation': 3})
        self.assertEqual(self.projections, ['lead'] * 3)

    def test_claude_old_root_generation_failure_rolls_back_the_move(self):
        notice = self.claude_notice()
        self.event(notice)
        original = self.usage()
        with self.db:
            self.db.execute("CREATE TRIGGER reject_old_root BEFORE INSERT ON analytics_usage_roots "
                            "WHEN NEW.root='lead' BEGIN SELECT RAISE(ABORT,'old root failure'); END")
        self.agent['rootId'] = 'other-root'
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'old root failure'):
            self.event(notice, at=101)
        self.assertEqual(self.usage(), original)
        self.assertEqual(self.state(), {'maxSeq': 1, 'generation': 1})
        self.assertEqual(self.reader._usage_state(self.db, 'other-root'), {'maxSeq': 0, 'generation': 0})
        self.assertEqual(self.db.execute("SELECT value FROM analytics_meta WHERE key='usageGeneration'").fetchone()[0], '1')

    def test_duplicate_reuses_cost_projection_and_keeps_exact_budget_and_notices(self):
        spent = self.event()
        original_rows, original_state = self.usage(), self.state()
        first = self.estimate()
        duplicate_spent = self.event(at=101)
        second = self.estimate()
        self.assertEqual(self.usage(), original_rows)
        self.assertEqual(self.projections, ['lead'], 'A duplicate must not read the full cost history again')
        self.assertEqual(self.state(), original_state)
        self.assertEqual(second['totalUSD'], first['totalUSD'])
        self.assertEqual((spent, duplicate_spent), (1100, 1100))
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)
        self.assertEqual(self.db.execute('SELECT sum(count) FROM analytics_notifications').fetchone()[0], 2)

    def test_notice_counters_do_not_replace_authoritative_usage_or_refresh_cost(self):
        authoritative = self.notice(responseId='response', rawTokenUsageRecord={'response_id': 'response'})
        self.event(authoritative)
        first, initial = self.estimate(), self.state()
        observed = self.notice(responseId='response')
        observed['tokenUsage']['total']['totalTokens'] = 2200
        observed['tokenUsage']['last']['cachedInputTokens'] = 500
        self.event(observed, at=101)
        second = self.estimate()
        self.assertEqual(self.projections, ['lead'])
        self.assertEqual(self.state(), initial)
        self.assertEqual(second['totalUSD'], first['totalUSD'])
        record = json.loads(self.usage()[0][1])
        self.assertEqual(record['noticeTotal']['totalTokens'], 2200)
        self.assertEqual(record['delta']['cachedInputTokens'], 0)

    def test_new_billable_usage_refreshes_cost_with_one_new_saved_row(self):
        self.event()
        first = self.estimate()
        changed = self.notice()
        changed['tokenUsage']['total']['totalTokens'] = 2200
        self.event(changed, at=101)
        second = self.estimate()
        self.assertEqual(self.state(), {'maxSeq': 2, 'generation': 2})
        self.assertEqual(self.projections, ['lead', 'lead'])
        self.assertEqual(second['pricedSamples'], 2)
        self.assertAlmostEqual(second['totalUSD'], first['totalUSD'] * 2)

    def test_same_seq_model_and_cached_token_enrichment_refresh_exact_cost(self):
        for model, cached in [('claude-opus-5-5', 0), ('gpt-6-luna', 500)]:
            with self.subTest(model=model, cached=cached):
                self.db.execute('DELETE FROM analytics_usage')
                self.db.execute('DELETE FROM analytics_usage_roots')
                self.db.commit()
                self.reader.cache.clear()
                self.projections.clear()
                self.event()
                initial_seq = self.state()['maxSeq']
                first = self.estimate()
                correction = self.notice(responseId='response', model=model,
                    rawTokenUsageRecord={'response_id': 'response'})
                correction['tokenUsage']['last']['cachedInputTokens'] = cached
                self.event(correction, at=101)
                second = self.estimate()
                self.assertEqual(self.state(), {'maxSeq': initial_seq, 'generation': 2})
                self.assertEqual(self.projections, ['lead', 'lead'])
                self.assertEqual(second['pricedSamples'], 1)
                self.assertNotEqual(second['totalUSD'], first['totalUSD'])
                row = json.loads(self.usage()[0][1])
                self.assertEqual((row['model'], row['delta']['cachedInputTokens']), (model, cached))

    def test_same_seq_claude_response_correction_keeps_generation_and_refresh(self):
        request = {'inputTokens': 100, 'cachedInputTokens': 20, 'cacheWriteInputTokens': 30, 'outputTokens': 10}
        first_notice = self.notice(responseId='response', usageSource='claudeResponse',
                                  requestUsage=request, model='claude-opus-5-5')
        self.event(first_notice)
        first, initial = self.estimate(), self.state()
        corrected = copy.deepcopy(first_notice)
        corrected['requestUsage']['cachedInputTokens'] = 200
        self.agent['provider'] = 'claude'
        self.event(corrected, at=101)
        second = self.estimate()
        self.assertEqual(self.state(), {'maxSeq': initial['maxSeq'], 'generation': 2})
        self.assertEqual(self.projections, ['lead', 'lead'])
        self.assertGreater(second['totalUSD'], first['totalUSD'])
        row = json.loads(self.usage()[0][1])
        self.assertEqual(row['provider'], 'claude')
        self.assertEqual(row['delta']['cachedInputTokens'], 200)

    def test_rejected_usage_keeps_cost_cache_and_returns_precommitted_budget(self):
        self.event()
        first, initial = self.estimate(), self.state()
        result = self.event(self.notice(usageSource='claudeResponse', responseId='response',
                                      requestUsage=None), at=101, budget_capture_value=67)
        second = self.estimate()
        self.assertEqual(result, 67)
        self.assertEqual(self.projections, ['lead'])
        self.assertEqual(self.state(), initial)
        self.assertEqual(second['totalUSD'], first['totalUSD'])
        self.assertEqual(self.db.execute('SELECT sum(count) FROM analytics_notifications').fetchone()[0], 2)
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)

    def test_generation_and_usage_rollback_together_then_exact_retry_commits_once(self):
        self.db.execute("CREATE TRIGGER reject_generation BEFORE INSERT ON analytics_usage_roots "
                        "BEGIN SELECT RAISE(ABORT,'fixture generation failure'); END")
        self.db.commit()
        with self.assertRaisesRegex(sqlite3.IntegrityError, 'fixture generation failure'):
            self.event()
        self.assertEqual(self.usage(), [])
        self.assertEqual(self.state(), {'maxSeq': 0, 'generation': 0})
        self.assertIsNone(self.db.execute("SELECT value FROM analytics_meta WHERE key='usageGeneration'").fetchone())
        self.db.execute('DROP TRIGGER reject_generation')
        self.db.commit()
        self.event()
        self.event(at=101)
        self.assertEqual(len(self.usage()), 1)
        self.assertEqual(self.state()['generation'], 1)
        self.assertEqual(self.db.execute('SELECT count(*) FROM runtime_budget_usage').fetchone()[0], 1)


if __name__ == '__main__':
    unittest.main()
