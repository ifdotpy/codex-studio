"""Run the real Python voice read and approval-speech functions on SQLite state."""

import json
import pathlib
import sqlite3
import sys
import threading

support = pathlib.Path(__file__).resolve().parents[1] / "turn_lifecycle"
sys.path.insert(0, str(support))
from oracle_guard import install_guard

install_guard()
server_src = pathlib.Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(server_src))
from codex_voice import VoiceStore


class Runtime:
    def __init__(self, root):
        self.root = pathlib.Path(root)
        self.lock = threading.RLock()

    def db(self):
        connection = sqlite3.connect(self.root / "canvas.sqlite3")
        connection.row_factory = sqlite3.Row
        return connection

    def agent(self, agent, db=None):
        if db is not None:
            row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent,)).fetchone()
            return json.loads(row[0]) if row else None
        with self.db() as connection:
            row = connection.execute("SELECT record FROM runtime_agents WHERE id=?", (agent,)).fetchone()
            return json.loads(row[0]) if row else None


root = pathlib.Path(sys.argv[1])
with sqlite3.connect(root / "canvas.sqlite3") as connection:
    connection.executescript("""
        CREATE TABLE IF NOT EXISTS voice_sessions(
          id TEXT PRIMARY KEY, agent TEXT NOT NULL, created REAL NOT NULL, ended REAL,
          sdp TEXT, answer TEXT, native_thread TEXT, account_key TEXT, connection_id TEXT,
          state TEXT, error TEXT);
        CREATE TABLE IF NOT EXISTS voice_records(
          seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, agent TEXT NOT NULL,
          session TEXT NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL, item_id TEXT NOT NULL,
          previous_item_id TEXT NOT NULL, payload TEXT NOT NULL, created REAL NOT NULL);
        CREATE TABLE IF NOT EXISTS voice_approvals(
          id TEXT PRIMARY KEY, agent TEXT NOT NULL, speech TEXT NOT NULL,
          transcript TEXT NOT NULL, status TEXT NOT NULL, result TEXT);
        CREATE TABLE IF NOT EXISTS voice_deliveries(
          id TEXT PRIMARY KEY, agent TEXT NOT NULL, records TEXT NOT NULL,
          text TEXT NOT NULL, edited_text TEXT);
    """)

voice = VoiceStore.__new__(VoiceStore)
voice.runtime = Runtime(root)
voice._publish_voice = lambda _agent: None
agent, request_id = sys.argv[2], sys.argv[3]
approval = voice.approval_speech(agent, request_id)
approvals = voice.approvals(agent)
records = voice.records(agent)
with voice.runtime.db() as connection:
    stored = [dict(row) for row in connection.execute(
        "SELECT * FROM voice_records WHERE agent=? ORDER BY seq", (agent,)
    )]
    events = [dict(row) for row in connection.execute(
        "SELECT * FROM runtime_events ORDER BY created,id"
    )]
    entities = [dict(row) for row in connection.execute(
        "SELECT * FROM sync_entities ORDER BY collection,id"
    )]
    documents = [dict(row) for row in connection.execute(
        "SELECT * FROM sync_documents ORDER BY scope,id,seq"
    )]
print(json.dumps({
    "approval": approval,
    "approvals": approvals,
    "records": records,
    "rows": stored,
    "events": events,
    "sync_entities": entities,
    "sync_documents": documents,
}, ensure_ascii=False))
