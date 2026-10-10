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
from codex_sync import SyncStore
from codex_sync_entities import ensure_tables, put


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
            db.executescript("CREATE TABLE analytics_usage(id TEXT PRIMARY KEY, record TEXT); CREATE VIRTUAL TABLE search_fts USING fts5(text);")
            ensure_tables(db)
        store = SyncStore(connect, lambda key: {"items": []})
        first = store.pull("state:entities:v1")
        after = first["checkpoint"]["seq"]
        timings = {"analyticsChurn": [], "uiWrites": []}
        bytes_out = {"analyticsChurn": 0, "uiWrites": 0}
        documents = {"analyticsChurn": 0, "uiWrites": 0}
        for i in range(iterations):
            with connect() as db:
                db.execute("INSERT INTO analytics_usage VALUES (?, '{}')", (f"a{i}",))
                db.execute("INSERT INTO search_fts VALUES (?)", (f"search {i}",))
            started = time.perf_counter_ns()
            result = store.pull("state:entities:v1", after)
            timings["analyticsChurn"].append((time.perf_counter_ns() - started) / 1e6)
            bytes_out["analyticsChurn"] += sum(len(row["payload"].encode()) for row in result["documents"])
            documents["analyticsChurn"] += len(result["documents"])
            with connect() as db:
                put(db, "project", f"project-{i}", {
                    "id": f"project-{i}", "path": f"/benchmark/project-{i}", "name": "team",
                })
            started = time.perf_counter_ns()
            result = store.pull("state:entities:v1", after)
            timings["uiWrites"].append((time.perf_counter_ns() - started) / 1e6)
            bytes_out["uiWrites"] += sum(len(row["payload"].encode()) for row in result["documents"])
            documents["uiWrites"] += len(result["documents"])
            after = result["checkpoint"]["seq"]
        return {
            "schemaVersion": 1,
            "benchmark": "sync_production_store",
            "iterations": iterations,
            "payloadBytes": bytes_out,
            "entityDocuments": documents,
            "latencyMs": {name: {"p50": percentile(values, .50), "p95": percentile(values, .95), "p99": percentile(values, .99)} for name, values in timings.items()},
            "databaseGeneration": store.generation(),
            "expectedAnalyticsEntityDocuments": 0,
            "expectedProjectEntityDocuments": iterations,
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
    if result["entityDocuments"]["analyticsChurn"] != result["expectedAnalyticsEntityDocuments"]:
        raise SystemExit("Analytics-only writes changed the entity projection")
    if result["entityDocuments"]["uiWrites"] != result["expectedProjectEntityDocuments"]:
        raise SystemExit("Project writes did not produce the expected entity documents")
    payload = json.dumps(result, indent=2, sort_keys=True)
    if args.output:
        args.output.write_text(payload + "\n", encoding="utf-8")
    print(payload)


if __name__ == "__main__":
    main()
