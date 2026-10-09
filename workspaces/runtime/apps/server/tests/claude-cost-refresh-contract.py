#!/usr/bin/env python3
"""Claude log checks reuse costs only when exact billable content is unchanged."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import closing
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from codex_session_costs import SessionCostReader


class FixedPricing:
    def __init__(self):
        self.input_rate = 2

    def snapshot(self):
        return {"providers": {
            "anthropic": {"models": {"claude-opus-5-5": {"id": "claude-opus-5-5",
                "cost": {"input": self.input_rate, "output": 4,
                         "cache_read": .5, "cache_write": 1}}}},
            "openai": {"models": {"gpt-6-luna": {"id": "gpt-6-luna",
                "cost": {"input": .1, "output": .5, "cache_read": .01}}}},
        }}

    def refresh_missing(self):
        raise AssertionError("The fixture models have prices")


class TrackedReader(SessionCostReader):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.history_scans = []

    def _cost_usage_groups(self, db, root):
        self.history_scans.append(root)
        return super()._cost_usage_groups(db, root)


class ClaudeCostRefreshContract(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="studio-claude-cost-refresh-")
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name)
        self.path = self.state / "canvas.sqlite3"
        self.profile = self.state / "claude"
        project = self.profile / "projects" / "fixture"
        project.mkdir(parents=True)
        self.log = project / "thread.jsonl"
        self.pricing = FixedPricing()
        self.accounts = {"claude-account": {"provider": "claude", "home": str(self.profile)}}
        self.agent = {"id": "lead", "rootId": "lead", "accountKey": "claude-account",
                      "provider": "claude", "threadId": "thread"}
        self.write_log([self.usage_row()])
        with closing(sqlite3.connect(self.path)) as db, db:
            db.executescript("""
              CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
              CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
              CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT,
                root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
              CREATE INDEX usage_root ON analytics_usage(root,seq);
              CREATE TABLE analytics_usage_roots(root TEXT PRIMARY KEY,generation INTEGER);
            """)
            db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (json.dumps(self.agent),))
            db.execute("INSERT INTO analytics_usage_roots VALUES ('lead',1)")
            self.insert_usage(db, 1, "native-a", "claude-opus-5-5", 1000, 10)
            self.insert_usage(db, 2, "codex-a", "gpt-6-luna", 1000, 0)
        self.reader = self.new_reader()

    def new_reader(self):
        return TrackedReader(self.path, self.pricing, self.accounts, state_root=self.state)

    @staticmethod
    def usage_row(identity="native-a", input_tokens=100, model="claude-opus-5-5"):
        return {"type": "assistant", "timestamp": "2026-10-04T12:00:00Z",
                "message": {"id": identity, "model": model, "usage": {
                    "input_tokens": input_tokens, "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 0, "output_tokens": 10}}}

    def write_log(self, rows, *, newline=True):
        self.log.write_text("\n".join(json.dumps(row) for row in rows) + ("\n" if newline else ""))

    def append_log(self, row):
        with self.log.open("a") as target:
            target.write(json.dumps(row) + "\n")

    @staticmethod
    def insert_usage(db, seq, response, model, input_tokens, output_tokens):
        record = {"accountKey": "claude-account", "model": model,
                  "responseId": response, "delta": {"inputTokens": input_tokens,
                      "cachedInputTokens": 0, "cacheWriteInputTokens": 0,
                      "outputTokens": output_tokens}}
        db.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?)",
                   (seq, "lead", "lead", "thread", str(seq), seq, json.dumps(record)))

    def refresh(self, reader=None):
        return (reader or self.reader)._compute_shared("lead", "lead", refresh=True)

    def test_nonusage_appends_keep_the_exact_cost_without_another_history_scan(self):
        first = self.refresh()
        first_source = self.reader.cache["lead"][2].copy()
        self.assertEqual(first["pricedSamples"], 2)
        self.assertAlmostEqual(first["totalUSD"], .00035)
        for row in ({"type": "progress", "data": {"status": "active"}},
                    {"type": "assistant", "message": {"id": "tool-a", "model": "claude-opus-5-5",
                                                       "content": [{"type": "tool_use", "id": "tool-a"}]}},
                    {"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tool-a"}]}}):
            self.append_log(row)
            result = self.refresh()
            self.assertEqual(result, first)
        source = self.reader.cache["lead"][2]
        self.assertNotEqual(source["claudeSignature"], first_source["claudeSignature"])
        self.assertEqual(self.reader.history_scans, ["lead"])

    def test_real_new_usage_reads_history_and_adds_the_exact_cost(self):
        first = self.refresh()
        self.append_log(self.usage_row("native-b", 200))
        result = self.refresh()
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        self.assertEqual(result["pricedSamples"], 3)
        self.assertAlmostEqual(result["totalUSD"], first["totalUSD"] + .00045)

    def test_same_count_usage_correction_reads_history_and_changes_the_cost(self):
        self.refresh()
        self.write_log([self.usage_row(input_tokens=200)])
        result = self.refresh()
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        self.assertEqual(result["pricedSamples"], 2)
        self.assertAlmostEqual(result["totalUSD"], .00055)

    def test_a_different_billable_row_with_the_same_id_keeps_the_existing_selection_rule(self):
        first = self.refresh()
        self.append_log(self.usage_row(input_tokens=200))
        result = self.refresh()
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        # The current receipt selection keeps the first row for this response.
        self.assertEqual(result, first)

    def test_same_count_response_identity_change_updates_the_exclusion(self):
        self.refresh()
        self.write_log([self.usage_row(identity="native-b")])
        result = self.refresh()
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        self.assertEqual(result["pricedSamples"], 3)
        self.assertAlmostEqual(result["totalUSD"], .00239)

    def test_same_count_model_correction_keeps_unknown_usage_visible(self):
        self.refresh()
        self.write_log([self.usage_row(model="claude-unknown-model")])
        # Unknown prices use the normal refresh_missing path.
        self.pricing.refresh_missing = lambda: None
        result = self.refresh()
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        self.assertEqual(result["pricedSamples"], 1)
        self.assertAlmostEqual(result["totalUSD"], .0001)
        self.assertEqual(result["unknownModels"], ["claude-unknown-model"])

    def test_rotation_with_the_same_billable_rows_reuses_the_cost(self):
        first = self.refresh()
        raw = self.log.read_bytes()
        self.log.rename(self.log.with_suffix(".previous"))
        self.log.write_bytes(raw)
        self.assertEqual(self.refresh(), first)
        self.assertEqual(self.reader.history_scans, ["lead"])

    def test_missing_and_truncated_logs_restore_the_saved_analytics_usage(self):
        for missing in (False, True):
            with self.subTest(missing=missing):
                self.write_log([self.usage_row()])
                reader = self.new_reader()
                reader._compute_shared("lead", "lead", refresh=True)
                if missing:
                    self.log.unlink()
                else:
                    self.log.write_text("")
                result = reader._compute_shared("lead", "lead", refresh=True)
                self.assertEqual(reader.history_scans, ["lead", "lead"])
                self.assertAlmostEqual(result["totalUSD"], .00214)
                self.assertEqual(result["pricedSamples"], 2)

    def test_a_final_row_without_newline_survives_a_nonusage_append(self):
        self.write_log([self.usage_row()], newline=False)
        first = self.refresh()
        with self.log.open("a") as target:
            target.write("\n" + json.dumps({"type": "progress"}) + "\n")
        self.assertEqual(self.refresh(), first)
        self.assertEqual(self.reader.history_scans, ["lead"])

    def test_account_history_changes_read_history_even_with_identical_usage(self):
        self.refresh()
        other = self.profile / "projects" / "fixture" / "old-thread.jsonl"
        other.write_text(json.dumps(self.usage_row()) + "\n")
        self.agent["accountHistory"] = [{"provider": "claude", "accountKey": "claude-account",
                                         "threadId": "old-thread"}]
        with closing(sqlite3.connect(self.path)) as db, db:
            db.execute("UPDATE analytics_agents SET record=? WHERE id='lead'", (json.dumps(self.agent),))
        result = self.refresh()
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        self.assertEqual(result["pricedSamples"], 3)
        self.assertAlmostEqual(result["totalUSD"], .0006)

    def test_pricing_and_same_sequence_analytics_corrections_read_history(self):
        self.refresh()
        self.pricing.input_rate = 3
        self.append_log({"type": "progress"})
        self.assertAlmostEqual(self.refresh()["totalUSD"], .00045)
        with closing(sqlite3.connect(self.path)) as db, db:
            record = json.loads(db.execute("SELECT record FROM analytics_usage WHERE seq=2").fetchone()[0])
            record["delta"]["inputTokens"] = 2000
            db.execute("UPDATE analytics_usage SET record=? WHERE seq=2", (json.dumps(record),))
            db.execute("UPDATE analytics_usage_roots SET generation=generation+1 WHERE root='lead'")
        self.assertAlmostEqual(self.refresh()["totalUSD"], .00055)
        self.assertEqual(self.reader.history_scans, ["lead", "lead", "lead"])

    def test_new_analytics_usage_reads_history_even_when_native_usage_is_unchanged(self):
        self.refresh()
        with closing(sqlite3.connect(self.path)) as db, db:
            self.insert_usage(db, 3, "codex-b", "gpt-6-luna", 1000, 0)
            db.execute("UPDATE analytics_usage_roots SET generation=generation+1 WHERE root='lead'")
        self.append_log({"type": "progress"})
        result = self.refresh()
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        self.assertEqual(result["pricedSamples"], 3)
        self.assertAlmostEqual(result["totalUSD"], .00045)

    def test_identical_native_usage_does_not_share_totals_between_roots(self):
        first = self.refresh()
        other_agent = {**self.agent, "id": "other", "rootId": "other"}
        with closing(sqlite3.connect(self.path)) as db, db:
            db.execute("INSERT INTO analytics_agents VALUES ('other',?)", (json.dumps(other_agent),))
            record = {"accountKey": "claude-account", "model": "gpt-6-luna",
                      "responseId": "other-response", "delta": {
                          "inputTokens": 100, "cachedInputTokens": 0, "outputTokens": 0}}
            db.execute("INSERT INTO analytics_usage VALUES (3,'other','other','thread','other-turn',3,?)",
                       (json.dumps(record),))
        other = self.reader._compute_shared("other", "other", refresh=True)
        self.assertAlmostEqual(other["totalUSD"], .00026)
        self.append_log({"type": "progress"})
        self.assertEqual(self.refresh(), first)
        self.assertEqual(self.reader._compute_shared("other", "other", refresh=True), other)
        self.assertEqual(self.reader.history_scans, ["lead", "other"])

    def test_persisted_cache_keeps_the_existing_raw_file_identity_requirement(self):
        first = self.refresh()
        same = self.new_reader()
        self.assertEqual(same._compute_shared("lead", "lead", refresh=True), first)
        self.assertEqual(same.history_scans, [])
        self.append_log({"type": "progress"})
        changed = self.new_reader()
        self.assertEqual(changed._compute_shared("lead", "lead", refresh=True), first)
        self.assertEqual(changed.history_scans, ["lead"])

    def test_legacy_cache_requires_one_full_read_before_reusing_parsed_usage(self):
        first = self.refresh()
        self.reader.cache["lead"][2].pop("claudeUsageSignature", None)
        self.append_log({"type": "progress"})
        self.assertEqual(self.refresh(), first)
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])
        self.append_log({"type": "progress"})
        self.assertEqual(self.refresh(), first)
        self.assertEqual(self.reader.history_scans, ["lead", "lead"])


if __name__ == "__main__":
    unittest.main()
