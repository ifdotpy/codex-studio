#!/usr/bin/env python3
"""Build a bounded /tmp copy from a read-only DB sample and measure before/after."""

import json
from collections import Counter
import math
from pathlib import Path
import random
import sqlite3
import statistics
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from codex_payload_migrate import migrate_batch
from codex_payloads import ensure_payload_schema
from codex_state import state_dir
from codex_transcript_history import history_rows
from codex_runtime import Runtime


def percent(values, q):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, math.ceil(q * len(ordered)) - 1)] if ordered else 0


def sample_rows(source, table, fields, count):
    max_rowid = source.execute(f"SELECT max(rowid) FROM {table}").fetchone()[0] or 0
    positions = sorted({max(1, min(max_rowid, int((i + .5) * max_rowid / count)))
                        for i in range(min(count, max_rowid))})
    rows = []
    for position in positions:
        row = source.execute(f"SELECT {fields} FROM {table} WHERE rowid>=? LIMIT 1", (position,)).fetchone()
        if row:
            rows.append(tuple(row))
    return rows


def timed(operation, repetitions=35):
    operation()
    values = []
    for _ in range(repetitions):
        start = time.perf_counter_ns()
        operation()
        values.append((time.perf_counter_ns() - start) / 1_000_000)
    return {"p50Ms": round(percent(values, .50), 3), "p95Ms": round(percent(values, .95), 3),
            "maxMs": round(max(values), 3)}


def copy_sample(source_path: Path, destination: Path):
    source = sqlite3.connect(source_path.resolve().as_uri() + "?mode=ro", uri=True, timeout=3)
    source.row_factory = sqlite3.Row
    target = sqlite3.connect(destination)
    target.row_factory = sqlite3.Row
    target.execute("PRAGMA journal_mode=WAL")
    target.execute("PRAGMA synchronous=NORMAL")
    ensure_payload_schema(target)
    target.executescript("""
      CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL,created REAL NOT NULL);
      CREATE INDEX runtime_item_agent ON runtime_items(agent,created);
      CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'),json_extract(record,'$.created'));
      CREATE INDEX runtime_task_history ON runtime_tasks(json_extract(record,'$.created') DESC,json_extract(record,'$.agent')) WHERE json_extract(record,'$.status')!='running';
      CREATE INDEX runtime_task_agent_created_id ON runtime_tasks(json_extract(record,'$.agent'),json_extract(record,'$.created') DESC,id DESC);
      CREATE INDEX runtime_task_agent_updated_id ON runtime_tasks(json_extract(record,'$.agent'),CASE WHEN COALESCE(json_extract(record,'$.finished'),0)>COALESCE(json_extract(record,'$.created'),0) THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END,id);
      CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE INDEX runtime_monitor_status ON runtime_monitors(json_extract(record,'$.status'),json_extract(record,'$.created'));
      CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
      CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT 'pending',updated REAL,error TEXT);
    """)

    agent_candidates = [json.loads(row[0])["id"] for row in source.execute(
        "SELECT record FROM runtime_agents WHERE json_extract(record,'$.isLead')=1 "
        "ORDER BY json_extract(record,'$.updated') DESC LIMIT 80")]
    agent_counts = [(source.execute("SELECT count(*) FROM runtime_items WHERE agent=?", (agent,)).fetchone()[0], agent)
                    for agent in agent_candidates]
    transcript_agent = max(agent_counts)[1] if agent_counts else None
    items = (source.execute("SELECT id,agent,record,created FROM runtime_items WHERE agent=? "
                            "AND json_extract(record,'$.afterRestore') IS NULL ORDER BY created DESC LIMIT 1000",
                            (transcript_agent,)).fetchall() if transcript_agent else [])
    target.executemany("INSERT INTO runtime_items VALUES (?,?,?,?)", [tuple(row) for row in items])

    task_samples = sample_rows(source, "runtime_tasks", "id,record", 5000)
    target.executemany("INSERT OR IGNORE INTO runtime_tasks VALUES (?,?)", task_samples)
    feed_agent = Counter(json.loads(row[1]).get("agent") for row in task_samples).most_common(1)[0][0]
    for table, column in (("runtime_checkpoints", "record"),
                          ("runtime_tool_requests", "record"),
                          ("runtime_tool_results", "result")):
        sampled = sample_rows(source, table, f"id,{column}", 500)
        target.executemany(f"INSERT OR IGNORE INTO {table}(id,{column}) VALUES (?,?)", sampled)
    source.close()
    target.commit()
    return target, transcript_agent, feed_agent, len(items), len(task_samples)


def main():
    state = state_dir()
    with tempfile.TemporaryDirectory(prefix="codex-payload-bench-") as temporary:
        target, transcript_agent, feed_agent, item_count, task_count = copy_sample(
            state / "canvas.sqlite3", Path(temporary) / "partial.sqlite3")
        def transcript():
            rows, limit = history_rows(target, transcript_agent, limit=120)
            return [json.loads(row["record"]) for row in rows[:limit]]

        def task_feed():
            rows = Runtime._workspace_task_rows(target, [feed_agent], order="created", limit=100)
            return [Runtime._workspace_task_summary(row[2]) for row in rows[:100]]

        before = {"transcriptPage": timed(transcript), "taskFeed": timed(task_feed)}
        table_names = ["checkpoints", "tool_requests", "tool_results", "tasks"]
        migration_times, migration_times_by_table = [], {name: [] for name in table_names}
        total_bytes, total_rows = 0, 0
        started = time.perf_counter()
        while True:
            pending = False
            for table in table_names:
                # Entity sync work can make task row updates much costlier
                # than blob-reference updates. Keep those write locks to a
                # single task while batching the other tables.
                result = migrate_batch(Path(temporary), target, table,
                                       batch_rows=1 if table == "tasks" else 8)
                migration_times.append(result["lockMs"])
                migration_times_by_table[table].append(result["lockMs"])
                total_bytes += result["movedBytes"]
                total_rows += result["rows"]
                pending |= not result["done"]
            if not pending:
                break
        elapsed = time.perf_counter() - started
        after = {"transcriptPage": timed(transcript), "taskFeed": timed(task_feed)}
        report = {"fixture": "partial-copy from read-only live SQLite samples; automatically deleted",
                  "transcriptAgent": transcript_agent, "transcriptItems": item_count,
                  "taskRows": task_count, "migrationRowsScanned": total_rows,
                  "rowBytesReduced": total_bytes, "migrationSeconds": round(elapsed, 3),
                  "migrationBytesPerSecond": round(total_bytes / elapsed) if elapsed else 0,
                  "migrationLockMs": {"p50": round(percent(migration_times, .50), 3),
                                      "p95": round(percent(migration_times, .95), 3),
                                      "max": round(max(migration_times, default=0), 3)},
                  "migrationLockMsByTable": {
                      table: {"p50": round(percent(values, .50), 3),
                              "p95": round(percent(values, .95), 3),
                              "max": round(max(values, default=0), 3)}
                      for table, values in migration_times_by_table.items()},
                  "before": before, "after": after}
        print(json.dumps(report, sort_keys=True))
        target.close()


if __name__ == "__main__":
    main()
