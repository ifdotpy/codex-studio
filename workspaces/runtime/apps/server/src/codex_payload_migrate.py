"""Online, resumable migration and retention for large runtime payloads.

Run against the installed application's state directory while its server is
serving requests. Each batch commits its cursor and changes together.
"""

from __future__ import annotations

import argparse
import errno
import json
import os
from pathlib import Path
import shutil
import sqlite3
import statistics
import sys
import time
import uuid
from collections.abc import Callable

from codex_payloads import (
    EXTERNALIZE_THRESHOLD,
    FREE_SPACE_RESERVE,
    TASK_NEWEST_PER_AGENT,
    TASK_PREVIEW_BYTES,
    TASK_RETENTION_SECONDS,
    collect_unreferenced,
    ensure_payload_schema,
    externalize_record,
    release_db_writer_lock,
    trim_task,
)
from codex_state import state_dir as default_state_dir
from studio_api.sync.resources.models import ResourceRef, TaskResource, TasksResource
from studio_api.sync.resources.relay.client import (
    NotifyCommittedWriteError,
    ResourceRelayClient,
)


TARGETS = {
    "checkpoints": ("runtime_checkpoints", "record"),
    "tool_requests": ("runtime_tool_requests", "record"),
    "tool_results": ("runtime_tool_results", "result"),
    "tasks": ("runtime_tasks", "record"),
}


def _connect(database: Path) -> sqlite3.Connection:
    db = sqlite3.connect(database, timeout=30)
    db.row_factory = sqlite3.Row
    # Migration writers yield quickly when the serving process owns SQLite.
    db.execute("PRAGMA busy_timeout=40")
    db.execute("PRAGMA synchronous=NORMAL")
    ensure_payload_schema(db)
    db.execute("CREATE TABLE IF NOT EXISTS runtime_payload_migrations "
               "(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,"
               "status TEXT NOT NULL DEFAULT 'pending',updated REAL,error TEXT)")
    columns = {row[1] for row in db.execute("PRAGMA table_info(runtime_payload_migrations)")}
    for name, declaration in (("status", "TEXT NOT NULL DEFAULT 'pending'"),
                              ("updated", "REAL"), ("error", "TEXT")):
        if name not in columns:
            db.execute(f"ALTER TABLE runtime_payload_migrations ADD COLUMN {name} {declaration}")
    return db


def _load_record(state: Path, table: str, raw: str) -> dict:
    from codex_payloads import resolve_record, resolve_result
    return (resolve_result(state, raw) if table == "tool_results"
            else resolve_record(state, json.loads(raw)))


def _encoded(table: str, record: dict) -> str:
    if table == "tool_results":
        return json.dumps(record, ensure_ascii=False)
    return json.dumps(record, ensure_ascii=False)


def _externalize(state: Path, db, table: str, record: dict) -> dict:
    if table == "tool_results":
        from codex_payloads import externalize_result
        return externalize_result(state, db, record)
    return externalize_record(state, db, table, record)


def _newest_ids(db, agent: str, cache: dict[str, set[str]]) -> set[str]:
    if agent not in cache:
        cache[agent] = {row[0] for row in db.execute(
            "SELECT id FROM runtime_tasks WHERE json_extract(record,'$.agent')=? "
            "ORDER BY json_extract(record,'$.created') DESC,id DESC LIMIT ?",
            (agent, TASK_NEWEST_PER_AGENT),
        )}
    return cache[agent]


