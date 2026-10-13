"""Committed Python schema plus the digest-pinned Python row writer."""
import json
import sqlite3
import sys
from pathlib import Path
from oracle import writer
from codex_sync_entities import register_functions

root = Path(sys.argv[1])
root.mkdir(parents=True, exist_ok=True)
fixture = Path(__file__).resolve().parents[3] / "runtime-store/tests/fixtures/live-state-schema.json"
schema = json.loads(fixture.read_text())
statement = writer()
for name in ("canvas.sqlite3", "analytics.sqlite3"):
    # Build one template in memory, then persist it atomically with SQLite backup.
    with sqlite3.connect(":memory:") as db:
        register_functions(db)
        objects = sorted(schema[name]["sqlite_master"], key=lambda x: ({"table":0,"index":1,"trigger":2,"view":3}[x["type"]],x["name"]))
        for obj in objects:
            if obj["name"] in {"runtime_search_next_config","runtime_search_next_data","runtime_search_next_docsize","runtime_search_next_idx"}: continue
            db.execute(obj["sql"])
        if name == "canvas.sqlite3":
            db.execute("INSERT INTO runtime_agent_record_generation VALUES (1,0)")
            db.execute("INSERT INTO runtime_scheduler_agent_change_state VALUES (1,0,0,0,0)")
            agent = dict(id="agent",rootId="agent",epoch=0,status="failed",accountKey="default",threadId="thread",turnId=None,inFlight=False,autoWake=True,error="AF_UNIX path too long",
                         startAttempt={"id":"attempt","epoch":0,"accountKey":"default","events":["event"],"submitted":False,"activeAtReservation":False})
            exec(statement,{"db":db,"table":"agents","record":agent,"json":json})
            db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('event','agent','user','original input','pending',1,0,NULL,NULL)")
        db.commit()
        with sqlite3.connect(root / name) as destination:
            db.backup(destination)
            destination.execute("PRAGMA journal_mode=WAL")
            destination.execute("PRAGMA synchronous=FULL")
