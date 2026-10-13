"""Run Python's transcript submission, notification, sync and search writers."""

from __future__ import annotations

from contextlib import nullcontext
import json
import sqlite3
import sys
import time
from pathlib import Path

server_source = Path(sys.argv[1])
fixture_path = Path(sys.argv[2])
database_path = sys.argv[3]
sys.path.insert(0, str(server_source))

import codex_browser_recovery
import codex_execution
import codex_radio
import codex_runtime as codex_runtime_module
import codex_token_rate
from codex_runtime import Runtime
from codex_streaming import StreamBuffer


class TestStreamBuffer(StreamBuffer):
    """Use the real buffer and flush path without starting its wall-clock timer."""

    def _schedule_locked(self):
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
    CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL);
    CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL);
    CREATE TABLE sync_entities(collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(collection,id));
    CREATE TABLE sync_documents(seq INTEGER,scope TEXT,id TEXT,payload TEXT,deleted INTEGER);
    CREATE TABLE sync_versions(seq INTEGER,scope TEXT,hash TEXT,deleted INTEGER,updated REAL);
    CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize='unicode61');
    CREATE TABLE runtime_search_rows(id TEXT PRIMARY KEY,search_rowid INTEGER NOT NULL UNIQUE);
    CREATE TABLE runtime_search_indexed(id TEXT PRIMARY KEY);
    CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='',contentless_delete=1,tokenize='unicode61');
    CREATE TABLE runtime_search_next_meta(id TEXT PRIMARY KEY,agent TEXT NOT NULL,kind TEXT NOT NULL,search_rowid INTEGER NOT NULL UNIQUE);
    CREATE TABLE runtime_search_rollout(id INTEGER PRIMARY KEY,phase TEXT NOT NULL,cursor INTEGER NOT NULL DEFAULT 0,updated REAL NOT NULL);
    INSERT INTO runtime_search_rollout VALUES(1,'building',0,0);
    """
)

# This fixed clock makes the parsed Python and Rust rows comparable without
# changing any ordering or identity decision.
time.time = lambda: 1234.5
_original_gmtime = time.gmtime
time.gmtime = lambda *_args: _original_gmtime(1234.5)

runtime = Runtime.__new__(Runtime)
runtime.closed = False
runtime.notification_db = lambda: nullcontext(database)
runtime.connection_current = lambda *_args: True
runtime._voice_store = None
runtime._stream_buffer = TestStreamBuffer(runtime)
runtime.record_task = lambda *_args, **_kwargs: None
runtime.analytics_safe = lambda *_args, **_kwargs: None
runtime.touch_ui = lambda *_args, **_kwargs: None
runtime.sync_workspace_volatile = lambda *_args, **_kwargs: None
runtime.mark_agent_records_changed = lambda *_args, **_kwargs: None
runtime._stage_transcript_resource = lambda *_args, **_kwargs: None
runtime._stage_resource_change = lambda *_args, **_kwargs: None
runtime._stage_task_resources_for_root = lambda *_args, **_kwargs: None
runtime._stage_team_task_resources = lambda *_args, **_kwargs: None
runtime._dispatch_token_rate_observation = lambda *_args, **_kwargs: None
runtime.put = lambda db, table, record: Runtime._put(runtime, db, table, record, sync_rooms=False)
codex_execution.needs_record = lambda *_args, **_kwargs: False
codex_runtime_module.record_needs_dispatch = lambda *_args, **_kwargs: False
codex_runtime_module.workspace_agent_resource_changed = lambda *_args, **_kwargs: False
codex_token_rate.token_rates = lambda *_args, **_kwargs: TokenRates()
codex_browser_recovery.observe = lambda *_args, **_kwargs: None
codex_radio.observe_item = lambda *_args, **_kwargs: None

agent = {
    "id": "agent",
    "rootId": "agent",
    "epoch": 1,
    "status": "running",
    "threadId": "thread",
    "accountKey": "default",
    "turnId": "replay-turn-1",
    "inFlight": True,
    "events": 0,
    "activeTools": [],
    "autoWake": False,
    "prompt": "A short parity fixture.",
}
Runtime._put(runtime, database, "agents", agent, sync_rooms=False)
for event_id, kind, text_value, created in [
    ("user-event", "user", "Please answer the replay.", 100.0),
    ("followup-event", "followup", "Add a short summary.", 101.0),
]:
    database.execute(
        "INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)",
        (event_id, "agent", kind, text_value, "dispatching", created, 1, "replay-turn-1", None),
    )
    event_assets = [{"id": "asset-1", "name": "notes.txt"}] if event_id == "user-event" else []
    database.execute(
        "INSERT INTO runtime_event_meta VALUES(?,?)",
        (event_id, json.dumps({"assets": event_assets, "modelEventProjection": 1})),
    )

input_rows = [
    {
        "id": event_id,
        "kind": kind,
        "text": text_value,
        "created": created,
        "assets": [{"id": "asset-1", "name": "notes.txt"}] if event_id == "user-event" else [],
    }
    for event_id, kind, text_value, created in [
        ("user-event", "user", "Please answer the replay.", 100.0),
        ("followup-event", "followup", "Add a short summary.", 101.0),
    ]
]
submitted_text = Runtime.model_event_text(runtime, input_rows)
database.commit()
seed = sqlite3.connect(database_path + ".seed")
database.backup(seed)
seed.close()
Runtime.item(
    runtime,
    database,
    "agent",
    input_rows[0]["id"],
    "user",
    submitted_text,
    inputs=input_rows,
    assets=input_rows[0]["assets"],
)


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


fixture = json.loads(fixture_path.read_text(encoding="utf-8"))
notification_results = []
for event in fixture["events"]:
    if event["method"] in {"item/agentMessage/delta", "item/completed"}:
        normalized = replace_fixture_ids(event)
        Runtime.notification(runtime, normalized, "default", "fixture-connection")
        notification_results.append({"method": normalized["method"], "status": "written"})

ordering_events = [
    {
        "method": "item/agentMessage/delta",
        "params": {"threadId": "thread", "turnId": "replay-turn-1", "itemId": "snapshot-answer", "delta": "before"},
    },
    {
        "method": "item/started",
        "params": {"threadId": "thread", "turnId": "replay-turn-1", "item": {"id": "snapshot-answer", "type": "agentMessage", "text": "snapshot"}},
    },
    {
        "method": "item/completed",
        "params": {"threadId": "thread", "turnId": "replay-turn-1", "item": {"id": "snapshot-answer", "type": "agentMessage", "text": ""}},
    },
    {
        "method": "item/completed",
        "params": {"threadId": "thread", "turnId": "replay-turn-1", "item": {"id": "late-answer", "type": "agentMessage", "text": "final"}},
    },
    {
        "method": "item/agentMessage/delta",
        "params": {"threadId": "thread", "turnId": "replay-turn-1", "itemId": "late-answer", "delta": "late"},
    },
]
for event in ordering_events:
    Runtime.notification(runtime, event, "default", "fixture-connection")
    notification_results.append({"method": event["method"], "status": "written"})
runtime._stream_buffer.flush_locked(
    database,
    account="default",
    thread_id="thread",
    turn_id="replay-turn-1",
    force=True,
)

database.commit()
items = {}
for row in database.execute("SELECT id,record FROM runtime_items WHERE agent='agent' ORDER BY id"):
    items[row["id"]] = json.loads(row["record"])
agent_record = json.loads(database.execute("SELECT record FROM runtime_agents WHERE id='agent'").fetchone()[0])
sync_row = database.execute(
    "SELECT seq,hash,payload FROM sync_entities WHERE collection='agent' AND id='agent'"
).fetchone()
legacy_search = [
    {"id": row[0], "agent": row[1], "kind": row[2], "body": row[3]}
    for row in database.execute(
        "SELECT s.id,s.agent,s.kind,s.body FROM runtime_search_rows r "
        "JOIN runtime_search s ON s.rowid=r.search_rowid ORDER BY s.id"
    )
]
next_search = [
    {"id": row[0], "agent": row[1], "kind": row[2], "searchRowId": row[3]}
    for row in database.execute(
        "SELECT id,agent,kind,search_rowid FROM runtime_search_next_meta ORDER BY id"
    )
]
event_meta = {
    row[0]: json.loads(row[1])
    for row in database.execute("SELECT id,record FROM runtime_event_meta ORDER BY id")
}
expected = {
    "items": items,
    "agent": agent_record,
    "sync": {
        "seq": sync_row[0],
        "hash": sync_row[1],
        "payload": json.loads(sync_row[2]),
    },
    "search": {"legacy": legacy_search, "next": next_search},
    "eventMeta": event_meta,
    "notificationResults": notification_results,
    "prepared": {
        "firstEventId": input_rows[0]["id"],
        "submittedAt": 1234.5,
        "createdAt": 1234.5,
        "text": submitted_text,
        "assets": input_rows[0]["assets"],
        "inputs": [
            {"id": row["id"], "at": row["created"], "kind": row["kind"], "text": row["text"], "assets": row["assets"]}
            for row in input_rows
        ],
    },
    "orderingEvents": ordering_events,
}
print(json.dumps(expected, ensure_ascii=False, sort_keys=True))
