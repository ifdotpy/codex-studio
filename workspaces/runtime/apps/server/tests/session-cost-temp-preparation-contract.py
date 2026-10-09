#!/usr/bin/env python3
"""Cost TEMP preparation releases history snapshots and replaces retry keys."""

# Support direct execution without runner-provided PYTHONPATH.
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT

from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sys
import unittest

ROOT = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
spec = importlib.util.spec_from_file_location("cost_snapshot_fixture", Path(__file__).with_name("session-cost-snapshot-contract.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class ObservedConnection:
    def __init__(self, db, before_keys=None, after_keys=None, before_models=None):
        self.db = db
        self.before_keys = before_keys
        self.after_keys = after_keys
        self.before_models = before_models

    def __getattr__(self, name):
        return getattr(self.db, name)

    def execute(self, statement, parameters=()):
        if statement.startswith("SELECT MAX(model IS NULL)") and self.before_models:
            self.before_models(self.db)
        return self.db.execute(statement, parameters)

    def executemany(self, statement, parameters):
        keys = list(parameters)
        if statement.startswith("INSERT OR IGNORE INTO session_cost_claude_messages") and self.before_keys:
            self.before_keys(self.db, keys)
        result = self.db.executemany(statement, keys)
        if statement.startswith("INSERT OR IGNORE INTO session_cost_claude_messages") and self.after_keys:
            self.after_keys(self.db, keys)
        return result


class TempPreparationContract(unittest.TestCase):
    def setUp(self):
        self.fixtures = fixture.SessionCostSnapshotContract()

    def observe(self, reader, **callbacks):
        connect = reader._connect
        reader._connect = lambda: ObservedConnection(connect(), **callbacks)

    def test_receipt_insert_allows_history_checkpoint_before_final_source_validation(self):
        with self.fixtures.fixture("attached", claude_only=True) as (reader, writers, history_name, _, _):
            inserts = []
            def before_keys(db, keys):
                self.assertFalse(db.in_transaction)
                self.assertEqual(db.execute("PRAGMA temp.cache_size").fetchone()[0], -16384)
                inserts.append(keys)
            def after_keys(db, keys):
                if len(inserts) == 1:
                    self.fixtures.append(writers, history_name, "attached")
                    for writer in writers.values():
                        self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))
            self.observe(reader, before_keys=before_keys, after_keys=after_keys)
            result = reader._compute("lead", "lead")
            self.assertEqual(len(inserts), 2, "The source change must repeat preparation")
            self.assertEqual(result["_cacheSource"]["usage"], {"maxSeq": 4, "generation": 2})
            self.assertEqual(result["pricedSamples"], 4)

    def test_model_preparation_allows_checkpoint_and_uses_only_captured_usage(self):
        for layout in ("single", "attached", "overlay"):
            with self.subTest(layout=layout), self.fixtures.fixture(layout) as (reader, writers, history_name, _, _):
                checks = []
                def before_models(db):
                    self.assertFalse(db.in_transaction)
                    self.fixtures.append(writers, history_name, layout)
                    for writer in writers.values():
                        self.assertEqual(writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone(), (0, 0, 0))
                    checks.append(True)
                self.observe(reader, before_models=before_models)
                old = reader._compute("lead", "lead")
                self.assertEqual(checks, [True])
                self.assertEqual(old["_cacheSource"]["usage"], {"maxSeq": 3, "generation": 1})
                self.assertEqual(old["pricedSamples"], 2)
                self.assertAlmostEqual(old["totalUSD"], .00035)
                # The next computation reads the update after the captured snapshot.
                connect = reader._connect
                reader._connect = lambda: connect().db
                new = reader._compute("lead", "lead")
                self.assertEqual(new["generation"], 2)
                self.assertEqual(new["pricedSamples"], 4 if layout == "overlay" else 3)

    def test_retry_replaces_native_receipt_keys(self):
        with self.fixtures.fixture("single", claude_only=True) as (reader, writers, _, _, _):
            writer = writers["canvas"]
            record = {"agentId": "lead", "threadId": "thread", "turnId": "native-turn",
                      "accountKey": "claude-account", "model": "claude-opus-5-5",
                      "responseId": "native-response", "inputTokensAreUncached": True,
                      "delta": {"inputTokens": 9999, "cachedInputTokens": 0,
                                "cacheWriteInputTokens": 0, "outputTokens": 100}}
            writer.execute("DELETE FROM analytics_usage")
            writer.execute("INSERT INTO analytics_usage VALUES (1,'native-usage','lead','lead','thread','native-turn',1,?)", (json.dumps(record),))
            writer.commit()
            log = reader.state_root / "claude" / "projects" / "sample" / "thread.jsonl"
            inserts = []
            def after_keys(db, keys):
                inserts.append(keys)
                if len(inserts) == 1:
                    row = json.loads(log.read_text())
                    row["message"]["id"] = "changed-receipt"
                    row["message"]["usage"]["input_tokens"] = 200
                    log.write_text(json.dumps(row) + "\n")
                    record["responseId"] = "changed-receipt"
                    writer.execute("UPDATE analytics_usage SET record=? WHERE seq=1", (json.dumps(record),))
                    writer.execute("UPDATE analytics_usage_roots SET generation=2 WHERE root='lead'")
                    writer.execute("UPDATE analytics_meta SET value='2' WHERE key='usageGeneration'")
                    writer.commit()
            self.observe(reader, after_keys=after_keys)
            result = reader._compute("lead", "lead")
            self.assertEqual(inserts, [[("claude-account", "thread", "native-response")],
                                      [("claude-account", "thread", "changed-receipt")]])
            self.assertEqual(result["pricedSamples"], 1)
            self.assertAlmostEqual(result["totalUSD"], .00048)
            self.assertEqual(result["_cacheSource"]["usage"], {"maxSeq": 1, "generation": 2})

    def test_unchanged_cost_uses_cache_without_native_parse_or_temp_tables(self):
        for claude in (False, True):
            with self.subTest(claude=claude), self.fixtures.fixture("attached", claude_only=claude) as (reader, _, _, _, statements):
                first = reader._compute("lead", "lead")
                source = first.pop("_cacheSource")
                reader.cache["lead"] = (reader.clock(), first, source)
                statements.clear()
                def unexpected_parse(path):
                    self.fail("The unchanged native log must use the cost cache")
                reader._log_rows = unexpected_parse
                second = reader._compute("lead", "lead")
                self.assertEqual(second, {**first, "_cacheSource": source})
                self.assertFalse(any("session_cost_" in sql for sql in statements), statements)


if __name__ == "__main__":
    unittest.main()
