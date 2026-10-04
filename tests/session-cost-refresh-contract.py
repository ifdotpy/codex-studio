#!/usr/bin/env python3
"""Cost display and append parser contracts use private databases and logs."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import os
from contextlib import closing
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

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
if os.environ.get("STUDIO_COST_SOURCE_DIR"):
    sys.path.insert(0, os.environ["STUDIO_COST_SOURCE_DIR"])
from codex_claude_costs import parse_claude_usage
from codex_session_costs import SessionCostReader
from codex_canvas import Canvas, make_server

spec = importlib.util.spec_from_file_location("cost_fixtures", ROOT / "tests" / "pricing-session-cost-contract.py")
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)


class SessionCostRefreshContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.db_path = self.root / "canvas.sqlite3"
        self.clock = [100.0]
        with closing(sqlite3.connect(self.db_path)) as db, db:
            db.executescript("""
              CREATE TABLE analytics_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
              CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
              CREATE TABLE analytics_usage (seq INTEGER PRIMARY KEY, agent TEXT, root TEXT,
                thread TEXT, turn TEXT, at REAL, record TEXT);
              CREATE INDEX analytics_usage_root ON analytics_usage(root,seq);
              CREATE TABLE analytics_usage_roots (root TEXT PRIMARY KEY, generation INTEGER NOT NULL);
              CREATE TABLE analytics_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            """)
            db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps({"rootId": "lead"}),))
        self.reader = SessionCostReader(self.db_path, fixtures.FixedPricing(), clock=lambda: self.clock[0])
        self.log = self.root / "log.jsonl"

    @staticmethod
    def usage(identity, tokens=100):
        return {"type": "assistant", "timestamp": "2026-10-02T12:00:00Z",
                "message": {"id": identity, "model": "claude-opus-5-5", "usage": {
                    "input_tokens": tokens, "cache_creation_input_tokens": 30,
                    "cache_read_input_tokens": 20, "output_tokens": 10}}}

    def write_rows(self, *rows, newline=True):
        self.log.write_text("\n".join(json.dumps(row) for row in rows) + ("\n" if newline else ""))

    def add_usage(self, tokens=1000):
        with closing(sqlite3.connect(self.db_path)) as db, db:
            record = {"model": "gpt-6-luna", "responseId": "response", "delta": {
                "inputTokens": tokens, "cachedInputTokens": 0, "cacheWriteInputTokens": 0,
                "outputTokens": 0}}
            db.execute("INSERT INTO analytics_usage VALUES (1,'lead','lead','thread','turn',1,?)",
                       (json.dumps(record),))
            db.execute("INSERT INTO analytics_usage_roots VALUES ('lead',1)")

    def test_background_refresh_publishes_after_refresh_state_is_released(self):
        observations = []

        def publish(state_dir, agent_id):
            acquired = self.reader.lock.acquire(blocking=False)
            observations.append((state_dir, agent_id, acquired, "lead" in self.reader.refreshing))
            if acquired:
                self.reader.lock.release()

        with patch("codex_session_costs._publish_session_cost", side_effect=publish):
            self.reader._background_refresh("lead", "lead")

        self.assertEqual(observations, [(self.db_path.parent, "lead", True, False)])

    def test_append_decodes_only_new_rows_and_keeps_exact_usage(self):
        self.write_rows(*({"type": "user", "text": "x" * 100} for _ in range(1000)))
        self.reader._log_rows(self.log)
        with self.log.open("a") as output:
            output.write(json.dumps(self.usage("new")) + "\n")
        with patch("codex_claude_costs.json.loads", wraps=json.loads) as decode:
            actual = self.reader._log_rows(self.log)
        self.assertEqual(decode.call_count, 1, "An append must not decode the existing transcript again")
        self.assertEqual(actual, parse_claude_usage(self.log))

    def test_valid_tail_without_newline_is_visible_and_not_duplicated(self):
        self.write_rows(self.usage("first"), newline=False)
        self.assertEqual([row["id"] for row in self.reader._log_rows(self.log)], ["first"])
        with self.log.open("a") as output:
            output.write("\n" + json.dumps(self.usage("second")))
        self.assertEqual([row["id"] for row in self.reader._log_rows(self.log)], ["first", "second"])
        with self.log.open("a") as output:
            output.write("\n")
        self.assertEqual(self.reader._log_rows(self.log), parse_claude_usage(self.log))

    def test_partial_tail_is_priced_once_after_completion(self):
        first, second = json.dumps(self.usage("first")), json.dumps(self.usage("second"))
        self.log.write_text(first + "\n" + second[:35])
        self.assertEqual([row["id"] for row in self.reader._log_rows(self.log)], ["first"])
        with self.log.open("a") as output:
            output.write(second[35:])
        self.assertEqual(self.reader._log_rows(self.log), parse_claude_usage(self.log))
        self.assertEqual([row["id"] for row in self.reader._log_rows(self.log)], ["first", "second"])

    def test_changed_old_prefix_and_append_force_exact_rebuild(self):
        self.write_rows(self.usage("first"))
        self.reader._log_rows(self.log)
        self.write_rows(self.usage("first", tokens=200), self.usage("second"))
        actual = self.reader._log_rows(self.log)
        self.assertEqual(actual, parse_claude_usage(self.log))
        self.assertEqual(actual[0]["usage"]["inputTokens"], 200)
        self.assertTrue(actual[0]["inputTokensAreUncached"])

    def test_truncate_and_replace_cannot_reuse_old_rows(self):
        self.write_rows(self.usage("first"), self.usage("second"))
        self.reader._log_rows(self.log)
        self.write_rows(self.usage("third"))
        self.assertEqual(self.reader._log_rows(self.log), parse_claude_usage(self.log))
        stat = self.log.stat()
        replacement = self.root / "replacement"
        replacement.write_text(json.dumps(self.usage("fourth")) + "\n")
        os.utime(replacement, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        replacement.replace(self.log)
        self.assertEqual([row["id"] for row in self.reader._log_rows(self.log)], ["fourth"])

    def test_in_place_edit_with_restored_mtime_changes_source_signature(self):
        config = self.root / "claude"
        projects = config / "projects" / "sample"
        projects.mkdir(parents=True)
        self.log = projects / "thread.jsonl"
        self.write_rows(self.usage("first"))
        claude = {"lead": ("default", config, {("default", "thread")})}
        with patch.object(self.reader, "_claude_profile", return_value=config):
            before = self.reader._claude_signature(claude)
            self.reader._log_rows(self.log)
            stat = self.log.stat()
            self.write_rows(self.usage("first", tokens=200))
            os.utime(self.log, ns=(stat.st_atime_ns, stat.st_mtime_ns))
            self.assertNotEqual(self.reader._claude_signature(claude), before)
        self.assertEqual(self.reader._log_rows(self.log), parse_claude_usage(self.log))

    def test_preserved_mtime_edit_updates_total_instead_of_reusing_source_cache(self):
        config = self.root / "claude"
        projects = config / "projects" / "sample"
        projects.mkdir(parents=True)
        self.log = projects / "thread.jsonl"
        self.write_rows(self.usage("first"))
        self.reader.accounts = {"default": {"provider": "claude", "home": str(config)}}
        record = {"rootId": "lead", "threadId": "thread", "accountKey": "default"}
        with closing(sqlite3.connect(self.db_path)) as db, db:
            db.execute("UPDATE analytics_agents SET record=? WHERE id='lead'", (json.dumps(record),))
        before = self.reader.snapshot("lead", wait=True)
        stat = self.log.stat()
        self.write_rows(self.usage("first", tokens=200))
        os.utime(self.log, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        self.reader._background_refresh("lead", "lead")
        after = self.reader.snapshot("lead")
        self.assertEqual(after["pricedSamples"], 1)
        self.assertAlmostEqual(after["totalUSD"], before["totalUSD"] + .0002)

    def test_existing_two_item_cache_entry_is_upgraded_on_read(self):
        self.write_rows(self.usage("first"))
        stat = self.log.stat()
        self.reader.file_cache[str(self.log)] = ((stat.st_size, stat.st_mtime_ns), [{"id": "stale"}])
        self.assertEqual(self.reader._log_rows(self.log), parse_claude_usage(self.log))
        self.assertEqual(len(self.reader.file_cache[str(self.log)]), 3)

    def test_agent_signature_tracks_cost_identity_only(self):
        record = {"rootId": "lead", "accountKey": "default", "threadId": "thread",
                  "accountHistory": [{"accountKey": "old", "provider": "claude", "threadId": "old-thread"}]}
        before = self.reader._agent_signature([("lead", record)])
        unrelated = {**record, "status": "busy", "lastAnswer": "x" * 1000, "toolPhase": "working", "name": "new"}
        self.assertEqual(before, self.reader._agent_signature([("lead", unrelated)]))
        for key in ("rootId", "accountKey", "threadId", "accountHistory"):
            changed = {**record, key: [] if key == "accountHistory" else "other"}
            self.assertNotEqual(before, self.reader._agent_signature([("lead", changed)]))

    def test_cold_and_cached_http_return_while_claude_discovery_is_blocked(self):
        entered, release = threading.Event(), threading.Event()
        original = SessionCostReader._claude_signature
        def blocked(reader, agents):
            entered.set()
            if not release.wait(3):
                raise RuntimeError("The fixture did not release file discovery")
            return original(reader, agents)
        canvas = Canvas(self.root)
        canvas.runtime = types.SimpleNamespace(lock=threading.RLock())
        readers = []
        def cost_reader(*args, **kwargs):
            reader = SessionCostReader(*args, **kwargs, clock=lambda: self.clock[0])
            readers.append(reader)
            return reader
        with patch("codex_pricing.PricingCatalog", return_value=fixtures.FixedPricing()), patch.object(
                SessionCostReader, "_claude_signature", blocked), patch(
                "codex_session_costs.SessionCostReader", side_effect=cost_reader):
            server = make_server(canvas)
            worker = threading.Thread(target=server.serve_forever, daemon=True)
            worker.start()
            endpoint = f"http://127.0.0.1:{server.server_port}/api/session-cost?agent=lead"
            try:
                cold = json.load(urlopen(endpoint, timeout=0.5))
                self.assertEqual(cold["pricingState"], "loading")
                self.assertTrue(entered.wait(1))
                for _ in range(3):
                    self.assertEqual(json.load(urlopen(endpoint, timeout=0.5))["pricingState"], "loading")
                release.set()
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    value = json.load(urlopen(endpoint, timeout=0.5))
                    if value["pricingState"] == "ready":
                        break
                    time.sleep(0.005)
                self.assertEqual(value["pricingState"], "ready")
                while readers[0].refreshing and time.monotonic() < deadline:
                    time.sleep(0.005)
                self.assertFalse(readers[0].refreshing)
                # Cached HTTP must not inspect files even when a file check is due.
                release.clear()
                entered.clear()
                self.clock[0] += 11
                value = json.load(urlopen(endpoint, timeout=0.5))
                self.assertEqual(value["pricingState"], "ready")
                self.assertTrue(entered.wait(1))
                self.assertEqual(len(readers), 1)
                for _ in range(3):
                    self.assertEqual(json.load(urlopen(endpoint, timeout=0.5))["pricingState"], "ready")
            finally:
                release.set()
                server.shutdown()
                worker.join(3)
                server.server_close()

    def test_display_poll_limits_background_checks_and_keeps_previous_totals(self):
        initial = self.reader.snapshot("lead", wait=True)
        # Existing readers have no new attributes until the new methods run.
        self.assertNotIn("refresh_checks", self.reader.__dict__)
        with patch("codex_session_costs.threading.Thread") as worker:
            self.reader.snapshot("lead")
            worker.assert_called_once()
        self.reader._background_refresh("lead", "lead")
        self.add_usage()
        with patch("codex_session_costs.threading.Thread") as worker:
            for _ in range(8):
                stale = self.reader.snapshot("lead")
                self.assertEqual(stale["totalUSD"], initial["totalUSD"])
                self.assertTrue(stale["refreshing"])
            worker.assert_not_called()
            self.clock[0] += 10
            self.reader.snapshot("lead")
            worker.assert_called_once()
        self.reader._background_refresh("lead", "lead")
        self.assertEqual(self.reader.snapshot("lead")["pricedSamples"], 1)

    def test_unchanged_background_check_skips_history_sql_and_persisted_restart_does_too(self):
        self.add_usage()
        expected = self.reader.snapshot("lead", wait=True)
        for reader in (self.reader, SessionCostReader(self.db_path, fixtures.FixedPricing())):
            statements = []
            original = reader._connect
            def traced():
                db = original()
                db.set_trace_callback(statements.append)
                return db
            with patch.object(reader, "_connect", traced):
                reader._background_refresh("lead", "lead")
            self.assertEqual(reader.cache["lead"][1]["totalUSD"], expected["totalUSD"])
            self.assertFalse(any("session_cost_excluded" in statement for statement in statements), statements)


if __name__ == "__main__":
    unittest.main()
