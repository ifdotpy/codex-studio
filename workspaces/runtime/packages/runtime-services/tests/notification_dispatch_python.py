"""Run the real Python transcript writers over the recorded provider replay."""

from __future__ import annotations

from contextlib import nullcontext
import json
import sqlite3
import sys
from pathlib import Path

server_source = Path(sys.argv[1])
fixture_path = Path(sys.argv[2])
database_path = sys.argv[3] if len(sys.argv) > 3 else ":memory:"
sys.path.insert(0, str(server_source))

import codex_browser_recovery
import codex_radio
import codex_token_rate
from codex_runtime import Runtime


class StreamBuffer:
    def enqueue(self, *_args, **_kwargs):
        return False

    def flush_locked(self, *_args, **_kwargs):
        return None


class TokenRates:
    def stream(self, *_args, **_kwargs):
        return None


database = sqlite3.connect(database_path)
database.row_factory = sqlite3.Row
database.executescript(
    """
    CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
    CREATE TABLE runtime_events(
        id TEXT PRIMARY KEY,agent TEXT NOT NULL,kind TEXT NOT NULL,text TEXT NOT NULL,
        status TEXT NOT NULL,created REAL NOT NULL,epoch INTEGER NOT NULL,turn_id TEXT,error TEXT
    );
    CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
    CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT NOT NULL,record TEXT NOT NULL,created REAL NOT NULL);
    CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT NOT NULL);
    INSERT INTO runtime_agents VALUES(
        'agent',
        '{"id":"agent","rootId":"agent","epoch":1,"status":"running","threadId":"thread","turnId":"replay-turn-1","inFlight":true,"events":0,"activeTools":[],"autoWake":false}'
    );
    INSERT INTO runtime_events VALUES(
        'user-event','agent','user','Reply with the fixture result.','dispatching',10,1,'replay-turn-1',NULL
    );
    INSERT INTO runtime_event_meta VALUES('user-event','{}');
    """
)

runtime = Runtime.__new__(Runtime)
runtime.notification_db = lambda: nullcontext(database)
runtime.connection_current = lambda *_args: True
runtime._voice_store = None
runtime._stream_buffer = StreamBuffer()
runtime.agent = lambda agent_id, db: json.loads(
    db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent_id,)).fetchone()[0]
)
runtime.record_task = lambda *_args, **_kwargs: None
runtime.analytics_safe = lambda *_args, **_kwargs: None
runtime.index_item = lambda *_args, **_kwargs: None
runtime.delete_search_item = lambda *_args, **_kwargs: None
runtime.touch_ui = lambda *_args, **_kwargs: None
runtime.put = lambda db, table, record: db.execute(
    "UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(record), record["id"])
)
runtime._dispatch_token_rate_observation = lambda *_args, **_kwargs: None
codex_token_rate.token_rates = lambda *_args, **_kwargs: TokenRates()
codex_browser_recovery.observe = lambda *_args, **_kwargs: None
codex_radio.observe_item = lambda *_args, **_kwargs: None

input_event = database.execute(
    "SELECT id,kind,text,created FROM runtime_events WHERE id='user-event'"
).fetchone()
Runtime.item(
    runtime,
    database,
    "agent",
    "user-event",
    "user",
    input_event["text"],
    inputs=[
        {
            "id": input_event["id"],
            "created": input_event["created"],
            "kind": input_event["kind"],
            "text": input_event["text"],
            "assets": [],
        }
    ],
    assets=[],
)

fixture = json.loads(fixture_path.read_text(encoding="utf-8"))


def replace_fixture_ids(value):
    if isinstance(value, dict):
        return {key: replace_fixture_ids(item) for key, item in value.items()}
    if isinstance(value, list):
        return [replace_fixture_ids(item) for item in value]
    if value == "$threadId":
        return "thread"
    if value == "$turnId":
        return "replay-turn-1"
    return value


for event in fixture["events"]:
    if event["method"] in {"item/agentMessage/delta", "item/completed"}:
        Runtime.notification(runtime, replace_fixture_ids(event))

database.commit()
rows = database.execute(
    "SELECT id,record FROM runtime_items WHERE agent='agent' ORDER BY id"
).fetchall()
result = {}
for row in rows:
    record = json.loads(row["record"])
    record.pop("at", None)
    result[row["id"]] = record
print(json.dumps(result, sort_keys=True))
