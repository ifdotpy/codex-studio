"""Exercise the real Python safety action and stage transitions without a provider."""
import json
import sqlite3
import sys
import threading
from concurrent.futures import Future
from pathlib import Path
from unittest.mock import patch

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
import codex_safety_buffering as safety
from codex_sync_entities import register_functions


AGENT = {
    "id": "agent",
    "rootId": "agent",
    "epoch": 7,
    "status": "running",
    "accountKey": "acct",
    "threadId": "thread",
    "turnId": "turn",
    "cwd": "/synthetic/project",
    "inFlight": True,
    "autoWake": True,
    "fastMode": False,
    "nativeSafetyBuffering": {
        "threadId": "thread",
        "turnId": "turn",
        "connectionId": "connection",
        "showBufferingUi": True,
        "fasterModel": "model-fast",
    },
}


def prepare_template(root: Path) -> None:
    schema_path = ROOT / "workspaces/runtime/packages/runtime-store/tests/fixtures/live-state-schema.json"
    schema = json.loads(schema_path.read_text())
    root.mkdir(parents=True, exist_ok=True)
    for database in ("canvas.sqlite3", "analytics.sqlite3"):
        with sqlite3.connect(root / database) as db:
            register_functions(db)
            objects = sorted(
                schema[database]["sqlite_master"],
                key=lambda item: ({"table": 0, "index": 1, "trigger": 2, "view": 3}[item["type"]], item["name"]),
            )
            for item in objects:
                if not item.get("sql") or item["name"].startswith("runtime_search_next_"):
                    continue
                db.execute(item["sql"])
            if database == "canvas.sqlite3":
                db.execute("INSERT INTO runtime_agent_record_generation VALUES (1,0)")
                db.execute("INSERT INTO runtime_scheduler_agent_change_state VALUES (1,0,0,0,0)")
                db.execute("INSERT INTO runtime_agents(id,record) VALUES (?,?)", (AGENT["id"], json.dumps(AGENT)))
            db.commit()
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA synchronous=FULL")


class NoTimer:
    def __init__(self, *_args, **_kwargs):
        pass

    def start(self):
        pass

    def cancel(self):
        pass


class Server:
    def __init__(self):
        self.callbacks = []

    def on_result(self, _submitted, callback):
        self.callbacks.append(callback)


class RuntimePort:
    def __init__(self):
        self.connection = sqlite3.connect(":memory:")
        self.connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.execute("CREATE TABLE runtime_safety_retries(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        self.connection.execute(
            "INSERT INTO runtime_agents VALUES (?,?)", (AGENT["id"], json.dumps(AGENT))
        )
        self.lock = threading.RLock()
        self.servers = {"acct": Server()}
        self.calls = []
        self.loaded = set()
        self.preparations = {}
        self.closed = False
        self.request_id = 0

    def db(self):
        return self.connection

    def checked_actor(self, _db, key):
        return self.agent(key)

    def agent(self, key, _db=None):
        row = self.connection.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        return json.loads(row[0])

    def put(self, db, table, record):
        db.execute(
            "INSERT INTO runtime_" + table + "(id,record) VALUES(?,?) "
            "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            (record["id"], json.dumps(record, ensure_ascii=False)),
        )

    def touch_ui(self, _agent):
        pass

    def connection_current(self, account, connection):
        return account == "acct" and connection == "connection"

    def assert_workspace_available(self, _db, _agent):
        pass

    def operation_current(self, agent, operation):
        return agent["epoch"] == operation["epoch"] and agent["accountKey"] == operation["accountKey"]

    def new_thread_params(self, _agent):
        return {}

    def turn_permissions(self, _agent):
        return {}

    def submit_reserved(self, _server, method, params):
        self.request_id += 1
        row = self.connection.execute(
            "SELECT record FROM runtime_safety_retries ORDER BY id"
        ).fetchone()
        stage = json.loads(row[0])["stage"]
        self.calls.append({"stage": stage, "method": method, "params": params})
        return (f"request-{self.request_id}", method, Future())

    def start_accepted(self, _agent_id, _attempt, _result):
        pass


def run_path():
    runtime = RuntimePort()
    with patch.object(safety, "Timer", NoTimer):
        safety.action(runtime, "agent", {"safety": "retry", "turnId": "turn"})
        for stage, result in [
            ("turns", {"data": [{"id": "turn", "status": "inProgress"}]}),
            ("items", {"data": [{"turnId": "turn", "item": {"type": "userMessage", "content": [{"type": "text", "text": "hello"}]}}]}),
            ("interrupt", {}),
            ("verify_turns", {"data": [{"id": "turn", "status": "interrupted"}]}),
            ("verify_items", {"data": [{"turnId": "turn", "item": {"type": "userMessage", "content": [{"type": "text", "text": "hello"}]}}]}),
            ("fork", {"thread": {"id": "new-thread"}}),
            ("start", {"turn": {"id": "new-turn"}}),
        ]:
            row = runtime.connection.execute(
                "SELECT record FROM runtime_safety_retries ORDER BY id"
            ).fetchone()
            operation = json.loads(row[0])
            future = Future()
            future.set_result(result)
            safety.complete(runtime, operation, stage, future)
    stored = json.loads(runtime.connection.execute(
        "SELECT record FROM runtime_safety_retries ORDER BY id"
    ).fetchone()[0])
    agent = runtime.agent("agent")
    return {
        "calls": runtime.calls,
        "stage": stored["stage"],
        "input": stored.get("input"),
        "threadId": agent.get("threadId"),
        "turnId": agent.get("turnId"),
    }


if __name__ == "__main__":
    prepare_template(Path(sys.argv[1]))
    print(json.dumps({"path": run_path()}, ensure_ascii=False))
