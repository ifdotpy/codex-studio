"""Run the Python model_directory projection over the Rust corpus rows."""

import json
import sqlite3
import sys
import threading
from contextlib import contextmanager
from pathlib import Path

runtime_root = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(runtime_root / "apps/server/src"))
sys.path.insert(0, str(Path(__file__).resolve().parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard()
from codex_efficiency import EfficiencyMixin


class Host(EfficiencyMixin):
    def __init__(self, payload):
        self.lock = threading.RLock()
        self.connection = sqlite3.connect(":memory:")
        self.connection.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
        self.connection.execute(
            "CREATE INDEX runtime_agent_reservation_cwd ON runtime_agents(json_extract(record,'$.cwd'))"
        )
        self.connection.execute("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT)")
        self.connection.execute("CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,epoch INTEGER,status TEXT)")
        for record in payload["agents"]:
            self.connection.execute("INSERT INTO runtime_agents VALUES(?,?)", (record["id"], json.dumps(record)))
        for record in payload["monitors"]:
            self.connection.execute("INSERT INTO runtime_monitors VALUES(?,?)", (record["id"], json.dumps(record)))

    @contextmanager
    def db(self):
        with self.connection:
            yield self.connection

    def agent(self, key, db):
        row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        if row is None:
            raise ValueError("Unknown agent")
        return json.loads(row[0])

    def checked_actor(self, db, key, actor=None):
        record = self.agent(key, db)
        if actor != key or record.get("deletedAt"):
            raise ValueError("The caller was stopped")
        return record

    def team_agents(self, db, root):
        return [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId')=? "
            "AND (json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt') IN (0,'')) ORDER BY rowid",
            (root,))]

    @staticmethod
    def worker_defaults(root):
        from codex_nested_delegation import DEFAULT_DELEGATION_DEPTH
        return {"model": "gpt-6-luna", "effort": "high", "fastMode": False,
                "daybreakEnabled": False, "delegationDepth": DEFAULT_DELEGATION_DEPTH,
                **root.get("workerDefaults", {})}

    @staticmethod
    def dispatch_active_slots(db):
        slots = {row[0]: {"id": row[0], "rootId": row[1]} for row in db.execute(
            "SELECT id,json_extract(record,'$.rootId') FROM runtime_agents "
            "WHERE json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval') "
            "UNION ALL SELECT id,json_extract(record,'$.rootId') FROM runtime_agents "
            "WHERE json_extract(record,'$.startAttempt.activeAtReservation')=1 AND EXISTS ("
            "SELECT 1 FROM json_each(runtime_agents.record,'$.startAttempt.events') AS input "
            "JOIN runtime_events AS event ON event.id=input.value WHERE event.agent=runtime_agents.id "
            "AND event.epoch=json_extract(runtime_agents.record,'$.startAttempt.epoch') "
            "AND event.status IN ('reserved','dispatching','uncertain'))")}
        return [slots[key] for key in sorted(slots)]


payload = json.loads(sys.argv[1])
host = Host(payload)
result = host.model_directory(payload["actor"], "orchestration_status", payload["args"])
print(json.dumps(result, separators=(",", ":")))
