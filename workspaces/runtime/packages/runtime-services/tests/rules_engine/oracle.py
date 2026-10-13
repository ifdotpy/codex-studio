"""Real RulesMixin decisions and Python-written records; never construct Runtime.

The small host supplies SQLite, deterministic observations and captures effects.
RulesMixin and Runtime._put execute unchanged. There is no provider or process.
"""
from __future__ import annotations

import argparse
import copy
import json
from pathlib import Path
import sqlite3
import sys
import threading
from contextlib import contextmanager
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_rules import RulesMixin
from codex_runtime import Runtime
from codex_sync_entities import ensure_tables, register_functions

NOW = 1_800_000_000.0
OWNER = {"id": "owner", "rootId": "owner", "epoch": 7, "status": "idle", "autoWake": True,
         "isLead": True, "accountKey": "fixture", "threadId": "thread-fixture", "role": "orchestrator",
         "approvalPolicy": "never", "sandbox": {"type": "workspaceWrite", "writableRoots": []}}


class Host(RulesMixin):
    put = Runtime._put

    def __init__(self):
        self.connection = sqlite3.connect(":memory:")
        for table in ("rules", "agents", "monitors"):
            self.connection.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
        ensure_tables(self.connection)
        register_functions(self.connection)
        self.lock = threading.RLock()
        self.closed = False
        self.changed = threading.Event()
        self.pool = self
        self.wakes = []
        self.launches = []
        self.fingerprint = [1_700_000_000_000_000_000, 5, 9]
        self.insert("agents", OWNER)

    def sync_workspace_volatile(self, db):
        pass

    def insert(self, table, record):
        self.connection.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES(?,?)", (record["id"], json.dumps(record)))

    @contextmanager
    def db(self):
        yield self.connection

    read_db = db

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table} ORDER BY rowid")]

    def agent(self, key, db=None):
        return json.loads((db or self.connection).execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])

    def checked_actor(self, db, key, actor=None):
        return self.agent(key, db)

    def team_agents(self, db, root):
        return [a for a in self.records(db, "agents") if a["rootId"] == root and not a.get("deletedAt")]

    def workspace_path(self, agent, path):
        return Path(path)

    def file_fingerprint(self, path):
        return copy.deepcopy(self.fingerprint)

    def enqueue_recovery_event(self, db, agent, kind, text, key):
        self.wakes.append({"id": key, "kind": kind, "text": text})

    def submit(self, function, rule):
        function(rule)

    def prepare(self, agent):
        return agent

    def monitor_auto_approved(self, agent):
        return True

    def monitor(self, agent, data, key, *, approved, epoch, rule):
        self.launches.append({"agent": agent, "data": data, "key": key, "approved": approved,
                              "epoch": epoch, "rule": copy.deepcopy(rule)})


def base(kind="interval"):
    host = Host()
    body = {"agent": "owner", "id": "rule", "name": "Watch 東京", "kind": kind,
            "intervalSeconds": 60, "at": NOW, "text": "Continue 🦀", "path": "/disposable/watch"}
    if kind == "low_workers":
        body.update(minimumWorkers=2, durationMinutes=1)
    with patch("codex_rules.time.time", return_value=NOW):
        host.rules_action(body)
    record = host.records(host.connection, "rules")[0]
    host.connection.close()
    return record


def evaluate(name, operation, rule, *, owner=None, now=NOW, fingerprint=None, workers=(), monitors=(), event=None, result=None):
    host = Host()
    owner = copy.deepcopy(owner or OWNER)
    host.insert("agents", owner)
    for worker in workers:
        host.insert("agents", worker)
    for monitor in monitors:
        host.insert("monitors", monitor)
    host.put(host.connection, "rules", copy.deepcopy(rule))
    stored = host.records(host.connection, "rules")[0]
    host.fingerprint = fingerprint
    with patch("codex_rules.time.time", return_value=now):
        if operation == "tick":
            host.rules_tick()
        elif operation == "recover":
            host.setup_rules(host.connection)
        elif operation == "event":
            host.rule_event(host.connection, owner, event["kind"], event["text"], event["key"])
        elif operation == "finish":
            host.rule_finished("rule", result["code"], result["error"], result["output"])
        else:
            raise AssertionError(operation)
    value = {"name": name, "operation": operation, "rule": stored, "owner": host.agent("owner"),
             "now": now, "fingerprint": fingerprint, "workers": list(workers), "monitors": list(monitors),
             "event": event, "result": result,
             "expected": {"rule": host.records(host.connection, "rules")[0], "wakes": host.wakes, "launches": host.launches}}
    host.connection.close()
    return value


def cases(full):
    specifications = json.loads(Path(__file__).with_name("vectors.json").read_text())
    result = []
    for spec in specifications:
        if spec["name"].startswith("matrix-") and not full:
            continue
        spec = copy.deepcopy(spec)
        name, operation, kind = (spec.pop(field) for field in ("name", "operation", "kind"))
        rule = base(kind)
        rule.update(spec.pop("changes", {}))
        for field in spec.pop("remove", []):
            rule.pop(field)
        if "ownerChanges" in spec:
            spec["owner"] = {**OWNER, **spec.pop("ownerChanges")}
        if "repeatOutput" in spec.get("result", {}):
            repeat = spec["result"].pop("repeatOutput")
            spec["result"]["output"] = repeat["character"] * repeat["count"]
        result.append(evaluate(name, operation, rule, **spec))
    return result


def template(directory):
    """Clone the committed Python schema, then seed with the real rules writer."""
    directory.mkdir(parents=True, exist_ok=True)
    schema = json.loads((ROOT / "workspaces/runtime/packages/runtime-store/tests/fixtures/live-state-schema.json").read_text())
    for name in ("canvas.sqlite3", "analytics.sqlite3"):
        db = sqlite3.connect(directory/name)
        db.execute("PRAGMA journal_mode=WAL")
        db.execute("PRAGMA synchronous=FULL")
        register_functions(db)
        db.execute("BEGIN")
        objects = sorted(schema[name]["sqlite_master"],key=lambda o:({"table":0,"index":1,"trigger":2,"view":3}[o["type"]],o["name"]))
        for obj in objects:
            if obj["sql"] and not obj["name"].startswith("runtime_search_next_"):
                db.execute(obj["sql"])
        if name == "canvas.sqlite3":
            host = Host()
            db.execute("INSERT INTO runtime_agents VALUES(?,?)",("owner",json.dumps(OWNER)))
            host.put(db,"rules",base())
            host.connection.close()
        db.commit()
        db.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--full", action="store_true")
    parser.add_argument("--template", type=Path)
    args = parser.parse_args()
    if args.template:
        template(args.template)
    else:
        print(json.dumps(cases(args.full)))
