#!/usr/bin/env python3
"""Cost prices use TEMP rows without retaining a history WAL snapshot."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import contextmanager
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import codex_session_costs
from codex_pricing import price_usage
from codex_source import source_function


class FixedPricing:
    def snapshot(self):
        return {"providers": {"openai": {"models": {
            "gpt-6-luna": {"id": "gpt-6-luna", "cost": {
                "input": .1, "output": .5, "cache_read": .01,
                "cache_write": .125,
            }},
        }}, "anthropic": {"models": {
            "claude-opus-5-5": {"id": "claude-opus-5-5", "cost": {
                "input": 2, "output": 4, "cache_read": .5, "cache_write": 1,
            }},
        }}}}

    def refresh_missing(self):
        raise AssertionError("The fixture model has a price")


def usage(seq, input_tokens, output_tokens=0, *, model="gpt-6-luna", response=True, turn=None):
    turn = turn or "turn-" + str(seq)
    record = {"agentId": "lead", "threadId": "thread", "turnId": turn,
              "model": model, "accountKey": "default",
              "delta": {"inputTokens": input_tokens, "outputTokens": output_tokens,
                        "cachedInputTokens": 0, "cacheWriteInputTokens": 0}}
    if response:
        record["responseId"] = "response-" + str(seq)
    return (seq, "usage-" + str(seq), "lead", "lead", "thread", turn, seq, json.dumps(record))


class SessionCostSnapshotContract(unittest.TestCase):
    def setUp(self):
        # A paired run uses both frozen functions in the current module namespace.
        baseline = os.environ.get("STUDIO_COST_SNAPSHOT_BASELINE")
        if baseline:
            source = Path(baseline).read_bytes()
            for name in ("_compute", "_cost_usage_groups"):
                function, _ = source_function(source, ("SessionCostReader", name),
                                              vars(codex_session_costs), baseline)
                replacement = patch.object(codex_session_costs.SessionCostReader, name, function)
                replacement.start()
                self.addCleanup(replacement.stop)

    @contextmanager
    def fixture(self, layout="single", *, with_usage=True, claude_only=False):
        with tempfile.TemporaryDirectory(prefix="studio-cost-snapshot-") as folder:
            directory = Path(folder)
            paths = {"canvas": directory / "canvas.sqlite3"}
            if layout != "single":
                paths["analytics"] = directory / "analytics.sqlite3"
            writers = {}
            reader_connections = []
            try:
                for name, path in paths.items():
                    db = sqlite3.connect(path, timeout=0)
                    writers[name] = db
                    self.assertEqual(db.execute("PRAGMA journal_mode=WAL").fetchone()[0], "wal")
                    db.execute("PRAGMA wal_autocheckpoint=0")
                    db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
                    if name == "analytics" or layout in {"single", "overlay"}:
                        db.executescript("""
                          CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);
                          CREATE TABLE analytics_meta(key TEXT PRIMARY KEY,value TEXT);
                          CREATE TABLE analytics_usage_roots(root TEXT PRIMARY KEY,generation INTEGER);
                        """)
                        db.execute("INSERT INTO analytics_meta VALUES ('usageGeneration','1')")
                        db.execute("INSERT INTO analytics_usage_roots VALUES ('lead',1)")
                        if with_usage:
                            db.executescript("""
                              CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,id TEXT UNIQUE,
                                agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
                              CREATE INDEX usage_root ON analytics_usage(root,seq);
                            """)
                    if name == "canvas":
                        agent = json.dumps({"rootId": "lead",
                                            "provider": "claude" if claude_only else "codex",
                                            "accountKey": "claude-account" if claude_only else "default",
                                            "threadId": "thread"})
                        db.execute("INSERT INTO runtime_agents VALUES ('lead',?)", (agent,))
                        if layout in {"single", "overlay"}:
                            db.execute("INSERT INTO analytics_agents VALUES ('lead',?)", (agent,))
                    db.commit()
                history_name = "canvas" if layout == "single" else "analytics"
                history = writers[history_name]
                if with_usage:
                    first = usage(1, 1000)
                    redundant = usage(2, 9000, response=False, turn="turn-1")
                    missing_model = usage(3, 2000, 100, model=None)
                    if layout == "overlay":
                        writers["canvas"].executemany("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?,?)",
                                                      (first, redundant))
                        writers["canvas"].commit()
                        history.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?,?)", missing_model)
                    else:
                        history.executemany("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?,?)",
                                            (first, redundant, missing_model))
                    history.commit()
                for db in writers.values():
                    self.assertEqual(db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))
                accounts = {}
                if claude_only:
                    profile = directory / "claude"
                    project = profile / "projects" / "sample"
                    project.mkdir(parents=True)
                    (project / "thread.jsonl").write_text(json.dumps({
                        "type": "assistant", "timestamp": "2026-10-05T12:00:00Z",
                        "message": {"id": "native-response", "model": "claude-opus-5-5",
                                    "usage": {"input_tokens": 100, "cache_read_input_tokens": 20,
                                              "cache_creation_input_tokens": 30, "output_tokens": 10}}}) + "\n")
                    accounts["claude-account"] = {"provider": "claude", "home": str(profile)}
                reader = codex_session_costs.SessionCostReader(paths["canvas"], FixedPricing(),
                                                               accounts=accounts, state_root=directory)
                statements = []
                connect = reader._connect

                def traced():
                    db = connect()
                    db.set_trace_callback(statements.append)
                    reader_connections.append(db)
                    return db

                reader._connect = traced
                yield reader, writers, history_name, reader_connections, statements
            finally:
                for db in reader_connections:
                    try:
                        db.in_transaction
                    except sqlite3.ProgrammingError:
                        continue
                    db.close()
                for db in writers.values():
                    db.close()

    def append(self, writers, history_name, layout):
        history = writers[history_name]
        history.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?,?)", usage(4, 4000, 200))
        history.execute("UPDATE analytics_usage_roots SET generation=2 WHERE root='lead'")
        history.execute("UPDATE analytics_meta SET value='2' WHERE key='usageGeneration'")
        history.commit()
        if layout == "overlay":
            legacy = writers["canvas"]
            legacy.execute("INSERT INTO analytics_usage VALUES (?,?,?,?,?,?,?,?)", usage(5, 8000))
            legacy.execute("UPDATE analytics_usage_roots SET generation=2 WHERE root='lead'")
            legacy.execute("UPDATE analytics_meta SET value='2' WHERE key='usageGeneration'")
            legacy.commit()
        elif layout == "attached":
            canvas = writers["canvas"]
            canvas.execute("UPDATE runtime_agents SET record=json_set(record,'$.uiMarker',1) WHERE id='lead'")
            canvas.commit()

    def assert_closed(self, connections):
        self.assertTrue(connections)
        for db in connections:
            with self.assertRaises(sqlite3.ProgrammingError):
                db.execute("SELECT 1")

    def check_price_snapshot(self, layout):
        with self.fixture(layout) as (reader, writers, history_name, connections, statements):
            prices = []

            def priced(*args, **kwargs):
                if not prices:
                    self.append(writers, history_name, layout)
                    for name, writer in writers.items():
                        checkpoint = writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()
                        self.assertEqual(checkpoint, (0, 0, 0),
                                         name + " history snapshot must be released before prices")
                    self.assertFalse(connections[-1].in_transaction)
                    projection = next(index for index, sql in enumerate(statements)
                                      if "CREATE TEMP TABLE session_cost_usage" in sql)
                    final_select = next(index for index, sql in enumerate(statements)
                                        if "WITH priced AS MATERIALIZED" in sql)
                    commit = max(index for index, sql in enumerate(statements[:final_select])
                                 if sql.strip() == "COMMIT")
                    self.assertLess(projection, commit)
                    self.assertLess(commit, final_select)
                result = price_usage(*args, **kwargs)
                prices.append(result)
                return result

            with patch.object(codex_session_costs, "price_usage", side_effect=priced):
                old = reader._compute("lead", "lead")
            self.assertEqual(len(prices), 2)
            self.assertEqual(old["pricedSamples"], 2)
            self.assertAlmostEqual(old["totalUSD"], .00035)
            self.assertEqual(old["unknownModels"], [])
            self.assertEqual(old["generation"], 1)
            self.assertEqual(old["_cacheSource"]["usage"], {"maxSeq": 3, "generation": 1})
            self.assert_closed(connections)
            source = old.pop("_cacheSource")
            reader._remember("lead", old, source)
            new = reader._compute("lead", "lead")
            self.assertEqual(new["generation"], 2)
            self.assertEqual(new["_cacheSource"]["usage"],
                             {"maxSeq": 5 if layout == "overlay" else 4, "generation": 2})
            self.assertEqual(new["pricedSamples"], 4 if layout == "overlay" else 3)
            self.assertAlmostEqual(new["totalUSD"], .00165 if layout == "overlay" else .00085)
            self.assert_closed(connections)

    def test_single_database_releases_snapshot_before_prices(self):
        self.check_price_snapshot("single")

    def test_separate_analytics_releases_attached_canvas_snapshot(self):
        self.check_price_snapshot("attached")

    def test_legacy_overlay_materializes_both_snapshots_before_prices(self):
        self.check_price_snapshot("overlay")

    def test_price_exception_closes_connection_and_releases_snapshot(self):
        with self.fixture("overlay") as (reader, writers, history_name, connections, _):
            def fail(*args, **kwargs):
                self.append(writers, history_name, "overlay")
                raise ValueError("fixture price failure")

            with patch.object(codex_session_costs, "price_usage", side_effect=fail):
                with self.assertRaisesRegex(ValueError, "^fixture price failure$"):
                    reader._compute("lead", "lead")
            self.assert_closed(connections)
            self.assertFalse(reader.cache)
            for writer in writers.values():
                self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))
            result = reader._compute("lead", "lead")
            self.assertEqual(result["pricedSamples"], 4)
            self.assertEqual(result["_cacheSource"]["usage"], {"maxSeq": 5, "generation": 2})
            self.assertAlmostEqual(result["totalUSD"], .00165)

    def test_missing_usage_table_returns_empty_estimate_and_closes_snapshot(self):
        with self.fixture(with_usage=False) as (reader, writers, _, connections, _):
            with patch.object(codex_session_costs, "price_usage") as priced:
                result = reader._compute("lead", "lead")
            priced.assert_not_called()
            self.assertEqual(result["pricingState"], "ready")
            self.assertEqual(result["pricedSamples"], 0)
            self.assertIsNone(result["totalUSD"])
            self.assertEqual(result["_cacheSource"]["usage"], {"maxSeq": 0, "generation": 1})
            self.assert_closed(connections)
            writer = writers["canvas"]
            writer.execute("UPDATE analytics_meta SET value='2' WHERE key='usageGeneration'")
            writer.commit()
            self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))

    def test_claude_only_missing_usage_table_releases_snapshot_before_prices(self):
        with self.fixture(with_usage=False, claude_only=True) as (reader, writers, _, connections, _):
            writer = writers["canvas"]
            prices = []

            def priced(*args, **kwargs):
                writer.execute("UPDATE analytics_usage_roots SET generation=2 WHERE root='lead'")
                writer.execute("UPDATE analytics_meta SET value='2' WHERE key='usageGeneration'")
                writer.commit()
                self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0),
                                 "Missing analytics_usage must release the snapshot before Claude prices")
                self.assertFalse(connections[-1].in_transaction)
                result = price_usage(*args, **kwargs)
                prices.append(result)
                return result

            with patch.object(codex_session_costs, "price_usage", side_effect=priced):
                old = reader._compute("lead", "lead")
            self.assertEqual(len(prices), 1)
            self.assertEqual(old["pricedSamples"], 1)
            self.assertAlmostEqual(old["totalUSD"], .00028)
            self.assertEqual(old["generation"], 1)
            self.assertEqual(old["_cacheSource"]["usage"], {"maxSeq": 0, "generation": 1})
            self.assert_closed(connections)
            source = old.pop("_cacheSource")
            reader._remember("lead", old, source)
            new = reader._compute("lead", "lead")
            self.assertEqual(new["pricedSamples"], 1)
            self.assertAlmostEqual(new["totalUSD"], .00028)
            self.assertEqual(new["generation"], 2)
            self.assertEqual(new["_cacheSource"]["usage"], {"maxSeq": 0, "generation": 2})
            self.assert_closed(connections)

    def test_other_sql_error_propagates_and_closes_snapshot(self):
        with self.fixture() as (reader, writers, history_name, connections, _):
            def invalid_projection(db, root):
                return db.execute("SELECT fixture_invalid_column FROM analytics_usage WHERE root=?", (root,))

            with patch.object(reader, "_cost_usage_groups", side_effect=invalid_projection):
                with patch.object(codex_session_costs, "price_usage") as priced:
                    with self.assertRaisesRegex(sqlite3.OperationalError, "no such column: fixture_invalid_column"):
                        reader._compute("lead", "lead")
            priced.assert_not_called()
            self.assertFalse(reader.cache)
            self.assert_closed(connections)
            self.append(writers, history_name, "single")
            self.assertEqual(writers["canvas"].execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))


if __name__ == "__main__":
    unittest.main()
