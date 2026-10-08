#!/usr/bin/env python3
"""Compare incremental cost groups with the legacy reader after mixed edits."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

from contextlib import closing
import json
from pathlib import Path
import random
import statistics
import sqlite3
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from codex_analytics import AnalyticsMixin
import codex_cost_usage as usage
import codex_session_costs as costs


class Capture(AnalyticsMixin):
    def records(self, _db, _table):
        return []


class Prices:
    def __init__(self):
        rates = {"input": 2, "output": 4, "cache_read": .5, "cache_write": 1,
                 "tiers": [{"tier": {"type": "context", "size": 200},
                            "input": 20, "output": 40, "cache_read": 5, "cache_write": 10}]}
        self.value = {"providers": {"openai": {"models": {
            "gpt-a": {"cost": rates}, "gpt-b": {"cost": {"input": 3, "output": 8,
                "cache_read": 1, "cache_write": 2}}}}, "anthropic": {"models": {
            "claude-opus-5-5": {"cost": rates}}}}}

    def snapshot(self):
        return self.value

    def refresh_missing(self):
        pass


class IncrementalCosts(unittest.TestCase):
    def setUp(self):
        self.folder = tempfile.TemporaryDirectory(prefix="studio-incremental-cost-")
        self.addCleanup(self.folder.cleanup)
        self.root = Path(self.folder.name)
        self.path = self.root / "canvas.sqlite3"
        self.db = sqlite3.connect(self.path)
        self.db.row_factory = sqlite3.Row
        self.addCleanup(self.db.close)
        self.db.execute("PRAGMA journal_mode=WAL")
        Capture().analytics_init(self.db)
        self.db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
        self.profile = self.root / "claude"
        self.log = self.profile / "projects" / "sample" / "thread.jsonl"
        self.log.parent.mkdir(parents=True)
        for identity in ("lead", "other"):
            member = {"id": identity, "rootId": identity, "provider": "codex", "accountKey": "new",
                      "threadId": "new-thread", "accountHistory": [{"provider": "claude",
                      "accountKey": "claude", "threadId": "thread"}]}
            self.db.execute("INSERT INTO analytics_agents VALUES (?,?)", (identity, json.dumps(member)))
            self.db.execute("INSERT INTO runtime_agents VALUES (?,?)", (identity, json.dumps(member)))
        self.db.commit()
        self.pricing = Prices()
        self.accounts = {"new": {"provider": "codex"}, "claude": {"provider": "claude", "home": str(self.profile)}}
        self.reader = costs.SessionCostReader(self.path, self.pricing, self.accounts)
        self.statements = []
        connect = self.reader._connect
        def observed():
            db = connect()
            db.set_trace_callback(self.statements.append)
            return db
        self.reader._connect = observed

    def record(self, identity, *, root="lead", model="gpt-a", response=None, at=None, **fields):
        turn = fields.pop("turnId", "turn-" + str(identity))
        record = {"agentId": "lead", "threadId": "thread", "turnId": turn, "model": model,
                  "accountKey": "claude", "responseId": response,
                  "delta": {"inputTokens": 150, "cachedInputTokens": 20,
                            "cacheWriteInputTokens": 30, "outputTokens": 10}, **fields}
        self.db.execute("INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES (?,?,?,?,?,?,?) "
                        "ON CONFLICT(id) DO UPDATE SET root=excluded.root,turn=excluded.turn,at=excluded.at,record=excluded.record",
                        (str(identity), "lead", root, "thread", turn, identity if at is None else at, json.dumps(record)))
        return record

    def check(self, root="lead"):
        self.db.commit()
        actual = self.reader._compute_shared(root, root, refresh=True)
        oracle = costs.SessionCostReader(self.path, self.pricing, self.accounts, state_root=self.root / "oracle")
        # The oracle uses the unchanged full-history SQL against the same inputs.
        with patch.object(costs, "cost_source_state", return_value=None):
            expected = oracle._compute(root, root)
        for key in ("rootId", "totalUSD", "pricedSamples", "breakdown", "unknownModels",
                    "method", "claudeHistoryIncomplete"):
            self.assertEqual(actual[key], expected[key], key)
        return actual

    def log_message(self, identity, tokens=100):
        self.log.write_text(json.dumps({"type": "assistant", "timestamp": "2026-10-08T12:00:00Z", "message": {"id": identity,
            "model": "claude-opus-5-5", "usage": {"input_tokens": tokens,
            "cache_read_input_tokens": 20, "cache_creation_input_tokens": 10,
            "output_tokens": 5}}}) + "\n")

    def test_mixed_corrections_suppression_model_sources_logs_and_prices(self):
        self.record(1, response="response-1")
        self.record(2, model=None, turnId="turn-1")
        self.record(3, model=None)
        self.record(4, model="gpt-b", at=50)
        self.record(5, model="unknown-model", inputTokensAreUncached=True)
        self.record(6, model="<synthetic>")
        self.record(7, model=None, turnId=None, agentId=None, threadId=None)
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, 7)
        self.record(4, model="gpt-a", at=51)
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, 1)
        self.record(8, model="gpt-b", response="turn-3-response", turnId="turn-3")
        self.check()
        self.db.execute("DELETE FROM analytics_usage WHERE id='8'")
        self.check()
        self.record(1, response="response-1", model="claude-opus-5-5", inputTokensAreUncached=True,
                    delta={"inputTokens": 150, "cachedInputTokens": 20, "outputTokens": 10})
        self.check()
        self.log_message("response-1")
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, 0)
        self.log_message("other-response", tokens=200)
        self.check()
        self.log.unlink()
        self.check()
        self.pricing.value["providers"]["openai"]["models"]["gpt-a"]["cost"]["input"] = 3
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, 0)
        self.record(1, root="other", response="response-1")
        self.check()
        self.check("other")
        self.reader = costs.SessionCostReader(self.path, self.pricing, self.accounts)
        self.record(9, model=None)
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, 1)

    def test_random_mutations_match_full_history(self):
        rng = random.Random(4108)
        for step in range(100):
            identity = rng.randrange(1, 25)
            if rng.randrange(6) == 0:
                self.db.execute("DELETE FROM analytics_usage WHERE id=?", (str(identity),))
            else:
                self.record(identity, root=rng.choice(("lead", "other")), at=rng.randrange(100),
                    model=rng.choice((None, "", "gpt-a", "gpt-b", "claude-opus-5-5", "unknown")),
                    response=rng.choice((None, "", "r" + str(identity), False, 0, [], {}, [1], {"a": 1})),
                    turnId="turn-" + str(rng.randrange(5)), agentId=rng.choice(("lead", None, "other")),
                    threadId=rng.choice((None, "thread", "other")), inputTokensAreUncached=bool(step % 2),
                    delta={"inputTokens": rng.choice((150, True, 150.0, None)), "outputTokens": 10,
                           "cachedInputTokens": rng.choice((None, 20, False)), "cacheWriteInputTokens": 0},
                    last={"inputTokens": 100, "cachedInputTokens": 0, "outputTokens": 20})
            self.check()
            self.check("other")

    def test_journal_retention_forces_rebuild_and_remains_bounded(self):
        self.record(1, response="first")
        self.check()
        self.db.executemany("INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES "
                            "(?,'lead','lead','thread',?,?,?)", (
            (str(i), "turn-" + str(i), i, json.dumps({"agentId": "lead", "threadId": "thread",
             "turnId": "turn-" + str(i), "model": "gpt-a", "responseId": str(i),
             "delta": {"inputTokens": 100, "cachedInputTokens": 0, "outputTokens": 10}}))
            for i in range(2, usage.JOURNAL_REVISIONS + 4)))
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, usage.JOURNAL_REVISIONS + 3)
        count = self.db.execute("SELECT COUNT(*) FROM analytics_cost_changes_v1 WHERE root='lead'").fetchone()[0]
        self.assertEqual(count, usage.JOURNAL_REVISIONS)

    def test_journal_retention_during_delta_pages_forces_rebuild(self):
        self.record(1, response="first")
        self.check()
        for identity in range(2, 601):
            self.record(identity, response=str(identity))
        self.db.commit()
        original = usage.UsageIndex.apply
        applied = []
        def apply(index, changes, receipt_changes=()):
            original(index, changes, receipt_changes)
            applied.append(len(changes))
            if len(applied) == 1:
                self.db.execute("UPDATE analytics_cost_roots_v1 SET floor=512 WHERE root='lead'")
                self.db.execute("DELETE FROM analytics_cost_changes_v1 WHERE root='lead' AND revision<=512")
                self.db.commit()
        with patch.object(usage.UsageIndex, "apply", apply):
            with self.assertRaisesRegex(RuntimeError, "usage journal changed during cost capture"):
                self.reader._compute_shared("lead", "lead", refresh=True)
        self.assertEqual(self.check()["pricedSamples"], 600)
        self.assertEqual(self.reader.last_usage_capture_rows, 600)

    def test_capture_pages_release_shared_snapshot_and_reconcile_concurrent_change(self):
        for identity in range(1, usage.PAGE_ROWS + 3):
            self.record(identity, response=str(identity))
        self.db.commit()
        original = usage.UsageIndex._copy_page
        copied = []
        def copy_page(index, page):
            original(index, page)
            copied.append(len(page))
            self.assertEqual(tuple(self.db.execute("PRAGMA wal_checkpoint(TRUNCATE)").fetchone()), (0, 0, 0))
            if len(copied) == 1:
                self.record(1, response="corrected", model="gpt-b")
                self.db.execute("DELETE FROM analytics_usage WHERE id='2'")
                self.record(3, root="other", response="moved")
                self.record(usage.PAGE_ROWS + 1, response="changed-later-page", model="gpt-b")
                self.record(usage.PAGE_ROWS + 3, response="appended")
                self.db.commit()
        with patch.object(usage.UsageIndex, "_copy_page", copy_page):
            self.reader._compute_shared("lead", "lead", refresh=True)
        self.assertEqual(copied, [usage.PAGE_ROWS, 2])
        self.assertEqual(self.reader.last_usage_capture_rows, usage.PAGE_ROWS + 7)
        self.check()
        self.check("other")

    def test_page_copy_reconciles_sequence_moves_in_both_directions(self):
        for identity in range(1, usage.PAGE_ROWS + 3):
            self.record(identity, response=str(identity))
        self.db.commit()
        original = usage.UsageIndex._copy_page
        copied = []
        def copy_page(index, page):
            original(index, page)
            copied.append(len(page))
            if len(copied) == 1:
                self.db.execute("INSERT OR REPLACE INTO analytics_usage SELECT 257,id,agent,root,thread,turn,at,record FROM analytics_usage WHERE id='1'")
                self.db.execute("INSERT OR REPLACE INTO analytics_usage SELECT 2,id,agent,root,thread,turn,at,record FROM analytics_usage WHERE id='258'")
                self.db.commit()
        with patch.object(usage.UsageIndex, "_copy_page", copy_page):
            result = self.reader._compute_shared("lead", "lead", refresh=True)
        self.assertEqual(copied, [usage.PAGE_ROWS, 1])
        self.assertEqual(result["pricedSamples"], usage.PAGE_ROWS)
        self.check()

    def test_team_move_during_copy_rejects_the_old_chat_scope(self):
        self.record(1, response="first")
        self.db.commit()
        original = usage.UsageIndex._copy_page
        def copy_page(index, page):
            original(index, page)
            member = json.loads(self.db.execute("SELECT record FROM analytics_agents WHERE id='lead'").fetchone()[0])
            member["rootId"] = "other"
            self.db.execute("UPDATE analytics_agents SET record=? WHERE id='lead'", (json.dumps(member),))
            self.record(1, root="other", response="first")
            self.db.commit()
        with patch.object(usage.UsageIndex, "_copy_page", copy_page):
            with self.assertRaisesRegex(RuntimeError, "chat team changed during cost capture"):
                self.reader._compute_shared("lead", "lead", refresh=True)
        moved = self.reader.snapshot("lead", wait=True)
        self.assertEqual((moved["rootId"], moved["pricedSamples"]), ("other", 1))

    def test_large_numeric_tokens_remain_sqlite_real_values(self):
        fields = {"inputTokens": 1e30, "cachedInputTokens": 20, "outputTokens": 10}
        self.record(1, response="large", delta=fields)
        self.check()
        self.record(1, response="large", model="gpt-b", delta=fields)
        self.check()

    def test_transaction_rollback_and_changed_identity(self):
        self.record(1, response="first")
        self.check()
        before = usage.source_state(self.db, "lead")
        self.record(1, root="other", response="corrected", model="gpt-b")
        self.db.rollback()
        self.assertEqual(usage.source_state(self.db, "lead"), before)
        self.check()
        self.db.execute("UPDATE analytics_usage SET id='changed-id' WHERE id='1'")
        self.check()
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM analytics_cost_projection_v1").fetchone()[0], 1)
        self.db.execute("INSERT OR REPLACE INTO analytics_usage SELECT seq,'replacement',agent,root,thread,turn,at,record FROM analytics_usage")
        self.check()
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM analytics_cost_projection_v1").fetchone()[0], 1)

    def test_startup_does_not_backfill_history_and_root_lookup_uses_index(self):
        with closing(sqlite3.connect(self.root / "legacy.sqlite3")) as db:
            db.executescript("CREATE TABLE analytics_agents(id TEXT PRIMARY KEY,record TEXT);"
                             "CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,id TEXT UNIQUE,agent,root,thread,turn,at,record);")
            db.execute("INSERT INTO analytics_usage VALUES(1,'old','lead','lead','thread','turn',1,'{}')")
            db.commit()
            usage.install(db)
            self.assertEqual(db.execute("SELECT COUNT(*) FROM analytics_cost_projection_v1").fetchone()[0], 0)
            plan = db.execute("EXPLAIN QUERY PLAN SELECT id,record FROM analytics_agents WHERE id=? OR json_extract(record,'$.rootId')=?", ("lead", "lead")).fetchall()
            self.assertTrue(any("analytics_agents_root" in row[3] for row in plan), plan)

    def test_source_identity_and_corrupt_cache_force_rebuild(self):
        self.record(1, response="first")
        self.record(2, response="second")
        self.check()
        replacement = self.root / "replacement.sqlite3"
        with closing(sqlite3.connect(replacement)) as copied:
            self.db.backup(copied)
        self.db.close()
        replacement.replace(self.path)
        self.db = sqlite3.connect(self.path)
        self.db.row_factory = sqlite3.Row
        self.addCleanup(self.db.close)
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, 2)
        path = next((self.root / "session-cost-usage").glob("*.sqlite3"))
        path.write_bytes(b"invalid SQLite cache")
        self.record(3, response="third")
        self.check()
        self.assertEqual(self.reader.last_usage_capture_rows, 3)

    def test_prune_preserves_open_cache_and_file_permissions(self):
        directory = self.root / "cache"
        directory.mkdir()
        path = directory / "usage-v1-test.sqlite3"
        index = usage.UsageIndex(path)
        try:
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            usage.UsageIndex.prune(directory, 0)
            self.assertTrue(path.exists())
        finally:
            index.close()
        usage.UsageIndex.prune(directory, 0)
        self.assertFalse(path.exists())

    def test_capture_writer_work_does_not_scan_previous_usage(self):
        self.db.executemany("INSERT INTO analytics_usage(id,agent,root,thread,turn,at,record) VALUES "
                            "(?,'lead','lead','thread','turn',1,?)",
                            ((str(i), '{"model":"gpt-a","responseId":"r"}') for i in range(4096)))
        self.db.commit()
        operations = [0]
        def progress():
            operations[0] += 100
            return 0
        self.db.set_progress_handler(progress, 100)
        try:
            self.record(4097, response="new")
        finally:
            self.db.set_progress_handler(None, 0)
        # A projection scan executes over 24,000 instructions at this size.
        self.assertLess(operations[0], 2000, operations)
        self.db.rollback()

    def test_replace_of_unprojected_legacy_row_invalidates_both_roots(self):
        names = [row[0] for row in self.db.execute("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'analytics_cost_%'")]
        for name in names:
            self.db.execute('DROP TRIGGER "' + name + '"')
        self.record(1, response="first")
        self.record(2, response="second")
        self.db.commit()
        usage.install(self.db)
        self.db.commit()
        self.assertEqual(self.db.execute("SELECT COUNT(*) FROM analytics_cost_projection_v1").fetchone()[0], 0)
        self.assertEqual(self.check()["pricedSamples"], 2)
        record = self.db.execute("SELECT record FROM analytics_usage WHERE seq=1").fetchone()[0]
        self.db.execute("INSERT OR REPLACE INTO analytics_usage VALUES(1,'replacement','lead','other','thread','turn-1',1,?)", (record,))
        self.assertEqual(self.check()["pricedSamples"], 1)
        self.assertEqual(self.check("other")["pricedSamples"], 1)

    def test_receipt_delta_uses_legacy_text_affinity(self):
        self.record(1, response=123, model="claude-opus-5-5")
        self.check()
        self.log_message("123")
        self.assertEqual(self.check()["pricedSamples"], 1)
        self.log.unlink()
        self.assertEqual(self.check()["pricedSamples"], 1)
        member = json.loads(self.db.execute("SELECT record FROM analytics_agents WHERE id='lead'").fetchone()[0])
        member["accountHistory"] = [{"provider": "claude", "accountKey": "123", "threadId": "thread"}]
        self.db.execute("UPDATE analytics_agents SET record=? WHERE id='lead'", (json.dumps(member),))
        self.accounts["123"] = {"provider": "claude", "home": str(self.profile)}
        self.record(1, response=123, accountKey=123, model="claude-opus-5-5")
        self.check()
        self.log_message("123")
        self.assertEqual(self.check()["pricedSamples"], 1)
        self.log.unlink()
        self.check()

    def test_replace_can_remove_two_unprojected_legacy_identities(self):
        names = [row[0] for row in self.db.execute("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'analytics_cost_%'")]
        for name in names:
            self.db.execute('DROP TRIGGER "' + name + '"')
        self.record(1, response="first")
        self.record(2, response="second", root="other")
        self.db.commit()
        usage.install(self.db)
        self.db.commit()
        self.check()
        self.check("other")
        record = self.db.execute("SELECT record FROM analytics_usage WHERE id='2'").fetchone()[0]
        self.db.execute("INSERT OR REPLACE INTO analytics_usage VALUES(1,'2','lead','lead','thread','turn-2',2,?)", (record,))
        self.assertEqual(self.check()["pricedSamples"], 1)
        self.assertEqual(self.check("other")["pricedSamples"], 0)


def benchmark(count, legacy_base=False):
    fixture = IncrementalCosts()
    fixture.setUp()
    try:
        if legacy_base:
            names = [row[0] for row in fixture.db.execute("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'analytics_cost_%'")]
            for name in names:
                fixture.db.execute('DROP TRIGGER "' + name + '"')
        for identity in range(1, count + 1):
            fixture.record(identity, model=None if identity % 3 == 0 else "gpt-a",
                           response=str(identity) if identity % 3 == 1 else None,
                           turnId="turn-" + str((identity - 1) // 3))
        fixture.db.commit()
        if legacy_base:
            usage.install(fixture.db)
            fixture.db.commit()
        started = time.perf_counter()
        fixture.reader._compute_shared("lead", "lead", refresh=True)
        cold = time.perf_counter() - started
        results = []
        for append_count in (1, 10, 100):
            for identity in range(count + 1, count + append_count + 1):
                fixture.record(identity, model=None if identity % 3 == 0 else "gpt-a",
                               response=str(identity) if identity % 3 == 1 else None,
                               turnId="turn-" + str((identity - 1) // 3))
            count += append_count
            fixture.db.commit()
            fixture.statements.clear()
            started = time.perf_counter()
            result = fixture.reader._compute_shared("lead", "lead", refresh=True)
            elapsed = time.perf_counter() - started
            captured = fixture.reader.last_usage_capture_rows
            assert captured == append_count, (captured, append_count)
            assert not any("FROM analytics_usage u LEFT JOIN" in sql for sql in fixture.statements)
            results.append({"appended": append_count, "captured": captured, "seconds": elapsed,
                            "samples": result["pricedSamples"]})
        print(json.dumps({"initialRows": count - 111, "legacyBase": legacy_base,
                          "coldSeconds": cold, "refreshes": results}))
    finally:
        fixture.doCleanups()


def ingestion_benchmark():
    results = []
    for payload_bytes, count in ((4096, 2000), (65536, 500)):
        for incremental in (False, True):
            fixture = IncrementalCosts()
            fixture.setUp()
            try:
                if not incremental:
                    names = [row[0] for row in fixture.db.execute("SELECT name FROM sqlite_master WHERE type='trigger' AND name LIKE 'analytics_cost_%'")]
                    for name in names:
                        fixture.db.execute('DROP TRIGGER "' + name + '"')
                samples = []
                for repeat in range(3):
                    started = time.perf_counter()
                    for identity in range(repeat * count + 1, (repeat + 1) * count + 1):
                        fixture.record(identity, response=str(identity), unrelated="x" * payload_bytes)
                    fixture.db.commit()
                    insert = time.perf_counter() - started
                    started = time.perf_counter()
                    for identity in range(repeat * count + 1, (repeat + 1) * count + 1):
                        fixture.record(identity, response=str(identity), model="gpt-b", unrelated="x" * payload_bytes)
                    fixture.db.commit()
                    update = time.perf_counter() - started
                    samples.append((insert, update))
                results.append({"incremental": incremental, "payloadBytes": payload_bytes, "rows": count,
                    "insertMedianSeconds": statistics.median(sample[0] for sample in samples),
                    "updateMedianSeconds": statistics.median(sample[1] for sample in samples)})
            finally:
                fixture.doCleanups()
    print(json.dumps({"ingestion": results}))


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--benchmark":
        benchmark(int(sys.argv[2]) if len(sys.argv) > 2 else 30000, "--legacy-base" in sys.argv)
    elif len(sys.argv) > 1 and sys.argv[1] == "--ingestion-benchmark":
        ingestion_benchmark()
    else:
        unittest.main()
