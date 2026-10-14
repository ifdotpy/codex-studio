"""Run RulesRuntime.rules on an isolated Python-written state corpus."""

import json
import sqlite3
import sys
import threading
from contextlib import contextmanager
from pathlib import Path
from unittest.mock import patch

server_source = Path(__file__).resolve().parents[3] / "apps/server/src"
sys.path.insert(0, str(server_source))
sys.path.insert(0, str(Path(__file__).resolve().parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard()
from codex_rules import RulesMixin
from codex_sync_entities import ensure_tables, put as sync_entity_put


class Host(RulesMixin):
    def __init__(self, cwd):
        self.lock = threading.RLock()
        self.changed = threading.Event()
        self.connection = sqlite3.connect(":memory:")
        self.connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
        self.connection.execute("CREATE TABLE runtime_rules(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
        self.connection.execute("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
        ensure_tables(self.connection)
        self.connection.execute("INSERT INTO runtime_agents VALUES(?,?)", (
            "lead", json.dumps({"id":"lead", "rootId":"lead", "epoch":3,
                                "status":"running", "isLead":True, "autoWake":True,
                                "cwd":cwd, "concurrency":2, "maxAgents":8}),
        ))
        self.cancelled = []

    @contextmanager
    def db(self):
        with self.connection:
            yield self.connection

    def agent(self, key, db=None):
        db = db or self.connection
        row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        if row is None:
            raise ValueError("Unknown agent")
        return json.loads(row[0])

    def checked_actor(self, db, key, actor=None):
        value = self.agent(key, db)
        if actor is not None and actor != key:
            raise ValueError("The caller was stopped")
        return value

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table}")]

    def put(self, db, table, record):
        db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)",
                   (record["id"], json.dumps(record)))
        if table == "rules":
            sync_entity_put(db, "rule", record["id"], record)

    def workspace_path(self, agent_id, path):
        supplied = Path(path).expanduser()
        root = Path(self.agent(agent_id)["cwd"]).resolve()
        return (supplied if supplied.is_absolute() else root / supplied).resolve()

    def cancel_monitor(self, key):
        self.cancelled.append(key)


host = Host(sys.argv[1])
Path(sys.argv[1], "watch.txt").write_text("watch corpus")
with patch("codex_rules.time.time", return_value=100.0):
    saved = host.rules({"id":"rule-fixed", "kind":"file", "name":"watch",
                        "path":"watch.txt", "intervalSeconds":30}, actor="lead", epoch=3)
    with host.db() as db:
        db.execute("INSERT INTO runtime_monitors VALUES (?,?)", (
            "watch-monitor", json.dumps({"id":"watch-monitor", "agent":"lead",
                                         "ruleId":"rule-fixed", "status":"running"}),
        ))
    paused = host.rules({"action":"pause", "id":"rule-fixed"}, actor="lead", epoch=3)
    entity = host.connection.execute(
        "SELECT payload FROM sync_entities WHERE collection='rule' AND id='rule-fixed'"
    ).fetchone()[0]
print(json.dumps({"saved":saved, "paused":paused, "cancelled":host.cancelled,
                  "syncEntity":json.loads(entity)}, separators=(",", ":")))
