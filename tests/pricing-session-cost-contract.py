#!/usr/bin/env python3
"""Pricing, Claude log, and team estimate contracts use local fixtures only."""
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import threading
import time
import types
import unittest
from unittest.mock import patch
from urllib.request import urlopen

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
from codex_costs import ClaudeCostReader
from codex_pricing import PricingCatalog, lookup, price_usage
from codex_session_costs import SessionCostReader
from codex_canvas import Canvas, make_server


def catalog():
    def provider(models):
        return {"models": models}
    return {"providers": {
        "openai": provider({"gpt-6-luna": {"id": "gpt-6-luna", "cost": {
            "input": 0.1, "output": 0.5, "cache_read": 0.01, "cache_write": 0.125,
            "tiers": [{"tier": {"type": "context", "size": 272000}, "input": 2, "output": 10, "cache_read": 0.2, "cache_write": 0.25}],
        }}}),
        "anthropic": provider({"claude-opus-5-5": {"id": "claude-opus-5-5", "cost": {
            "input": 2, "output": 4, "cache_read": 0.5, "cache_write": 1,
        }}}),
    }}


class FixedPricing:
    def snapshot(self):
        return catalog()
    def wait_ready(self, timeout=8):
        return catalog()


class LoadingPricing:
    def snapshot(self):
        return None


class PricingSessionCostContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_lookup_applies_tier_and_cache_rates_and_keeps_unknown_unpriced(self):
        self.assertEqual(lookup(catalog(), "openai", "missing"), None)
        low, _, low_tier = price_usage(catalog(), "openai", "gpt-6-luna", {
            "inputTokens": 250000, "cachedInputTokens": 50000,
            "cacheWriteInputTokens": 0, "outputTokens": 10000,
        }, context_tokens=250000)
        self.assertFalse(low_tier)
        self.assertAlmostEqual(low, (200000 * .1 + 50000 * .01 + 10000 * .5) / 1_000_000)
        cost, status, tier = price_usage(catalog(), "openai", "gpt-6-luna", {
            "inputTokens": 300000, "cachedInputTokens": 50000,
            "cacheWriteInputTokens": 50000, "outputTokens": 10000,
        }, context_tokens=300000)
        self.assertEqual(status, "priced")
        self.assertTrue(tier)
        self.assertAlmostEqual(cost, (200000 * 2 + 50000 * .2 + 50000 * .25 + 10000 * 10) / 1_000_000)
        self.assertEqual(price_usage(catalog(), "openai", "missing", {})[0], None)

    def test_claude_reader_deduplicates_and_sums_today_and_30_days(self):
        now = time.time()
        projects = self.root / "claude" / "projects" / "sample"
        projects.mkdir(parents=True)
        row = {"type": "assistant", "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now)),
               "message": {"id": "msg-1", "model": "claude-opus-5-5", "usage": {
                   "input_tokens": 100, "cache_creation_input_tokens": 30,
                   "cache_read_input_tokens": 20, "output_tokens": 10}}}
        old = {**row, "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now - 86400)),
               "message": {**row["message"], "id": "msg-old"}}
        unknown = {**row, "message": {**row["message"], "id": "msg-unknown", "model": "opus[1m]"}}
        synthetic = {**row, "message": {**row["message"], "id": "synthetic", "model": "<synthetic>"}}
        (projects / "one.jsonl").write_text("\n".join(json.dumps(item) for item in (row, row, old, unknown, synthetic)))
        reader = ClaudeCostReader(self.root / "cache", self.root / "claude", FixedPricing())
        reader._refresh()
        result = reader.snapshot()
        self.assertEqual(result["data"]["coverage"], "partial")
        self.assertEqual(result["data"]["unknownModels"], ["opus[1m]"])
        self.assertAlmostEqual(result["data"]["todayUSD"], .00028)
        self.assertAlmostEqual(result["data"]["last30DaysUSD"], .00056)
        with patch("codex_costs.os.replace") as replace:
            reader._refresh()
            replace.assert_not_called()

    def test_claude_reader_refresh_interval_is_five_minutes(self):
        now = [1000.0]
        reader = ClaudeCostReader(self.root / "cache", self.root / "claude", FixedPricing(), clock=lambda: now[0])
        reader.checked = now[0]
        with patch("codex_costs.threading.Thread") as worker:
            now[0] += 299
            reader.snapshot()
            worker.assert_not_called()
            now[0] += 1
            reader.snapshot()
            worker.assert_called_once()

    def test_session_cost_keeps_codex_analytics_dedup_and_unpriced_models(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        for agent in ("lead", "worker-claude", "worker-unknown"):
            db.execute("INSERT INTO analytics_agents VALUES (?,?)", (agent, json.dumps({"id": agent, "rootId": "lead"})))
        rows = [
            ("lead", "turn-1", {"responseId": "codex-r", "model": "gpt-6-luna", "last": {},
                                 "delta": {"inputTokens": 1000, "cachedInputTokens": 100, "cacheWriteInputTokens": 0, "outputTokens": 100}}),
            ("lead", "turn-1", {"model": "gpt-6-luna", "delta": {"inputTokens": 1000, "outputTokens": 100}}),
            ("worker-unknown", "turn-3", {"responseId": "future-r", "model": "gpt-5.6-luna", "delta": {"inputTokens": 10, "outputTokens": 1}}),
        ]
        for index, (agent, turn, record) in enumerate(rows, 1):
            db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                       (index, agent, "lead", "thread-" + agent, turn, index, json.dumps(record)))
        db.commit()
        db.close()
        value = SessionCostReader(db_path, FixedPricing()).snapshot("worker-unknown")
        self.assertEqual(value["pricedSamples"], 1)
        self.assertIn("openai", value["breakdown"]["providers"])
        self.assertEqual(value["unknownModels"], ["gpt-5.6-luna"])
        self.assertAlmostEqual(value["totalUSD"], (900 * .1 + 100 * .01 + 100 * .5) / 1_000_000)
        canvas = Canvas(self.root)
        canvas.runtime = types.SimpleNamespace(lock=threading.RLock())
        with patch("codex_pricing.PricingCatalog", return_value=FixedPricing()):
            server = make_server(canvas)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                endpoint = f"http://127.0.0.1:{server.server_port}/api/session-cost?agent=worker-claude"
                response = json.load(urlopen(endpoint, timeout=4))
                self.assertEqual(response["rootId"], "lead")
                self.assertAlmostEqual(response["totalUSD"], value["totalUSD"])
            finally:
                server.shutdown()
                thread.join()
                server.server_close()

    def test_claude_session_logs_add_real_models_skip_aliases_and_dedupe_messages(self):
        db_path = self.root / "canvas.sqlite3"
        config = self.root / "claude"
        projects = config / "projects" / "project"
        projects.mkdir(parents=True)
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        lead = {"id": "lead", "rootId": "lead", "accountKey": "default", "threadId": "codex-thread"}
        worker = {"id": "worker", "rootId": "lead", "accountKey": "claude-local", "threadId": "thread-current",
                  "accountHistory": [{"provider": "claude", "accountKey": "claude-local", "threadId": "thread-history"}]}
        for key, record in (("lead", lead), ("worker", worker)):
            db.execute("INSERT INTO analytics_agents VALUES (?,?)", (key, json.dumps(record)))
        analytics = [
            {"responseId": "codex-response", "model": "gpt-6-luna", "delta": {"inputTokens": 1000,
                "cachedInputTokens": 100, "cacheWriteInputTokens": 0, "outputTokens": 100}},
            {"responseId": "claude-alias", "model": "opus[1m]", "delta": {"inputTokens": 999999, "outputTokens": 999999}},
        ]
        for index, (agent, record) in enumerate((("lead", analytics[0]), ("worker", analytics[1])), 1):
            db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                       (index, agent, "lead", "thread", "turn-" + str(index), index, json.dumps(record)))
        db.commit()
        db.close()
        stamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        usage = {"input_tokens": 100, "cache_creation_input_tokens": 30,
                 "cache_read_input_tokens": 20, "output_tokens": 10}
        def row(identity, model="claude-opus-5-5"):
            return {"type": "assistant", "timestamp": stamp,
                    "message": {"id": identity, "model": model, "usage": usage}}
        current_file = projects / "thread-current.jsonl"
        current_file.write_text("\n".join(json.dumps(item) for item in (
            row("message-1"), row("message-1"), row("synthetic-1", "<synthetic>"))))
        (projects / "thread-native.jsonl").write_text(json.dumps(row("message-2")))
        (projects / "thread-fork.jsonl").write_text(json.dumps(row("message-3")))
        (projects / "thread-history.jsonl").write_text(json.dumps(row("message-4")))
        bridge_session = self.root / "account-servers" / "claude-local" / "sessions" / "thread-current.json"
        bridge_session.parent.mkdir(parents=True)
        bridge_session.write_text(json.dumps({"nativeId": "thread-native",
                                              "historyBranches": [{"nativeId": "thread-fork"}]}))
        accounts = types.SimpleNamespace(
            data={"accounts": {"claude-local": {"provider": "claude", "home": str(config)}}},
            lock=threading.RLock(),
        )
        value = SessionCostReader(db_path, FixedPricing(), accounts=accounts,
                                  state_root=self.root).snapshot("worker")
        self.assertIn("anthropic", value["breakdown"]["providers"])
        self.assertIn("openai", value["breakdown"]["providers"])
        self.assertEqual(value["pricedSamples"], 5)
        self.assertEqual(value["unknownModels"], [])
        self.assertAlmostEqual(value["breakdown"]["providers"]["anthropic"], 4 * .00028)

    def test_session_cost_reports_loading_without_fake_unknown_models(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
        db.commit()
        db.close()
        result = SessionCostReader(db_path, LoadingPricing()).snapshot("lead")
        self.assertEqual(result["pricingState"], "loading")
        self.assertIsNone(result["totalUSD"])
        self.assertEqual(result["unknownModels"], [])

    def test_session_cost_cache_waits_30_seconds_and_keeps_at_most_16_roots(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
        db.commit()
        db.close()
        clock = [100.0]
        reader = SessionCostReader(db_path, FixedPricing(), clock=lambda: clock[0])
        first = reader.snapshot("lead")
        db = sqlite3.connect(db_path)
        db.execute("INSERT INTO analytics_usage VALUES (1,'lead','lead','thread','turn',1,?)",
                   (json.dumps({"responseId": "new", "model": "gpt-6-luna", "delta": {
                       "inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),))
        db.commit()
        db.close()
        clock[0] += 29
        cached = reader.snapshot("lead")
        self.assertEqual(cached["cacheAgeSeconds"], 29)
        self.assertEqual(cached["generation"], first["generation"])
        self.assertIsNone(cached["totalUSD"])
        clock[0] += 1
        with patch("codex_session_costs.threading.Thread") as worker:
            with patch.object(reader, "_compute", wraps=reader._compute) as compute:
                stale = reader.snapshot("lead")
                again = reader.snapshot("lead")
                self.assertEqual(stale["cacheAgeSeconds"], 30)
                self.assertTrue(stale["refreshing"])
                self.assertEqual(again["cacheAgeSeconds"], 30)
                self.assertTrue(again["refreshing"])
                worker.assert_called_once()
                compute.assert_not_called()
        reader._background_refresh("lead", "lead")
        updated = reader.snapshot("lead")
        self.assertEqual(updated["pricedSamples"], 1)
        self.assertEqual(updated["cacheAgeSeconds"], 0)

        restarted = SessionCostReader(db_path, FixedPricing(), state_root=self.root, clock=lambda: clock[0])
        clock[0] += 31
        with patch("codex_session_costs.threading.Thread") as worker:
            with patch.object(restarted, "_compute", wraps=restarted._compute) as compute:
                restored = restarted.snapshot("lead")
                self.assertEqual(restored["totalUSD"], updated["totalUSD"])
                self.assertEqual(restored["cacheAgeSeconds"], 31)
                self.assertTrue(restored["refreshing"])
                worker.assert_called_once()
                compute.assert_not_called()
        db = sqlite3.connect(db_path)
        for index in range(18):
            key = "root-" + str(index)
            db.execute("INSERT INTO analytics_agents VALUES (?,?)", (key, json.dumps({"rootId": key})))
        db.commit()
        db.close()
        for index in range(18):
            reader.snapshot("root-" + str(index))
        self.assertLessEqual(len(reader.cache), 16)

    def test_failed_catalog_refresh_keeps_last_good_copy(self):
        path = self.root / "model-pricing" / "models-dev-v1.json"
        good = PricingCatalog(self.root, fetch=catalog)
        good._refresh()
        before = json.loads(path.read_text())["catalog"]
        stale = PricingCatalog(self.root, fetch=lambda: (_ for _ in ()).throw(OSError("offline")))
        stale._refresh()
        self.assertEqual(stale.snapshot(), before)
        self.assertEqual(json.loads(path.read_text())["catalog"], before)

    def test_failed_first_fetch_is_limited_to_one_attempt_per_day(self):
        calls = []
        pricing = PricingCatalog(self.root, fetch=lambda: (calls.append(True), (_ for _ in ()).throw(OSError("offline")))[1])
        pricing.last_attempt = pricing.clock()
        pricing._refresh()
        pricing.snapshot()
        self.assertEqual(len(calls), 1)

    def test_scanner_copy_keeps_native_parser_threshold_semantics(self):
        pricing = PricingCatalog(self.root, fetch=catalog)
        pricing._refresh()
        scanner_path = self.root / "scanner" / "model-pricing" / "models-dev-v1.json"
        pricing.write_scanner_copy(self.root / "scanner")
        scanner = json.loads(scanner_path.read_text())
        model = scanner["catalog"]["providers"]["openai"]["models"]["gpt-6-luna"]
        self.assertNotIn("context_over_200k", model["cost"])
        self.assertEqual(model["cost"]["tiers"][0]["tier"]["size"], 272000)


if __name__ == "__main__":
    unittest.main()
