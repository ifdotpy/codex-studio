"""Bounded isolated SyncStore benchmark; no live Studio state is opened."""
import argparse
import contextlib
import json
import math
from pathlib import Path
import sqlite3
import statistics
import tempfile
import time

import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[2]))
from sync.sync_store import SyncStore


def percentile(values, percent):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(percent * len(ordered)) - 1)]


def run(iterations):
    with tempfile.TemporaryDirectory(prefix="sync-bench-") as directory:
        path = Path(directory) / "synthetic.sqlite3"
        @contextlib.contextmanager
        def connect():
            db = sqlite3.connect(path, timeout=5)
            try:
                with db:
                    yield db
            finally:
                db.close()
        with connect() as db:
            db.executescript("CREATE TABLE groups(id TEXT PRIMARY KEY, name TEXT, members TEXT); CREATE TABLE analytics_usage(id TEXT PRIMARY KEY, record TEXT); CREATE VIRTUAL TABLE search_fts USING fts5(text);")
        state_builds = 0
        def state():
            nonlocal state_builds
            state_builds += 1
            with connect() as db:
                group_count = db.execute("SELECT count(*) FROM groups").fetchone()[0]
            return {"agents": [], "groupCount": group_count, "largeProjection": "x" * 3_000_000}
        store = SyncStore(connect, state, lambda key: {"items": []})
        first = store.pull("state")
        after = first["checkpoint"]["seq"]
        timings = {"analyticsChurn": [], "uiWrites": []}
        bytes_out = {"analyticsChurn": 0, "uiWrites": 0}
        observed_invalidations = 0
        previous_generation = store.generations()["state"]
        for i in range(iterations):
            with connect() as db:
                db.execute("INSERT INTO analytics_usage VALUES (?, '{}')", (f"a{i}",))
                db.execute("INSERT INTO search_fts VALUES (?)", (f"search {i}",))
            started = time.perf_counter_ns()
            result = store.pull("state", after)
            timings["analyticsChurn"].append((time.perf_counter_ns() - started) / 1e6)
            bytes_out["analyticsChurn"] += sum(len(row["payload"].encode()) for row in result["documents"])
            if store.generations()["state"] != previous_generation:
                observed_invalidations += 1
            with connect() as db:
                db.execute("INSERT OR REPLACE INTO groups VALUES (?, 'team', '[]')", (f"g{i}",))
            started = time.perf_counter_ns()
            result = store.pull("state", after)
            timings["uiWrites"].append((time.perf_counter_ns() - started) / 1e6)
            bytes_out["uiWrites"] += sum(len(row["payload"].encode()) for row in result["documents"])
            after = result["checkpoint"]["seq"]
            current_generation = store.generations()["state"]
            if current_generation != previous_generation:
                observed_invalidations += 1
                previous_generation = current_generation
        return {
            "schemaVersion": 1,
            "benchmark": "sync_scoped_invalidation",
            "iterations": iterations,
            "payloadBytes": bytes_out,
            "latencyMs": {name: {"p50": percentile(values, .50), "p95": percentile(values, .95), "p99": percentile(values, .99)} for name, values in timings.items()},
            "stateBuildCount": state_builds,
            "scopedInvalidationCount": observed_invalidations,
            "storeSnapshotBuildCount": store.snapshot_builds,
            "legacyBroadGeneration": store.generation(),
            "expectedAnalyticsStateInvalidations": 0,
        }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--iterations", type=int, default=300)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    if not 1 <= args.iterations <= 5000:
        parser.error("--iterations must be between 1 and 5000")
    result = run(2 if args.check else args.iterations)
    expected_iterations = 2 if args.check else args.iterations
    if result["scopedInvalidationCount"] != expected_iterations:
        raise SystemExit("Scoped invalidation count did not match relevant UI writes")
    if result["stateBuildCount"] != result["scopedInvalidationCount"] + 1:
        raise SystemExit("Unexpected state projection rebuild count")
    payload = json.dumps(result, indent=2, sort_keys=True)
    if args.output:
        args.output.write_text(payload + "\n", encoding="utf-8")
    print(payload)


if __name__ == "__main__":
    main()
