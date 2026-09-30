#!/usr/bin/env python3
"""Compare full agent decoding with the indexed team read on an offline DB copy."""
import argparse
import json
import sqlite3
import statistics
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from codex_runtime import Runtime


def measure(runtime, operation, repetitions):
    values = []
    for _ in range(repetitions):
        began = time.monotonic_ns()
        with runtime.lock:
            result = operation()
        values.append((time.monotonic_ns() - began) / 1e6)
        if isinstance(result, list):
            count = len(result)
        else:
            count = result
        if count <= 0:
            raise RuntimeError("benchmark query returned no agents")
    ordered = sorted(values)
    return {"p50LockMs": round(statistics.median(values), 3),
            "p95LockMs": round(ordered[min(len(ordered) - 1, int((len(ordered) - 1) * .95))], 3),
            "samples": len(values), "rows": count}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("database", type=Path, help="offline copy of canvas.sqlite3")
    parser.add_argument("--repetitions", type=int, default=7)
    args = parser.parse_args()
    if args.repetitions < 1:
        parser.error("--repetitions must be positive")
    path = args.database.resolve()
    db = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True)
    db.row_factory = sqlite3.Row
    db.execute("PRAGMA query_only=ON")
    runtime = Runtime.__new__(Runtime)
    runtime.lock = threading.RLock()
    runtime._agent_records_cache_lock = threading.RLock()
    runtime._agent_record_revision = 0
    roots = db.execute("SELECT json_extract(record,'$.rootId') AS root, COUNT(*) AS n "
                       "FROM runtime_agents GROUP BY root ORDER BY n DESC LIMIT 1").fetchone()
    if not roots or not roots["root"]:
        parser.error("database has no agents with a rootId")
    root_id = roots["root"]

    def all_agents():
        runtime._agent_record_revision += 1
        runtime.__dict__.setdefault("_agent_records_cache", {}).clear()
        return runtime.records(db, "agents")

    full = measure(runtime, all_agents, args.repetitions)
    team = measure(runtime, lambda: runtime.team_agents(db, root_id), args.repetitions)
    plan = [row[3] for row in db.execute(
        "EXPLAIN QUERY PLAN SELECT record FROM runtime_agents "
        "WHERE json_extract(record,'$.rootId')=?", (root_id,))]
    print(json.dumps({"agentRows": db.execute("SELECT COUNT(*) FROM runtime_agents").fetchone()[0],
                      "teamRoot": root_id, "teamRows": roots["n"],
                      "fullDecode": full, "teamRead": team, "teamQueryPlan": plan}, indent=2))
    db.close()


if __name__ == "__main__":
    main()
