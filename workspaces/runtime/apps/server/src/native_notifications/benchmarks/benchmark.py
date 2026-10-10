#!/usr/bin/env python3
"""Measure the production native agent selector against synthetic SQLite rows."""
import argparse
import json
import math
from pathlib import Path
import sqlite3
import statistics
import sys
import tempfile
import time

SCRIPT_ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(SCRIPT_ROOT))

from native_notifications import dispatch

ACCOUNT_KEY = "default"
CONNECTION_ID = "benchmark-connection"
TARGET_THREAD = "benchmark-target"
MATCHING_AGENTS = 3
DEFAULT_ROWS = (1000, 10000, 50000)
DEFAULT_ROUNDS = 100

INDEX_SQL = """CREATE INDEX runtime_agent_native_scope ON runtime_agents(
    json_extract(record,'$.threadId'),
    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
         ELSE json_extract(record,'$.accountKey') END)"""


class IndexedRuntime:
    def records(self, _db, _table):
        raise AssertionError("indexed thread lookup unexpectedly scanned all agents")


def make_database(path, row_count):
    db = sqlite3.connect(path)
    db.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
    db.execute(INDEX_SQL)
    target = [{"id": f"target-{number}", "threadId": TARGET_THREAD,
               "accountKey": ACCOUNT_KEY, "inFlight": True}
              for number in range(MATCHING_AGENTS)]
    db.executemany("INSERT INTO runtime_agents VALUES (?,?)",
                   ((agent["id"], json.dumps(agent)) for agent in target))
    remaining = max(0, row_count - len(target))
    db.executemany("INSERT INTO runtime_agents VALUES (?,?)",
                   ((f"other-{number}", json.dumps({
                       "id": f"other-{number}", "threadId": f"thread-{number}",
                       "accountKey": ACCOUNT_KEY, "inFlight": True}))
                    for number in range(remaining)))
    db.commit()
    return db


def query_plan(db):
    return [row[3] for row in db.execute(
        "EXPLAIN QUERY PLAN SELECT record FROM runtime_agents "
        "WHERE json_extract(record,'$.threadId')=? "
        "AND CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default' "
        "ELSE json_extract(record,'$.accountKey') END=?",
        (TARGET_THREAD, ACCOUNT_KEY))]


def percentile(samples, percentile_value):
    ordered = sorted(samples)
    return ordered[max(0, math.ceil(percentile_value * len(ordered)) - 1)]


def measure(row_count, rounds):
    with tempfile.TemporaryDirectory(prefix="native-notification-benchmark-") as directory:
        db = make_database(Path(directory) / "agents.sqlite", row_count)
        plan = query_plan(db)
        decoded = 0
        original_mode_fields = dispatch.mode_fields

        def count_decoded(agent):
            nonlocal decoded
            decoded += 1
            return original_mode_fields(agent)

        dispatch.mode_fields = count_decoded
        runtime = IndexedRuntime()
        samples = []
        try:
            for _ in range(rounds):
                started = time.perf_counter_ns()
                matches = dispatch.matching_agents(runtime, db, ACCOUNT_KEY, TARGET_THREAD)
                samples.append((time.perf_counter_ns() - started) / 1000)
        finally:
            dispatch.mode_fields = original_mode_fields
        db.close()
    return {
        "rowsInDatabase": row_count,
        "matchingAgents": len(matches),
        "decodedRowsPerLookup": decoded // rounds,
        "rounds": rounds,
        "lookupMicroseconds": {
            "p50": round(statistics.median(samples), 3),
            "p95": round(percentile(samples, 0.95), 3),
            "p99": round(percentile(samples, 0.99), 3),
        },
        "queryPlan": plan,
    }


def run_check():
    reports = [measure(row_count, rounds=3) for row_count in (1000, 10000)]
    for report in reports:
        if report["matchingAgents"] != MATCHING_AGENTS:
            raise AssertionError("selector failed to return every matching agent")
        if report["decodedRowsPerLookup"] != MATCHING_AGENTS:
            raise AssertionError("selector decoded rows outside the matching thread scope")
        if not any("runtime_agent_native_scope" in step for step in report["queryPlan"]):
            raise AssertionError("SQLite did not use the existing native scope index")
    if reports[0]["decodedRowsPerLookup"] != reports[1]["decodedRowsPerLookup"]:
        raise AssertionError("decoded row count grew with unrelated database rows")
    print(json.dumps({"check": "passed", "evidence": reports}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="verify index use and decoded-row scaling")
    parser.add_argument("--rows", nargs="+", type=int, default=DEFAULT_ROWS,
                        help="synthetic total agent rows for each measured database")
    parser.add_argument("--rounds", type=int, default=DEFAULT_ROUNDS)
    args = parser.parse_args()
    if args.rounds < 1 or any(row_count < MATCHING_AGENTS for row_count in args.rows):
        parser.error("rounds must be positive and rows must include the matching agents")
    if args.check:
        run_check()
    else:
        print(json.dumps({"benchmark": "native_notifications.matching_agents",
                          "evidence": [measure(row_count, args.rounds) for row_count in args.rows]}, indent=2))


if __name__ == "__main__":
    main()
