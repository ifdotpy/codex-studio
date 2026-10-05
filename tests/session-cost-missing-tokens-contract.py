#!/usr/bin/env python3
"""Legacy usage fields do not prevent a team cost estimate."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import closing
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import codex_session_costs
from codex_pricing import price_usage


class FixedPricing:
    def __init__(self):
        self.missing_refreshes = 0

    def snapshot(self):
        return {"providers": {"anthropic": {"models": {
            "claude-opus-5-5": {"id": "claude-opus-5-5", "cost": {
                "input": 2, "output": 4, "cache_read": .5, "cache_write": 1,
                "tiers": [{"tier": {"type": "context", "size": 200},
                           "input": 20, "output": 40, "cache_read": 5,
                           "cache_write": 10}],
            }},
        }}}}

    def refresh_missing(self):
        self.missing_refreshes += 1


class SessionCostMissingTokensContract(unittest.TestCase):
    def estimate(self, records):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "canvas.sqlite3"
            with closing(sqlite3.connect(path)) as db, db:
                db.executescript("""
                  CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
                  CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
                  CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,
                    root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
                  CREATE INDEX usage_root ON analytics_usage(root,seq);
                """)
                db.execute("INSERT INTO analytics_agents VALUES ('lead',?)",
                           (json.dumps({"rootId": "lead"}),))
                for seq, record in enumerate(records, 1):
                    db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                               (seq, "lead", "lead", "thread", str(seq), seq,
                                json.dumps({"model": "claude-opus-5-5",
                                            "responseId": str(seq), **record})))
            pricing = FixedPricing()
            reader = codex_session_costs.SessionCostReader(path, pricing)
            outcomes = []

            def priced(*args, **kwargs):
                result = price_usage(*args, **kwargs)
                outcomes.append((kwargs.get("context_tokens"), result))
                return result

            with patch.object(codex_session_costs, "price_usage", side_effect=priced):
                result = reader.snapshot("lead", wait=True)
            self.assertEqual(pricing.missing_refreshes, 0)
            self.assertFalse(reader.inflight)
            self.assertEqual(result["pricingState"], "ready")
            return result, outcomes

    def test_legacy_missing_write_prices_zero_with_delta_and_last(self):
        for source in ("delta", "last"):
            for explicit_null in (False, True):
                with self.subTest(source=source, explicit_null=explicit_null):
                    usage = {"inputTokens": 100, "cachedInputTokens": 20,
                             "outputTokens": 10}
                    if explicit_null:
                        usage["cacheWriteInputTokens"] = None
                    result, outcomes = self.estimate([
                        {source: usage, "inputTokensAreUncached": True}])
                    self.assertEqual(result["pricedSamples"], 1)
                    self.assertAlmostEqual(result["totalUSD"], .00025)
                    self.assertEqual(result["unknownModels"], [])
                    self.assertEqual(outcomes, [(120, (.00025, "priced", False))])

    def test_missing_input_is_incomplete(self):
        for explicit_null in (False, True):
            with self.subTest(explicit_null=explicit_null):
                usage = {"cachedInputTokens": 20, "cacheWriteInputTokens": 30,
                         "outputTokens": 10}
                if explicit_null:
                    usage["inputTokens"] = None
                result, outcomes = self.estimate([
                    {"last": usage, "inputTokensAreUncached": True}])
                self.assertEqual(result["pricedSamples"], 0)
                self.assertIsNone(result["totalUSD"])
                self.assertEqual(result["unknownModels"], ["claude-opus-5-5"])
                self.assertEqual(outcomes, [(None, (None, "incomplete", False))])

    def test_missing_cached_input_is_incomplete(self):
        for explicit_null in (False, True):
            with self.subTest(explicit_null=explicit_null):
                usage = {"inputTokens": 100, "cacheWriteInputTokens": 30,
                         "outputTokens": 10}
                if explicit_null:
                    usage["cachedInputTokens"] = None
                result, outcomes = self.estimate([
                    {"delta": usage, "inputTokensAreUncached": True}])
                self.assertEqual(result["pricedSamples"], 0)
                self.assertIsNone(result["totalUSD"])
                self.assertEqual(result["unknownModels"], ["claude-opus-5-5"])
                self.assertEqual(outcomes[0][1], (None, "incomplete", False))

    def test_incomplete_row_does_not_discard_the_other_team_cost(self):
        result, outcomes = self.estimate([
            {"last": {"cachedInputTokens": 20, "outputTokens": 10},
             "inputTokensAreUncached": True},
            {"delta": {"inputTokens": 100, "cachedInputTokens": 20,
                       "outputTokens": 10}, "inputTokensAreUncached": True},
        ])
        self.assertEqual(result["pricedSamples"], 1)
        self.assertAlmostEqual(result["totalUSD"], .00025)
        self.assertEqual(result["unknownModels"], ["claude-opus-5-5"])
        self.assertEqual(sorted(outcome[1][1] for outcome in outcomes),
                         ["incomplete", "priced"])

    def test_missing_write_preserves_the_inclusive_context_tier(self):
        result, outcomes = self.estimate([
            {"delta": {"inputTokens": 180, "cachedInputTokens": 20,
                       "outputTokens": 10}, "inputTokensAreUncached": True}])
        self.assertAlmostEqual(result["totalUSD"], .0041)
        self.assertEqual(outcomes, [(200, (.0041, "priced", True))])

    def test_uncached_input_includes_cache_reads_and_writes_for_the_tier(self):
        result, outcomes = self.estimate([
            {"delta": {"inputTokens": 150, "cachedInputTokens": 20,
                       "cacheWriteInputTokens": 30, "outputTokens": 10},
             "inputTokensAreUncached": True}])
        self.assertAlmostEqual(result["totalUSD"], .0038)
        self.assertEqual(outcomes, [(200, (.0038, "priced", True))])

    def test_total_input_keeps_its_context_size_and_cache_deductions(self):
        result, outcomes = self.estimate([
            {"delta": {"inputTokens": 150, "cachedInputTokens": 20,
                       "cacheWriteInputTokens": 30, "outputTokens": 10},
             "inputTokensAreUncached": False}])
        self.assertAlmostEqual(result["totalUSD"], .00028)
        self.assertEqual(outcomes, [(150, (.00028, "priced", False))])


if __name__ == "__main__":
    unittest.main()
