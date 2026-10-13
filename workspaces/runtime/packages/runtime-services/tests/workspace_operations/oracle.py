"""Real Python workspace functions, guarded before any application imports."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
GUARD = install_guard(block_processes=True)
import contextlib
import json
import sqlite3
from unittest.mock import patch
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))
from codex_workspace import WorkspaceMixin
import codex_workspace


class Harness(WorkspaceMixin):
    closed = False

    def __init__(self, actor, operations=(), checkpoints=(), items=(), committed=True):
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        self.connection.executescript("""
          CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_checkpoints(id TEXT PRIMARY KEY, record TEXT);
          CREATE TABLE runtime_items(id TEXT PRIMARY KEY, agent TEXT, record TEXT, created REAL);
          CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);
        """)
        self.root = Path.cwd()
        self.put(self.connection, "agents", actor)
        for operation in operations:
            self.put(self.connection, "workspace_operations", operation)
        for checkpoint in checkpoints:
            self.put(self.connection, "checkpoints", checkpoint)
        for item in items:
            self.connection.execute("INSERT INTO runtime_items VALUES(?,?,?,?)", (item["id"], actor["id"], json.dumps(item), item["created"]))
        if committed:
            self.connection.execute("INSERT INTO runtime_completed_turns VALUES(?)", (actor["id"] + ":" + str(actor.get("lastCompletedTurn")),))

    def put(self, db, table, value):
        db.execute(f"INSERT OR REPLACE INTO runtime_{table}(id,record) VALUES(?,?)", (value["id"], json.dumps(value)))

    def agent(self, key, db):
        return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table}")]

    def permanent_worker_hold(self, *_args):
        pass


def evaluate(case):
    actor = case["actor"]
    runtime = Harness(actor, case.get("operations", []), case.get("checkpoints", []), case.get("items", []), case.get("committed", True))
    db = runtime.connection
    try:
        if case["family"] == "completion":
            requested = case.get("request", actor)
            request = (requested["id"], requested["epoch"], requested["cwd"], requested.get("accountKey", "default"),
                       requested.get("threadId"), requested.get("lastCompletedTurn"), (requested.get("startAttempt") or {}).get("id"))
            eligible = runtime._checkpoint_completed_agent(db, request) is not None
            eligible = eligible and not actor.get("imageWorkspace") and bool(actor.get("worktreeReady"))
            return {"actor": runtime.agent(actor["id"], db), "eligible": eligible}
        if case["family"] == "history":
            selected = next(c for c in case["checkpoints"] if c["id"] == case["selected"])
            return {"ids": sorted(runtime._checkpoint_history_ids(db, selected))}
        if case["family"] == "provider":
            return runtime._workspace_provider_result(case["response"])
        if case["family"] == "recovery":
            with patch.object(codex_workspace.time, "time", return_value=10.0):
                runtime.setup_workspace(db)
            return {"operation": runtime.records(db, "workspace_operations")[0], "agent": runtime.agent(actor["id"], db)}
        raise AssertionError(case["family"])
    except ValueError as error:
        return {"error": str(error)}
    finally:
        db.close()


def run(case):
    runtime = Harness(case["actor"], case.get("operations", []), case.get("checkpoints", []), case.get("items", []), case.get("committed", True))
    try:
        stored = {"actor": runtime.agent(case["actor"]["id"], runtime.connection),
                  "operations": runtime.records(runtime.connection, "workspace_operations"),
                  "checkpoints": runtime.records(runtime.connection, "checkpoints"),
                  "items": runtime.records(runtime.connection, "items")}
    finally:
        runtime.connection.close()
    return {"stored": stored, "result": evaluate(case)}


request = json.load(sys.stdin)
if request.get("command") == "seed":
    actor = dict(id="fixture", rootId="fixture", name="Workspace fixture", cwd=request["cwd"],
                 worktreeReady=True, autoWake=True, status="idle", epoch=1, accountKey="default",
                 inFlight=False, threadId="thread", lastCompletedTurn="turn", startAttempt={"id":"attempt"})
    schema_path = Path(__file__).resolve().parents[3] / "runtime-store/tests/fixtures/live-state-schema.json"
    schema = json.loads(schema_path.read_text())
    for name in ("canvas.sqlite3", "analytics.sqlite3"):
        with sqlite3.connect(Path(request["root"]) / name) as db:
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA synchronous=FULL")
            db.execute("BEGIN")
            objects = sorted(schema[name]["sqlite_master"], key=lambda o: {"table":0,"index":1,"trigger":2}.get(o["type"],3))
            for obj in objects:
                if obj["name"] in {"runtime_search_next_config", "runtime_search_next_data", "runtime_search_next_docsize", "runtime_search_next_idx"}:
                    continue
                if obj.get("sql"):
                    db.execute(obj["sql"])
            if name == "canvas.sqlite3":
                runtime = Harness(actor)
                runtime.connection.close()
                runtime.put(db, "agents", actor)
                # Real Python reservation/settlement writers produce the seed records.
                with patch.object(codex_workspace.time, "time", return_value=1.0):
                    operation = runtime._reserve_checkpoint(db, actor, "checkpoint", "prior-turn")
                    runtime._settle_checkpoint(db, actor, operation)
                db.execute("INSERT INTO runtime_completed_turns VALUES(?)", (actor["id"] + ":turn",))
                db.execute("INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('agent',?,1,'fixture',?,0)",
                           (actor["id"],json.dumps({"collection":"agent","id":actor["id"],"value":actor})))
    print(json.dumps(actor))
else:
    print(json.dumps([run(case) for case in request["cases"]]))
