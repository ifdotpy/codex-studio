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
