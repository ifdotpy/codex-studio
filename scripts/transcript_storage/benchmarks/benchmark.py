"""Synthetic production-code benchmark: FTS work per streamed fragment."""
import argparse
import json
import sqlite3
import statistics
import time

from transcript_storage.storage import drain, index_item, initialize, persist


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


def percentile(values, p):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int((len(ordered) - 1) * p))]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true", help="run a small correctness/work-count check")
    parser.add_argument("--trials", type=int, default=21)
    parser.add_argument("--fragments", type=int, default=200)
    parser.add_argument("--fragment-chars", type=int, default=24)
    args = parser.parse_args()
    trials = 3 if args.check else max(3, args.trials)
    fragments = 20 if args.check else max(1, args.fragments)
    baseline = [run_trial(fragments, args.fragment_chars, coalesced=False) for _ in range(trials)]
    optimized = [run_trial(fragments, args.fragment_chars, coalesced=True) for _ in range(trials)]
    if args.check and optimized[0][1] >= baseline[0][1]:
        raise SystemExit("coalesced indexing did not reduce FTS writes")
    report = {}
    for name, values in (("per_fragment", baseline), ("coalesced", optimized)):
        timings = [value[0] * 1000 for value in values]
        report[name] = {"p50_ms": statistics.median(timings), "p95_ms": percentile(timings, .95),
                        "p99_ms": percentile(timings, .99), "fts_writes": values[0][1],
                        "fragments": fragments}
    print(json.dumps(report, sort_keys=True, indent=2))


if __name__ == "__main__":
    main()
