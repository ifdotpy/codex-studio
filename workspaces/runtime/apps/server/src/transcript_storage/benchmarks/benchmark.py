"""Synthetic production-code SQLite microbenchmark for streaming FTS work."""
import argparse
import json
import math
import sqlite3
import statistics
import time

from transcript_storage.storage import (
    ITEM_BACKFILL_BATCH_SIZE,
    ITEM_BACKFILL_TEXT_BUDGET_BYTES,
    STREAM_INDEX_DELAY_SECONDS,
    backfill_items,
    drain,
    index_item,
    initialize,
    persist,
)

FRAGMENT_INTERVAL_SECONDS = 0.1
# Keep in sync with Runtime.schedule's changed.wait(1) interval.
SCHEDULER_INTERVAL_SECONDS = 1.0
SCHEDULER_FRAGMENT_CADENCE = round(SCHEDULER_INTERVAL_SECONDS / FRAGMENT_INTERVAL_SECONDS)


def run_trial(fragments, fragment_chars, *, coalesced):
    db = sqlite3.connect(":memory:")
    db.row_factory = sqlite3.Row
    db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
    initialize(db)
    item_id = "synthetic:assistant"
    full = ""
    index_writes = 0

    def count_index_write(statement):
        nonlocal index_writes
        if statement.lstrip().upper().startswith("INSERT INTO RUNTIME_SEARCH(ID,AGENT,KIND,BODY)"):
            index_writes += 1

    db.set_trace_callback(count_index_write)
    started = time.perf_counter()
    for fragment in range(fragments):
        full += (f"fragment{fragment} " + "x" * max(0, fragment_chars - len(str(fragment)) - 9))
        record = json.dumps({"id": item_id, "title": "assistant", "text": full[-20000:]})
        db.execute("INSERT INTO runtime_items VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                   (item_id, "synthetic", record, fragment))
        if coalesced:
            persist(db, item_id, "synthetic", "assistant", full, streaming=True, now=fragment)
        else:
            index_item(db, item_id, "synthetic", "assistant", full)
    if coalesced:
        # The benchmarked final authoritative item replaces the streamed prefix.
        persist(db, item_id, "synthetic", "assistant", full, streaming=False, now=fragments + 1)
        drain(db, now=fragments + 1)
    elapsed = time.perf_counter() - started
    token = full.split()[0]
    actual = db.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH ? LIMIT 1", (token,)).fetchone()
    assert actual and actual[0] == full
    db.close()
    return elapsed, index_writes


def run_checkpoint_trial(fragments, fragment_chars):
    """Model continuous fragments and a periodic scheduler using synthetic time."""
    db = sqlite3.connect(":memory:")
    db.row_factory = sqlite3.Row
    db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
    initialize(db)
    item_id = "synthetic:assistant"
    full = ""
    index_writes = 0
    checkpoints = 0
    dirty_since = None
    max_dirty_age = 0.0

    def count_index_write(statement):
        nonlocal index_writes
        if statement.lstrip().upper().startswith("INSERT INTO RUNTIME_SEARCH(ID,AGENT,KIND,BODY)"):
            index_writes += 1

    db.set_trace_callback(count_index_write)
    started = time.perf_counter()
    for fragment in range(fragments):
        now = fragment * FRAGMENT_INTERVAL_SECONDS
        if dirty_since is None:
            dirty_since = now
        full += (f"fragment{fragment} " + "x" * max(0, fragment_chars - len(str(fragment)) - 9))
        record = json.dumps({"id": item_id, "title": "assistant", "text": full[-20000:]})
        db.execute("INSERT INTO runtime_items VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                   (item_id, "synthetic", record, fragment))
        persist(db, item_id, "synthetic", "assistant", full, streaming=True, now=now)
        if (fragment + 1) % SCHEDULER_FRAGMENT_CADENCE == 0:
            count = drain(db, now=now)
            if count:
                indexed = db.execute("SELECT body FROM runtime_search WHERE rowid=(SELECT search_rowid FROM runtime_search_rows WHERE id=?)",
                                     (item_id,)).fetchone()
                assert indexed and indexed[0] == full
                checkpoints += count
                max_dirty_age = max(max_dirty_age, now - dirty_since)
                dirty_since = None
    final_now = fragments * FRAGMENT_INTERVAL_SECONDS
    persist(db, item_id, "synthetic", "assistant", full, streaming=False, now=final_now)
    if dirty_since is not None:
        max_dirty_age = max(max_dirty_age, final_now - dirty_since)
    actual = db.execute("SELECT body FROM runtime_search WHERE runtime_search MATCH 'fragment0' LIMIT 1").fetchone()
    assert actual and actual[0] == full
    elapsed = time.perf_counter() - started
    db.close()
    return elapsed, index_writes, checkpoints, max_dirty_age


def run_migration_trial():
    """Measure one production migration page with 20 KB legacy excerpts."""
    db = sqlite3.connect(":memory:")
    db.row_factory = sqlite3.Row
    db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
    initialize(db)
    db.execute("UPDATE runtime_search_address_cursor SET done=1 WHERE singleton=1")
    excerpt = ("representative long transcript excerpt word " * 500)[:20000]
    record = json.dumps({"title": "assistant", "text": excerpt, "truncated": True})
    db.executemany("INSERT INTO runtime_items VALUES (?,?,?,?)",
                   ((f"legacy:{row}", "synthetic", record, row) for row in range(ITEM_BACKFILL_BATCH_SIZE)))
    started = time.perf_counter()
    processed = backfill_items(db)
    elapsed = time.perf_counter() - started
    assert processed > 0
    assert db.execute("SELECT rowid FROM runtime_search_item_cursor").fetchone()[0] == processed
    assert db.execute("SELECT count(*) FROM runtime_search_partial").fetchone()[0] == processed
    db.close()
    return elapsed, processed, len(excerpt.encode("utf-8"))


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, math.ceil(len(ordered) * p) - 1)]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="run a small correctness/work-count check")
    parser.add_argument("--trials", type=int, default=21)
    parser.add_argument("--fragments", type=int, default=200)
    parser.add_argument("--fragment-chars", type=int, default=24)
    args = parser.parse_args()
    trials = 3 if args.check else max(3, args.trials)
    fragments = 60 if args.check else max(1, args.fragments)
    baseline = [run_trial(fragments, args.fragment_chars, coalesced=False) for _ in range(trials)]
    one_write = [run_trial(fragments, args.fragment_chars, coalesced=True) for _ in range(trials)]
    checkpointed = [run_checkpoint_trial(fragments, args.fragment_chars) for _ in range(trials)]
    migration = [run_migration_trial() for _ in range(trials)]
    if args.check and (checkpointed[0][1] >= baseline[0][1]
                       or checkpointed[0][2] < 1
                       or checkpointed[0][3] > STREAM_INDEX_DELAY_SECONDS + SCHEDULER_INTERVAL_SECONDS
                       or migration[0][1] >= ITEM_BACKFILL_BATCH_SIZE):
        raise SystemExit("scheduler checkpoint work or freshness bound failed")
    report = {}
    for name, values in (("burst_per_fragment_microbenchmark", baseline),
                         ("burst_one_final_write_microbenchmark", one_write),
                         ("continuous_scheduler_checkpoints", checkpointed)):
        timings = [value[0] * 1000 for value in values]
        report[name] = {"p50_ms": statistics.median(timings), "p95_ms": percentile(timings, .95),
                        "p99_ms": percentile(timings, .99), "fts_writes": values[0][1],
                        "fragments": fragments}
        if name == "continuous_scheduler_checkpoints":
            report[name]["scheduler_checkpoints"] = values[0][2]
            report[name]["max_dirty_age_seconds"] = values[0][3]
            report[name]["freshness_bound_seconds"] = STREAM_INDEX_DELAY_SECONDS + SCHEDULER_INTERVAL_SECONDS
    migration_ms = [value[0] * 1000 for value in migration]
    report["legacy_item_migration_long_excerpt_batch"] = {
        "p50_ms": statistics.median(migration_ms),
        "p95_ms": percentile(migration_ms, .95),
        "p99_ms": percentile(migration_ms, .99),
        "rows_processed": migration[0][1],
        "row_limit": ITEM_BACKFILL_BATCH_SIZE,
        "excerpt_bytes_per_row": migration[0][2],
        "text_byte_budget": ITEM_BACKFILL_TEXT_BUDGET_BYTES,
    }
    report["timing_scope"] = "In-memory SQLite production-code microbenchmark; not durable filesystem or application latency."
    print(json.dumps(report, sort_keys=True, indent=2))


if __name__ == "__main__":
    main()
