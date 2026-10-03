#!/usr/bin/env python3
"""Pricing, Claude log, and team estimate contracts use local fixtures only."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

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
    missing_refreshes = 0
    def snapshot(self):
        return catalog()
    def refresh_missing(self):
        FixedPricing.missing_refreshes += 1
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
        unknown = {**row, "message": {**row["message"], "id": "msg-unknown", "model": "claude-mystery-9"}}
        alias = {**row, "message": {**row["message"], "id": "msg-alias", "model": "opus[1m]"}}
        synthetic = {**row, "message": {**row["message"], "id": "synthetic", "model": "<synthetic>"}}
        (projects / "one.jsonl").write_text("\n".join(json.dumps(item) for item in (row, row, old, unknown, alias, synthetic)))
        reader = ClaudeCostReader(self.root / "cache", self.root / "claude", FixedPricing())
        reader._refresh()
        result = reader.snapshot()
        self.assertEqual(result["data"]["coverage"], "partial")
        # A Claude Code alias prices as its family model; an unknown model stays unpriced.
        self.assertEqual(result["data"]["unknownModels"], ["claude-mystery-9"])
        self.assertAlmostEqual(result["data"]["todayUSD"], .00056)
        self.assertAlmostEqual(result["data"]["last30DaysUSD"], .00084)
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

    def test_session_cost_preserves_legacy_views_during_analytics_copy(self):
        db_path = self.root / "canvas.sqlite3"
        for filename in ("canvas.sqlite3", "analytics.sqlite3"):
            with sqlite3.connect(self.root / filename) as db:
                db.executescript("""
                  CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
                  CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
                  CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, id TEXT UNIQUE,
                    agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
                """)
                db.execute("INSERT INTO analytics_agents VALUES ('lead',?)",
                           (json.dumps({"rootId": "lead"}),))
                response = "legacy" if filename == "canvas.sqlite3" else "copied"
                usage = {"responseId": response, "model": "gpt-6-luna",
                         "delta": {"inputTokens": 1000, "cachedInputTokens": 0,
                                   "cacheWriteInputTokens": 0, "outputTokens": 0}}
                seq = 1 if response == "legacy" else 2
                db.execute("INSERT INTO analytics_usage VALUES (?,?, 'lead','lead','thread',?,1,?)",
                           (seq, response, response, json.dumps(usage)))
        reader = SessionCostReader(db_path, FixedPricing())
        value = reader.snapshot("lead", wait=True)
        self.assertEqual(value["pricedSamples"], 2)
        self.assertAlmostEqual(value["totalUSD"], .0002)
        db = reader._connect()
        try:
            self.assertEqual(db.execute("PRAGMA temp_store").fetchone()[0], 1)
            self.assertEqual(db.execute("SELECT count(*) FROM analytics_usage").fetchone()[0], 2)
        finally:
            db.close()

    def test_session_cost_keeps_codex_analytics_dedup_and_unpriced_models(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
          CREATE TABLE analytics_usage_roots (root TEXT PRIMARY KEY, generation INTEGER NOT NULL);
        """)
        for agent in ("lead", "worker-claude", "worker-unknown"):
            db.execute("INSERT INTO analytics_agents VALUES (?,?)", (agent, json.dumps({"id": agent, "rootId": "lead"})))
        rows = [
            ("lead", "turn-1", {"responseId": "codex-r", "model": "gpt-6-luna", "last": {},
                                 "delta": {"inputTokens": 1000, "cachedInputTokens": 100, "cacheWriteInputTokens": 0, "outputTokens": 100}}),
            ("lead", "turn-1", {"model": "gpt-6-luna", "delta": {"inputTokens": 1000, "outputTokens": 100}}),
            ("worker-unknown", "turn-3", {"responseId": "future-r", "model": "gpt-5.6-luna", "delta": {"inputTokens": 10, "outputTokens": 1}}),
            ("lead", "turn-2", {"responseId": "imported-r", "model": None, "agentId": "lead", "threadId": "thread-lead",
                                "turnId": "turn-2", "delta": {"inputTokens": 1000, "cachedInputTokens": 0,
                                                             "cacheWriteInputTokens": 0, "outputTokens": 0}}),
            ("lead", "turn-4", {"responseId": "later-r", "model": "gpt-6-luna", "agentId": "lead", "threadId": "thread-lead",
                                "turnId": "turn-4", "delta": {"inputTokens": 0, "cachedInputTokens": 0,
                                                             "cacheWriteInputTokens": 0, "outputTokens": 0}}),
        ]
        for index, (agent, turn, record) in enumerate(rows, 1):
            db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                       (index, agent, "lead", "thread-" + agent, turn, index, json.dumps(record)))
        db.commit()
        db.close()
        FixedPricing.missing_refreshes = 0
        value = SessionCostReader(db_path, FixedPricing()).snapshot("worker-unknown", wait=True)
        self.assertEqual(FixedPricing.missing_refreshes, 1)
        self.assertEqual(value["pricedSamples"], 3)
        self.assertIn("openai", value["breakdown"]["providers"])
        self.assertEqual(value["unknownModels"], ["gpt-5.6-luna"])
        expected = 0.0
        for usage in (
            {"inputTokens": 1000, "cachedInputTokens": 100, "cacheWriteInputTokens": 0, "outputTokens": 100},
            {"inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0},
        ):
            expected += price_usage(catalog(), "openai", "gpt-6-luna", usage)[0]
        self.assertEqual(value["totalUSD"], expected)
        canvas = Canvas(self.root)
        canvas.runtime = types.SimpleNamespace(lock=threading.RLock())
        with patch("codex_pricing.PricingCatalog", return_value=FixedPricing()):
            server = make_server(canvas)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                endpoint = f"http://127.0.0.1:{server.server_port}/api/session-cost?agent=worker-claude"
                deadline = time.monotonic() + 3
                while True:
                    response = json.load(urlopen(endpoint, timeout=1))
                    if response["pricingState"] == "ready" or time.monotonic() >= deadline:
                        break
                    time.sleep(0.01)
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
        worker = {"id": "worker", "rootId": "lead", "accountKey": "claude-local", "provider": "claude", "threadId": "thread-current",
                  "accountHistory": [{"provider": "claude", "accountKey": "claude-local", "threadId": "thread-history"},
                                     {"provider": "codex", "accountKey": "codex-old", "threadId": "thread-before"}]}
        for key, record in (("lead", lead), ("worker", worker)):
            db.execute("INSERT INTO analytics_agents VALUES (?,?)", (key, json.dumps(record)))
        analytics = [
            {"responseId": "codex-response", "model": "gpt-6-luna", "delta": {"inputTokens": 1000,
                "cachedInputTokens": 100, "cacheWriteInputTokens": 0, "outputTokens": 100}},
            {"responseId": "message-1", "accountKey": "claude-local", "model": "claude-opus-5-5",
             "delta": {"inputTokens": 100, "cachedInputTokens": 20, "cacheWriteInputTokens": 30, "outputTokens": 10}},
            {"responseId": "claude-history-extra", "accountKey": "claude-local", "provider": "claude",
             "model": "claude-opus-5-5", "inputTokensAreUncached": True,
             "delta": {"inputTokens": 100, "cachedInputTokens": 20, "cacheWriteInputTokens": 30, "outputTokens": 10}},
            {"responseId": "codex-before", "accountKey": "codex-old", "model": "gpt-6-luna",
             "delta": {"inputTokens": 1000, "cachedInputTokens": 100, "cacheWriteInputTokens": 0, "outputTokens": 100}},
        ]
        for index, (agent, thread, record) in enumerate((
                ("lead", "thread", analytics[0]),
                ("worker", "thread-current", analytics[1]),
                ("worker", "thread-history", analytics[2]),
                ("worker", "thread-before", analytics[3])), 1):
            db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                       (index, agent, "lead", thread, "turn-" + str(index), index, json.dumps(record)))
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
                                  state_root=self.root).snapshot("worker", wait=True)
        self.assertIn("anthropic", value["breakdown"]["providers"])
        self.assertIn("openai", value["breakdown"]["providers"])
        self.assertEqual(value["pricedSamples"], 7)
        self.assertEqual(value["unknownModels"], [])
        self.assertAlmostEqual(value["breakdown"]["providers"]["anthropic"], 5 * .00028)
        self.assertAlmostEqual(value["breakdown"]["providers"]["openai"],
                               2 * price_usage(catalog(), "openai", "gpt-6-luna", analytics[0]["delta"])[0])
        self.assertTrue(value["claudeHistoryIncomplete"])

    def test_session_cost_reads_runtime_agent_before_analytics_registration(self):
        canvas = Canvas(self.root)
        canvas.runtime = types.SimpleNamespace(lock=threading.RLock())
        db = sqlite3.connect(canvas.db)
        db.execute("CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        db.execute("CREATE TABLE IF NOT EXISTS runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        db.execute("INSERT INTO runtime_agents VALUES ('new-chat',?)",
                   (json.dumps({"id": "new-chat", "rootId": "new-chat"}),))
        db.commit()
        db.close()
        with patch("codex_pricing.PricingCatalog", return_value=FixedPricing()):
            server = make_server(canvas)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                endpoint = f"http://127.0.0.1:{server.server_port}/api/session-cost?agent=new-chat"
                deadline = time.monotonic() + 3
                while True:
                    response = json.load(urlopen(endpoint, timeout=1))
                    if response["pricingState"] == "ready" or time.monotonic() >= deadline:
                        break
                    time.sleep(0.01)
                self.assertEqual(response["rootId"], "new-chat")
                self.assertEqual(response["pricingState"], "ready")
                self.assertEqual(response["pricedSamples"], 0)
                self.assertIsNone(response["totalUSD"])
            finally:
                server.shutdown()
                thread.join()
                server.server_close()

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
        result = SessionCostReader(db_path, LoadingPricing()).snapshot("lead", wait=True)
        self.assertEqual(result["pricingState"], "loading")
        self.assertIsNone(result["totalUSD"])
        self.assertEqual(result["unknownModels"], [])

    def test_session_cost_reads_a_chat_that_analytics_has_not_recorded(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        db.execute("INSERT INTO runtime_agents VALUES ('fresh',?)", (json.dumps({"rootId": "fresh"}),))
        db.commit()
        db.close()
        reader = SessionCostReader(db_path, FixedPricing())
        result = reader._compute("fresh", "fresh")
        self.assertEqual((result["rootId"], result["pricedSamples"]), ("fresh", 0))

    def test_missing_model_inference_ignores_deduplicated_rows(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
        db.execute("INSERT INTO analytics_agents VALUES ('worker',?)", (json.dumps({"rootId": "lead"}),))
        records = (
            (1, "turn-1", {"agentId": "lead", "threadId": "native", "turnId": "turn-1",
                            "responseId": "r1", "model": "gpt-6-luna", "delta": {"inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),
            (2, "turn-1", {"agentId": "lead", "threadId": "native", "turnId": "turn-1",
                            "model": "gpt-5.6-luna", "delta": {"inputTokens": 9000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),
            (3, "turn-2", {"agentId": "lead", "threadId": "native", "turnId": "turn-2",
                            "responseId": "r2", "delta": {"inputTokens": 2000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),
            (4, "turn-3", {"agentId": "worker", "threadId": "native", "turnId": "turn-3",
                            "responseId": "r3", "model": "gpt-5.6-luna", "delta": {"inputTokens": 3000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),
        )
        for seq, turn, record in records:
            db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                       (seq, record["agentId"], "lead", "native", turn, seq, json.dumps(record)))
        db.commit()
        db.close()
        result = SessionCostReader(db_path, FixedPricing()).snapshot("lead", wait=True)
        self.assertEqual(result["pricedSamples"], 2)
        self.assertEqual(result["unknownModels"], ["gpt-5.6-luna"])
        self.assertEqual(result["totalUSD"], price_usage(catalog(), "openai", "gpt-6-luna", {
            "inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0})[0] +
            price_usage(catalog(), "openai", "gpt-6-luna", {
            "inputTokens": 2000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0})[0])

    def test_session_cost_cache_skips_unchanged_and_invalidates_root_changes(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
          CREATE TABLE analytics_usage_roots (root TEXT PRIMARY KEY, generation INTEGER NOT NULL);
        """)
        db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
        db.commit()
        db.close()
        clock = [100.0]
        reader = SessionCostReader(db_path, FixedPricing(), clock=lambda: clock[0])
        first = reader.snapshot("lead", wait=True)
        db = sqlite3.connect(db_path)
        db.execute("INSERT INTO analytics_usage VALUES (1,'lead','lead','thread','turn',1,?)",
                   (json.dumps({"responseId": "new", "model": "gpt-6-luna", "delta": {
                       "inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),))
        db.execute("INSERT INTO analytics_usage_roots VALUES ('lead',1)")
        db.commit()
        db.close()
        clock[0] += 29
        with patch("codex_session_costs.threading.Thread") as worker:
            with patch.object(reader, "_compute", wraps=reader._compute) as compute:
                stale = reader.snapshot("lead", wait=True)
                again = reader.snapshot("lead", wait=True)
                self.assertEqual(stale["cacheAgeSeconds"], 29)
                self.assertTrue(stale["refreshing"])
                self.assertEqual(again["cacheAgeSeconds"], 29)
                self.assertTrue(again["refreshing"])
                worker.assert_called_once()
                compute.assert_not_called()
        reader._background_refresh("lead", "lead")
        updated = reader.snapshot("lead", wait=True)
        self.assertEqual(updated["pricedSamples"], 1)
        self.assertEqual(updated["cacheAgeSeconds"], 0)
        clock[0] += 60
        with patch("codex_session_costs.threading.Thread") as worker:
            with patch.object(reader, "_compute", wraps=reader._compute) as compute:
                unchanged = reader.snapshot("lead", wait=True)
                self.assertEqual(unchanged["cacheAgeSeconds"], 60)
                self.assertFalse(unchanged["refreshing"])
                worker.assert_not_called()
                compute.assert_not_called()
        db = sqlite3.connect(db_path)
        db.execute("INSERT INTO analytics_usage VALUES (2,'other','other','thread','turn',2,?)",
                   (json.dumps({"responseId": "other", "model": "gpt-6-luna", "delta": {
                       "inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),))
        db.execute("INSERT INTO analytics_usage_roots VALUES ('other',1)")
        db.commit()
        db.close()
        with patch("codex_session_costs.threading.Thread") as worker:
            with patch.object(reader, "_compute", wraps=reader._compute) as compute:
                unchanged = reader.snapshot("lead", wait=True)
                self.assertFalse(unchanged["refreshing"])
                worker.assert_not_called()
                compute.assert_not_called()
        db = sqlite3.connect(db_path)
        corrected = {"responseId": "new", "model": "gpt-6-luna", "delta": {
            "inputTokens": 2000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}
        db.execute("UPDATE analytics_usage SET record=? WHERE seq=1", (json.dumps(corrected),))
        db.execute("UPDATE analytics_usage_roots SET generation=2 WHERE root='lead'")
        db.commit()
        db.close()
        with patch("codex_session_costs.threading.Thread") as worker:
            changed = reader.snapshot("lead", wait=True)
            self.assertTrue(changed["refreshing"])
            worker.assert_called_once()
        reader._background_refresh("lead", "lead")
        changed = reader.snapshot("lead", wait=True)
        self.assertEqual(changed["totalUSD"], updated["totalUSD"] * 2)

        restarted = SessionCostReader(db_path, FixedPricing(), state_root=self.root, clock=lambda: clock[0])
        with patch("codex_session_costs.threading.Thread") as worker:
            with patch.object(restarted, "_compute", wraps=restarted._compute) as compute:
                restored = restarted.snapshot("lead", wait=True)
                self.assertEqual(restored["totalUSD"], changed["totalUSD"])
                self.assertEqual(restored["cacheAgeSeconds"], 0)
                self.assertFalse(restored["refreshing"])
                worker.assert_not_called()
                compute.assert_not_called()
        db = sqlite3.connect(db_path)
        for index in range(18):
            key = "root-" + str(index)
            db.execute("INSERT INTO analytics_agents VALUES (?,?)", (key, json.dumps({"rootId": key})))
        db.commit()
        db.close()
        for index in range(18):
            reader.snapshot("root-" + str(index), wait=True)
        self.assertLessEqual(len(reader.cache), 16)

    def test_session_cost_pricing_change_invalidates_unchanged_usage(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
          CREATE TABLE analytics_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        """)
        db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
        db.execute("INSERT INTO analytics_usage VALUES (1,'lead','lead','thread','turn',1,?)",
                   (json.dumps({"responseId": "r", "model": "gpt-6-luna", "delta": {
                       "inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}}),))
        db.commit()
        db.close()
        class MutablePricing:
            value = catalog()
            def snapshot(self):
                return self.value
            def refresh_missing(self):
                pass
        pricing = MutablePricing()
        reader = SessionCostReader(db_path, pricing, state_root=self.root)
        initial = reader.snapshot("lead", wait=True)
        pricing.value["providers"]["openai"]["models"]["gpt-6-luna"]["cost"]["input"] = 0.2
        with patch("codex_session_costs.threading.Thread") as worker:
            stale = reader.snapshot("lead", wait=True)
            self.assertTrue(stale["refreshing"])
            worker.assert_called_once()
        reader._background_refresh("lead", "lead")
        refreshed = reader.snapshot("lead", wait=True)
        self.assertEqual(refreshed["totalUSD"], initial["totalUSD"] * 2)

    def test_session_cost_concurrent_cold_requests_share_one_compute(self):
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
        reader = SessionCostReader(db_path, FixedPricing())
        entered, release = threading.Event(), threading.Event()
        calls = []
        original = reader._compute
        def slow_compute(agent_id, root):
            calls.append((agent_id, root))
            entered.set()
            self.assertTrue(release.wait(3))
            return original(agent_id, root)
        results = []
        with patch.object(reader, "_compute", side_effect=slow_compute):
            workers = [threading.Thread(target=lambda: results.append(reader.snapshot("lead", wait=True)))
                       for _ in range(8)]
            for worker in workers:
                worker.start()
            self.assertTrue(entered.wait(3))
            deadline = time.monotonic() + 3
            while len(reader.inflight) != 1 and time.monotonic() < deadline:
                time.sleep(0.005)
            release.set()
            for worker in workers:
                worker.join(3)
        self.assertEqual(calls, [("lead", "lead")])
        self.assertEqual(len(results), 8)
        self.assertTrue(all(value["rootId"] == "lead" for value in results))

    def test_cold_http_cost_returns_pending_without_waiting_for_history(self):
        db_path = self.root / "canvas.sqlite3"
        db = sqlite3.connect(db_path)
        db.executescript("""
          CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
          CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT, thread TEXT, turn TEXT, at REAL, record TEXT);
        """)
        for root in ("lead", "other"):
            db.execute("INSERT INTO analytics_agents VALUES (?,?)", (root, json.dumps({"rootId": root})))
        usage = {"inputTokens": 1000, "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 0}
        record = {"agentId": "lead", "threadId": "thread", "turnId": "turn",
                  "responseId": "response", "model": "gpt-6-luna", "delta": usage}
        db.execute("INSERT INTO analytics_usage VALUES (1,'lead','lead','thread','turn',1,?)", (json.dumps(record),))
        db.commit()
        db.close()
        entered, release = threading.Event(), threading.Event()
        calls = []
        original = SessionCostReader._compute
        def slow_compute(reader, agent_id, root):
            calls.append(root)
            entered.set()
            if not release.wait(4):
                raise RuntimeError("The fixture did not release the history read")
            return original(reader, agent_id, root)
        canvas = Canvas(self.root)
        canvas.runtime = types.SimpleNamespace(lock=threading.RLock())
        with patch("codex_pricing.PricingCatalog", return_value=FixedPricing()), patch.object(
                SessionCostReader, "_compute", slow_compute):
            server = make_server(canvas)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            def read(root):
                endpoint = f"http://127.0.0.1:{server.server_port}/api/session-cost?agent={root}"
                return json.load(urlopen(endpoint, timeout=1))
            try:
                # These HTTP requests must finish while the history read stays blocked.
                for _ in range(8):
                    value = read("lead")
                    self.assertEqual(value["pricingState"], "loading")
                    self.assertIsNone(value["totalUSD"])
                self.assertTrue(entered.wait(1))
                self.assertEqual(read("other")["pricingState"], "loading")
                self.assertEqual(calls, ["lead"], "Only one heavy history job runs across chats")
                release.set()
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    value = read("lead")
                    if value["pricingState"] == "ready":
                        break
                    time.sleep(0.01)
                self.assertEqual(value["pricingState"], "ready")
                self.assertEqual(value["pricedSamples"], 1)
                self.assertEqual(value["totalUSD"], price_usage(catalog(), "openai", "gpt-6-luna", usage)[0])
                self.assertEqual(calls, ["lead"])
            finally:
                release.set()
                server.shutdown()
                thread.join()
                server.server_close()

    def test_failed_cold_cost_remains_an_error_and_can_recover(self):
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
        reader = SessionCostReader(db_path, FixedPricing())
        with patch.object(reader, "_compute", side_effect=ValueError("Invalid cost data")):
            self.assertEqual(reader.snapshot("lead")["pricingState"], "loading")
            deadline = time.monotonic() + 2
            while reader.refreshing and time.monotonic() < deadline:
                time.sleep(0.005)
            with self.assertRaisesRegex(ValueError, "Invalid cost data"):
                reader.snapshot("lead")
            while reader.refreshing and time.monotonic() < deadline:
                time.sleep(0.005)
        with self.assertRaisesRegex(ValueError, "Invalid cost data"):
            reader.snapshot("lead")
        deadline = time.monotonic() + 2
        while reader.refreshing and time.monotonic() < deadline:
            time.sleep(0.005)
        self.assertEqual(reader.snapshot("lead")["pricingState"], "ready")

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

    def test_unknown_model_refreshes_early_at_most_hourly(self):
        calls, now = [], [1_000_000.0]
        done = threading.Event()
        def fetch():
            calls.append(now[0])
            done.set()
            return catalog()
        pricing = PricingCatalog(self.root, fetch=fetch, clock=lambda: now[0])
        pricing.last_attempt = now[0] - 30 * 60
        pricing.refresh_missing()
        self.assertEqual(calls, [])
        now[0] += 31 * 60
        pricing.refresh_missing()
        self.assertTrue(done.wait(3))
        for _ in range(100):
            if not pricing.refreshing:
                break
            time.sleep(0.02)
        pricing.refresh_missing()
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



class ClaudeAliasPricing(unittest.TestCase):
    def test_claude_code_aliases_resolve_to_newest_family_model(self):
        from codex_pricing import lookup, price_usage
        from codex_session_costs import provider_for
        catalog = {"providers": {"anthropic": {"models": {
            "claude-sonnet-4-5": {"release_date": "2025-09-29", "cost": {"input": 3, "output": 15}},
            "claude-sonnet-5-5": {"release_date": "2026-06-01", "cost": {"input": 2, "output": 10}},
            "claude-opus-5-5": {"release_date": "2026-06-01", "cost": {"input": 4, "output": 20}},
        }}}}
        self.assertEqual(lookup(catalog, "anthropic", "sonnet")["input"], 2)
        self.assertEqual(lookup(catalog, "anthropic", "opus[1m]")["output"], 20)
        self.assertIsNone(lookup(catalog, "anthropic", "haiku"))
        self.assertEqual(provider_for("opus[1m]"), "anthropic")
        self.assertIsNone(provider_for("<synthetic>"))
        catalog["providers"]["anthropic"]["models"]["claude-opus-5-5"]["cost"].update(cache_read=0.2, cache_write=5)
        cost, status, _ = price_usage(catalog, "anthropic", "opus[1m]", {
            "inputTokens": 1000, "cachedInputTokens": 600, "cacheWriteInputTokens": None, "outputTokens": 10})
        self.assertEqual(status, "priced")
        self.assertAlmostEqual(cost, (400 * 4 + 600 * .2 + 10 * 20) / 1_000_000)

if __name__ == "__main__":
    unittest.main()
