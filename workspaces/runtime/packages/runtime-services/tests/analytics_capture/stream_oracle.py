"""Python oracle for a committed two-sample text stream capture."""
import json
import sqlite3
import sys
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parents[4] / "apps/server/src"))
from codex_analytics import AnalyticsMixin
from codex_streaming import StreamBuffer


class Runtime(AnalyticsMixin):
    def __init__(self):
        self.closed = False

    def records(self, *_args):
        return []

    def analytics_budget_capture(self, *_args, **_kwargs):
        return None

    def connection_current(self, _account, _connection):
        return True

    def item(self, *_args, **_kwargs):
        return None

    def put(self, db, table, value):
        db.execute(f"UPDATE runtime_{table} SET record=? WHERE id=?",
                   (json.dumps(value), value["id"]))


def main():
    agent = {
        "id": "agent",
        "name": "Agent",
        "rootId": "root",
        "parentId": None,
        "accountKey": "default",
        "accountHistory": [],
        "provider": "codex",
        "threadId": "thread",
        "turnId": "turn",
        "model": "gpt-5",
        "effort": "high",
        "fastMode": False,
        "daybreakEnabled": False,
        "cyberAccessProgram": False,
        "cwd": "/disposable",
        "deletedAt": None,
        "epoch": 0,
        "status": "running",
        "events": 0,
        "activity": None,
        "tail": "",
    }
    db = sqlite3.connect(":memory:")
    db.row_factory = sqlite3.Row
    runtime = Runtime()
    with patch("time.time", return_value=12.0):
        runtime.analytics_init(db)
    db.executescript("""
      CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_items(id TEXT PRIMARY KEY,record TEXT NOT NULL);
      CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL);
    """)
    db.execute("INSERT INTO runtime_agents VALUES (?,?)", (agent["id"], json.dumps(agent)))
    db.execute("INSERT INTO runtime_items VALUES (?,?)", ("agent:item", json.dumps({"text": ""})))
    with patch("time.time", return_value=12.0):
        runtime.analytics_event(db, agent, "turn/started", {"turnId": "turn"}, at=10.0)
        runtime.analytics_event(db, agent, "item/started", {
            "threadId": "thread", "turnId": "turn",
            "item": {"id": "item", "type": "agentMessage"},
        }, at=11.0)
    db.commit()

    stream = StreamBuffer(runtime)
    captures = []
    with patch("time.time", side_effect=[12.0, 13.0, 14.0]):
        stream.enqueue({"method": "item/agentMessage/delta", "params": {
            "threadId": "thread", "turnId": "turn", "itemId": "item", "delta": "hello",
        }}, "default", None)
        stream.enqueue({"method": "item/agentMessage/delta", "params": {
            "threadId": "thread", "turnId": "turn", "itemId": "item", "delta": " world\n",
        }}, "default", None)
        with stream.lock:
            stream.flush_locked(db, force=True, analytics_captures=captures)
    assert len(captures) == 1
    with patch("time.time", return_value=12.0):
        captures[0](db)
    db.commit()
    names = ("analytics_agents", "analytics_notifications", "analytics_turns", "analytics_items",
             "analytics_usage", "analytics_usage_roots", "analytics_meta", "analytics_limits")
    rows = {name: [dict(row) for row in db.execute(f"SELECT * FROM {name} ORDER BY rowid")]
            for name in names}
    print(json.dumps(rows, sort_keys=True))


if __name__ == "__main__":
    main()
