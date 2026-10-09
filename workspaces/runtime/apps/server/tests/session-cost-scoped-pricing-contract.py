#!/usr/bin/env python3
"""Reuse rates for one cost calculation without changing prices or freshness."""
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
import codex_pricing as pricing
from codex_session_costs import SessionCostReader


def catalog():
    return {"providers": {
        "openai": {"models": {"gpt-test": {"cost": {
            "input": 2, "output": 4, "cache_read": .5, "cache_write": 1,
            "tiers": [
                {"tier": {"type": "context", "size": 200},
                 "input": 20, "output": 40, "cache_read": 5, "cache_write": 10},
                {"tier": {"type": "context", "size": 400}, "output": 80},
            ],
        }}}},
        "anthropic": {"models": {"claude-opus-5-5": {"cost": {
            "input": 2, "output": 4, "cache_read": .5, "cache_write": 1,
        }}}},
    }}


class FixedPricing:
    def __init__(self):
        self.value = catalog()

    def snapshot(self):
        return self.value

    def refresh_missing(self):
        pass


class ScopedPricingContract(unittest.TestCase):
    usage = {"inputTokens": 100, "cachedInputTokens": 20,
             "cacheWriteInputTokens": 10, "outputTokens": 5}

    def test_exact_totals_and_context_boundaries(self):
        source = catalog()
        scoped = pricing._scoped_pricer(source)
        for context, uncached, expected, tier in (
                (199, False, .00018, False),
                (200, False, .0018, True),
                (399, True, .0024, True),
                (400, False, .00056, True),
                (None, True, .00024, False)):
            with self.subTest(context=context, uncached=uncached):
                options = {"context_tokens": context, "input_tokens_are_uncached": uncached}
                actual = scoped("openai", "gpt-test", self.usage, **options)
                self.assertEqual(actual, pricing.price_usage(source, "openai", "gpt-test", self.usage, **options))
                self.assertAlmostEqual(actual[0], expected)
                self.assertEqual(actual[1:], ("priced", tier))
        rates = source["providers"]["openai"]["models"]["gpt-test"]["cost"]
        rates.pop("tiers")
        rates["context_over_200k"] = {"input": 20}
        legacy = pricing._scoped_pricer(source)
        for context, expected, tier in ((200000, .00018, False), (200001, .00144, True)):
            actual = legacy("openai", "gpt-test", self.usage, context_tokens=context)
            self.assertAlmostEqual(actual[0], expected)
            self.assertEqual(actual[1:], ("priced", tier))

    def test_missing_rates_and_usage_keep_their_status(self):
        source = catalog()
        scoped = pricing._scoped_pricer(source)
        for field in ("inputTokens", "cachedInputTokens", "outputTokens"):
            value = {key: amount for key, amount in self.usage.items() if key != field}
            self.assertEqual(scoped("openai", "gpt-test", value), (None, "incomplete", False))
        self.assertEqual(scoped("openai", "missing", self.usage), (None, "unpriced", False))
        self.assertEqual(scoped("openai", {}, self.usage), (None, "unpriced", False))
        value = {key: amount for key, amount in self.usage.items() if key != "cacheWriteInputTokens"}
        self.assertEqual(scoped("openai", "gpt-test", value), (.00019, "priced", False))
        source["providers"]["openai"]["models"]["no-cache"] = {"cost": {"input": 2, "output": 4}}
        self.assertEqual(scoped("openai", "no-cache", self.usage), (None, "incomplete", False))

    def test_provider_keys_and_unknown_models_lookup_once_per_scope(self):
        source = catalog()
        source["providers"]["anthropic"]["models"]["gpt-test"] = {"cost": {
            "input": 4, "output": 8, "cache_read": 1, "cache_write": 2}}
        cases = (("openai", "gpt-test"), ("anthropic", "gpt-test"),
                 ("anthropic", "claude-opus-5-5"), ("openai", "missing"))
        expected = [pricing.price_usage(source, *case, self.usage) for case in cases]
        with patch.object(pricing, "lookup", wraps=pricing.lookup) as lookup:
            scoped = pricing._scoped_pricer(source)
            for index in range(33943):
                self.assertEqual(scoped(*cases[index % len(cases)], self.usage), expected[index % len(cases)])
            self.assertEqual(lookup.call_count, len(cases))

    def test_public_price_reads_edits_and_each_scope_reads_new_rates(self):
        source = catalog()
        first = pricing._scoped_pricer(source)
        self.assertEqual(first("openai", "gpt-test", self.usage)[0], .00018)
        source["providers"]["openai"]["models"]["gpt-test"]["cost"]["input"] = 3
        self.assertEqual(pricing.price_usage(source, "openai", "gpt-test", self.usage)[0], .00025)
        self.assertEqual(pricing._scoped_pricer(source)("openai", "gpt-test", self.usage)[0], .00025)

    def test_custom_price_callback_keeps_original_arguments_and_exceptions(self):
        source = catalog()
        calls = []
        def custom(*args, **options):
            calls.append((args, options))
            return (7, "priced", False)
        scoped = pricing._scoped_pricer(source, price=custom)
        self.assertEqual(scoped("openai", "gpt-test", self.usage, context_tokens=200), (7, "priced", False))
        self.assertEqual(calls, [((source, "openai", "gpt-test", self.usage), {"context_tokens": 200})])
        def failed(*args, **options):
            raise RuntimeError("custom failure")
        with self.assertRaisesRegex(RuntimeError, "custom failure"):
            pricing._scoped_pricer(source, price=failed)("openai", "gpt-test", self.usage)

    def test_reader_reuses_rates_for_sql_groups_and_claude_logs_then_sees_catalog_edit(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            path = root / "canvas.sqlite3"
            profile = root / "claude"
            project = profile / "projects" / "sample"
            project.mkdir(parents=True)
            (project / "thread.jsonl").write_text(json.dumps({
                "type": "assistant", "timestamp": "2026-10-07T12:00:00Z",
                "message": {"id": "native-receipt", "model": "claude-opus-5-5", "usage": {
                    "input_tokens": 100, "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 10, "output_tokens": 5}}}) + "\n")
            with closing(sqlite3.connect(path)) as db, db:
                db.executescript("""
                  CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
                  CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
                  CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,
                    root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
                  CREATE INDEX usage_root ON analytics_usage(root,seq);
                """)
                db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({
                    "rootId": "lead", "provider": "claude", "accountKey": "claude", "threadId": "thread"}),))
                for seq in range(1, 9):
                    record = {"model": "gpt-test" if seq % 2 else "claude-opus-5-5",
                              "accountKey": "claude", "responseId": str(seq),
                              "delta": {**self.usage, "inputTokens": 100 + seq}}
                    db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                               (seq, "lead", "lead", "thread", str(seq), seq, json.dumps(record)))
            prices = FixedPricing()
            reader = SessionCostReader(path, prices, {"claude": {"provider": "claude", "home": str(profile)}})
            with patch.object(pricing, "lookup", wraps=pricing.lookup) as lookup:
                first = reader._compute_shared("lead", "lead", refresh=True)
                self.assertEqual(lookup.call_count, 2)
            expected = .00024  # Native Claude usage counts input separately from its caches.
            for seq in range(1, 9):
                expected += pricing.price_usage(prices.value, "openai" if seq % 2 else "anthropic",
                    "gpt-test" if seq % 2 else "claude-opus-5-5", {**self.usage, "inputTokens": 100 + seq})[0]
            self.assertEqual(first["pricedSamples"], 9)
            self.assertAlmostEqual(first["totalUSD"], expected)
            prices.value["providers"]["openai"]["models"]["gpt-test"]["cost"]["input"] = 3
            with patch.object(pricing, "lookup", wraps=pricing.lookup) as lookup:
                second = reader._compute_shared("lead", "lead", refresh=True)
                self.assertEqual(lookup.call_count, 2)
            self.assertAlmostEqual(second["totalUSD"] - first["totalUSD"], sum(70 + seq for seq in (1, 3, 5, 7)) / 1_000_000)


if __name__ == "__main__":
    unittest.main()
