"""Python diagnostics oracle over disposable Python-written SQLite rows."""
import json
from pathlib import Path
import sqlite3
import sys
from contextlib import closing, nullcontext
from types import SimpleNamespace

TESTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TESTS / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))
from codex_diagnostics import migration_status
from codex_execution import chain

root = Path(sys.argv[1])
root.mkdir(parents=True, exist_ok=True)
database = root / "canvas.sqlite3"
with closing(sqlite3.connect(database)) as db:
    db.executescript("""
        CREATE TABLE runtime_execution_runs (
            id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL,
            epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL,
            record TEXT NOT NULL);
        CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT NOT NULL,record TEXT NOT NULL);
        CREATE TABLE runtime_execution_nodes(id TEXT PRIMARY KEY,run TEXT NOT NULL,record TEXT NOT NULL);
        CREATE TABLE runtime_execution_effects(id TEXT PRIMARY KEY,run TEXT NOT NULL,kind TEXT NOT NULL,reference TEXT NOT NULL,record TEXT NOT NULL);
        CREATE TABLE runtime_execution_inputs(run TEXT NOT NULL,event TEXT NOT NULL,PRIMARY KEY(run,event));
        CREATE TABLE runtime_events(id TEXT PRIMARY KEY,status TEXT);
        CREATE TABLE runtime_search_rollout(id INTEGER PRIMARY KEY,phase TEXT,cursor INTEGER,updated REAL);
        CREATE TABLE runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER,complete INTEGER,status TEXT,updated REAL,error TEXT);
        CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    """)
    run = {
        "id": "run:fixture", "agent": "agent-fixture", "accountKey": "default",
        "epoch": 2, "threadId": "thread-fixture", "turnId": "turn-fixture",
        "created": 1700000000.0, "status": "completed", "firstAttemptId": "attempt:fixture",
        "rootAttemptId": "attempt:fixture", "latestAttemptId": "attempt:fixture",
        "attempts": [], "nodes": [], "effects": [], "attemptIds": [], "requestIds": [],
        "inputEventIds": [],
    }
    db.execute("INSERT INTO runtime_execution_runs VALUES(?,?,?,?,?,?,?,?)",
               (run["id"], run["agent"], run["accountKey"], run["epoch"], run["threadId"],
                run["turnId"], run["created"], json.dumps(run)))
    attempt = {"id": "attempt:fixture", "actionRequestId": "request-fixture"}
    db.execute("INSERT INTO runtime_execution_attempts VALUES(?,?,?)",
               (attempt["id"], run["id"], json.dumps(attempt)))
    effect = {"id": "effect:fixture", "requestId": "request-effect"}
    db.execute("INSERT INTO runtime_execution_effects VALUES(?,?,?,?,?)",
               (effect["id"], run["id"], "requests", "fixture", json.dumps(effect)))
    db.execute("INSERT INTO runtime_execution_nodes VALUES(?,?,?)",
               ("node:fixture", run["id"], json.dumps({"id": "node:fixture"})))
    db.execute("INSERT INTO runtime_execution_inputs VALUES(?,?)", (run["id"], "event-fixture"))
    for index, status in enumerate(("pending", "reserved", "dispatching", "uncertain", "delivered")):
        db.execute("INSERT INTO runtime_events VALUES(?,?)", (f"event-{index}", status))
    db.execute("INSERT INTO runtime_search_rollout VALUES(1,'building',7,1700000001.0)")
    db.execute("INSERT INTO runtime_payload_migrations VALUES('payload-v1:tasks',3,0,'waitingForSpace',1700000002.0,NULL)")
    db.execute("INSERT INTO sync_entity_meta VALUES('entity_tombstone_count','4')")
    db.execute("INSERT INTO sync_entity_meta VALUES('entity_tombstone_floor','2')")
    db.commit()
    expected = {
        "executions": chain(db),
        "durableInputPending": db.execute(
            "SELECT COUNT(*) FROM runtime_events WHERE status IN ('pending','reserved','dispatching','uncertain')"
        ).fetchone()[0],
        "migrations": migration_status(SimpleNamespace(
            db=lambda: nullcontext(db), search_migration_error=None,
            sync_store=SimpleNamespace(entity_prune_status={
                "status": "notStarted", "updated": 0, "deleted": 0,
            }),
        )),
    }
    print(json.dumps(expected, separators=(",", ":")))