def migrate_batch(state: Path, db, table: str, *, batch_rows: int = 16,
                  batch_bytes: int = 2 * 1024 * 1024, now: float | None = None,
                  notify: Callable[[str, list[ResourceRef]], object] | None = None) -> dict:
    if table not in TARGETS:
        raise ValueError("Unknown payload table")
    runtime_table, column = TARGETS[table]
    now = time.time() if now is None else now
    key = "payload-v1:" + table
    state_row = db.execute("SELECT cursor,complete FROM runtime_payload_migrations WHERE name=?", (key,)).fetchone()
    cursor = state_row[0] if state_row else 0
    if state_row and state_row[1]:
        return {"table": table, "done": True, "cursor": cursor, "rows": 0, "movedBytes": 0, "lockMs": 0}
    free = shutil.disk_usage(state).free
    if free < FREE_SPACE_RESERVE:
        db.execute("INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,error) "
                   "VALUES (?,?,0,'waitingForSpace',?,NULL) ON CONFLICT(name) DO UPDATE SET "
                   "status='waitingForSpace',updated=excluded.updated,error=NULL",
                   (key, cursor, now))
        db.commit()
        release_db_writer_lock(db)
        return {"table": table, "done": False, "waitingForSpace": True, "cursor": cursor,
                "rows": 0, "movedBytes": 0, "lockMs": 0}

    rows = db.execute(f"SELECT rowid,id,{column} FROM {runtime_table} WHERE rowid>? ORDER BY rowid LIMIT ?",
                      (cursor, batch_rows)).fetchall()
    age_cutoff = now - TASK_RETENTION_SECONDS
    staged = []
    staged_bytes = 0
    try:
        for row in rows:
            raw_record = row[column]
            record = _load_record(state, table, raw_record)
            updated = None
            next_record = record
            if table == "tasks":
                tail = record.get("tail")
                if (isinstance(tail, str) and record.get("status") != "running"
                        and record.get("created", now) < age_cutoff
                        and len(tail.encode("utf-8")) > TASK_PREVIEW_BYTES):
                    next_record = trim_task(record)
                    if next_record != record:
                        updated = json.dumps(next_record, ensure_ascii=False)
            else:
                next_record = _externalize(state, db, table, record)
                if next_record != record:
                    updated = _encoded(table, next_record)
            if updated is not None:
                staged_bytes += max(0, len(raw_record.encode("utf-8")) - len(updated.encode("utf-8")))
            staged.append((row, raw_record, updated, next_record))
            if staged_bytes >= batch_bytes:
                break
    except BaseException as error:
        release_db_writer_lock(db)
        waiting = isinstance(error, OSError) and error.errno == errno.ENOSPC
        try:
            db.execute("INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,error) "
                       "VALUES (?,?,0,?,?,?) ON CONFLICT(name) DO UPDATE SET "
                       "status=excluded.status,updated=excluded.updated,error=excluded.error",
                       (key, cursor, "waitingForSpace" if waiting else "error", now,
                        None if waiting else f"{type(error).__name__}: {error}"[:500]))
            db.commit()
        except Exception:
            pass
        if waiting:
            return {"table": table, "done": False, "waitingForSpace": True, "cursor": cursor,
                    "rows": 0, "movedBytes": 0, "lockMs": 0}
        raise

    started = time.perf_counter_ns()
    moved = scanned = 0
    conflict = False
    changed_tasks: dict[str, set[str]] = {}
    try:
        try:
            db.execute("BEGIN IMMEDIATE")
        except sqlite3.OperationalError as error:
            if "locked" not in str(error).lower() and "busy" not in str(error).lower():
                raise
            release_db_writer_lock(db)
            lock_ms = (time.perf_counter_ns() - started) / 1_000_000
            return {"table": table, "done": False, "cursor": cursor, "rows": 0,
                    "movedBytes": 0, "lockMs": round(lock_ms, 3), "busy": True}
        last = cursor
        newest_cache: dict[str, set[str]] = {}
        for row, raw_record, updated, next_record in staged:
            current = db.execute(f"SELECT {column} FROM {runtime_table} WHERE id=?", (row["id"],)).fetchone()
            if current is None or current[0] != raw_record:
                conflict = True
                break
            if table == "tasks" and updated is not None:
                record = json.loads(updated)
                if row["id"] in _newest_ids(db, str(record.get("agent", "")), newest_cache):
                    updated = None
            if updated is not None:
                db.execute(f"UPDATE {runtime_table} SET {column}=? WHERE id=?", (updated, row["id"]))
                if table == "tasks" and db.execute(
                        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_entity_meta'").fetchone():
                    from codex_sync_entities import sync_task_write
                    sync_task_write(db, next_record)
                    task_id = str(next_record.get("id", row["id"]))
                    agent_id = next_record.get("agent")
                    if isinstance(agent_id, str) and agent_id:
                        changed_tasks.setdefault(agent_id, set()).add(task_id)
                moved += max(0, len(raw_record.encode("utf-8")) - len(updated.encode("utf-8")))
            scanned += 1
            last = row["rowid"]
        done = not conflict and len(rows) < batch_rows and len(staged) == len(rows)
        db.execute("INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,error) "
                   "VALUES (?,?,?, ?,?,NULL) ON CONFLICT(name) DO UPDATE SET "
                   "cursor=excluded.cursor,complete=excluded.complete,status=excluded.status,"
                   "updated=excluded.updated,error=NULL",
                   (key, last, int(done), "complete" if done else "running", now))
        db.commit()
    except BaseException:
        db.rollback()
        raise
    finally:
        release_db_writer_lock(db)
    if notify is not None and changed_tasks:
        resources: list[ResourceRef] = []
        for agent_id in sorted(changed_tasks):
            resources.append(ResourceRef(TasksResource(kind="tasks", agentId=agent_id)))
            resources.extend(
                ResourceRef(TaskResource(kind="task", taskId=task_id))
                for task_id in sorted(changed_tasks[agent_id])
            )
        request_id = str(uuid.uuid4())
        try:
            notify(request_id, resources)
        except NotifyCommittedWriteError as error:
            raise NotifyCommittedWriteError(
                f"Payload batch committed at cursor {last}; UI invalidation is unconfirmed: {error}"
            ) from error
    lock_ms = (time.perf_counter_ns() - started) / 1_000_000
    return {"table": table, "done": done, "cursor": last, "rows": scanned,
            "movedBytes": moved, "lockMs": round(lock_ms, 3), "conflict": conflict}


def run(state: Path, *, tables: list[str], batch_rows: int, batch_bytes: int,
        max_batches: int | None, restart_tasks: bool = False,
        relay: ResourceRelayClient | None = None) -> dict:
    db = _connect(state / "canvas.sqlite3")
    samples = {table: [] for table in tables}
    bytes_moved = {table: 0 for table in tables}
    counts = {table: 0 for table in tables}
    try:
        if restart_tasks:
            db.execute("DELETE FROM runtime_payload_migrations WHERE name='payload-v1:tasks'")
            db.commit()
        batches = 0
        while max_batches is None or batches < max_batches:
            unfinished = False
            for table in tables:
                effective_rows = min(batch_rows, 1) if table == "tasks" else batch_rows
                result = migrate_batch(
                    state,
                    db,
                    table,
                    batch_rows=effective_rows,
                    batch_bytes=batch_bytes,
                    notify=relay.notify if relay is not None else None,
                )
                if result.get("busy"):
                    time.sleep(0.05)
                    unfinished = True
                    continue
                if result.get("waitingForSpace"):
                    unfinished = True
                    time.sleep(30)
                    continue
                if not result["done"]:
                    unfinished = True
                samples[table].append(result["lockMs"])
                bytes_moved[table] += result["movedBytes"]
                counts[table] += result["rows"]
            batches += 1
            if not unfinished:
                break
        def percent(values, q):
            if not values:
                return 0
            values = sorted(values)
            return values[min(len(values) - 1, int((len(values) - 1) * q))]
        return {"stateDir": str(state), "thresholdBytes": EXTERNALIZE_THRESHOLD,
                "taskRetentionDays": TASK_RETENTION_SECONDS // 86400,
                "taskNewestPerAgent": TASK_NEWEST_PER_AGENT,
                "taskPreviewBytes": TASK_PREVIEW_BYTES,
                "tables": {table: {"batches": len(samples[table]), "rowsScanned": counts[table],
                                   "bytesFreedFromRows": bytes_moved[table],
                                   "lockMsP50": percent(samples[table], .50),
                                   "lockMsP95": percent(samples[table], .95),
                                   "lockMsMax": max(samples[table], default=0)}
                           for table in tables}}
    finally:
        db.close()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, default=default_state_dir())
    parser.add_argument("--table", choices=(*TARGETS, "all"), default="all")
    parser.add_argument("--batch-rows", type=int, default=8)
    parser.add_argument("--batch-bytes", type=int, default=2 * 1024 * 1024)
    parser.add_argument("--max-batches", type=int)
    parser.add_argument("--restart-tasks", action="store_true",
                        help="rescan tasks for output newly eligible for seven-day retention")
    parser.add_argument("--gc", action="store_true", help="collect unreferenced blobs older than seven days")
    args = parser.parse_args(argv)
    state = args.state_dir.expanduser().resolve()
    if args.gc:
        db = _connect(state / "canvas.sqlite3")
        try:
            print(json.dumps(collect_unreferenced(state, db, now=time.time()), sort_keys=True))
        finally:
            db.close()
        return 0
    if args.batch_rows < 1 or args.batch_bytes < 1 or (args.max_batches is not None and args.max_batches < 1):
        parser.error("batch limits must be positive")
    tables = list(TARGETS) if args.table == "all" else [args.table]
    if args.restart_tasks and tables != ["tasks"]:
        parser.error("--restart-tasks requires --table tasks")
    try:
        result = run(state, tables=tables, batch_rows=args.batch_rows, batch_bytes=args.batch_bytes,
                     max_batches=args.max_batches, restart_tasks=args.restart_tasks,
                     relay=ResourceRelayClient(state))
    except NotifyCommittedWriteError as error:
        print(
            f"Source payload batch committed; UI invalidation is unconfirmed. "
            f"Do not repeat the source batch to recover it. {error}",
            file=sys.stderr,
        )
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
