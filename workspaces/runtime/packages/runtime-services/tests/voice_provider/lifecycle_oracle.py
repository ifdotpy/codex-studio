"""Run Python NativeVoice lifecycle entry points against an RPC-boundary fake."""

import concurrent.futures
import json
import pathlib
import sqlite3
import sys
import threading
from unittest.mock import patch

support = pathlib.Path(__file__).resolve().parents[1] / "turn_lifecycle"
sys.path.insert(0, str(support))
from oracle_guard import install_guard

install_guard()
server_src = pathlib.Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(server_src))
import codex_native_voice
from codex_voice import VoiceStore


class Server:
    def __init__(self):
        self.calls = []

    def call(self, method, _params, **_kwargs):
        if method == "account/read":
            return {"account": {"type": "chatgpt"}}
        return {}

    def submit(self, method, params):
        self.calls.append((method, params))
        return len(self.calls)

    def on_result(self, request_id, callback):
        future = concurrent.futures.Future()
        future.set_result({"id": request_id})
        callback(future)


class Runtime:
    def __init__(self, root):
        self.root = pathlib.Path(root)
        self.runtime_root = self.root
        self.lock = threading.RLock()
        self.server = Server()
        self.connection_ids = {"account": "receipt-matrix-connection"}

    def db(self):
        connection = sqlite3.connect(self.root / "canvas.sqlite3")
        connection.row_factory = sqlite3.Row
        return connection

    def agent(self, agent, db=None):
        if db is None:
            with self.db() as connection:
                row = connection.execute("SELECT record FROM runtime_agents WHERE id=?", (agent,)).fetchone()
        else:
            row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent,)).fetchone()
        return json.loads(row[0]) if row else None

    def connect(self, _account):
        return self.server

    def prepare(self, _actor):
        return None

    def connection_current(self, _account, _connection):
        return True

    def put(self, db, collection, record):
        assert collection == "agents"
        db.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(record), record["id"]))


root = pathlib.Path(sys.argv[1])
runtime = Runtime(root)
with runtime.db() as connection:
    if runtime.agent("agent", connection) is None:
        connection.execute("INSERT INTO runtime_agents(id,record) VALUES('agent',?)", (json.dumps({
            "id": "agent", "rootId": "agent", "epoch": 0, "status": "idle", "isLead": True,
            "threadId": "thread", "accountKey": "account",
        }),))
voice = VoiceStore.__new__(VoiceStore)
voice.__init__(runtime)
voice._publish_voice = lambda _agent: None
codex_native_voice.time.time = lambda: 100.0

with patch.object(threading.Thread, "start", lambda thread: thread.run()):
    voice.start("agent", "oracle-session", "v=0\r\noffer")

voice.native_notification({"method": "thread/realtime/sdp", "params": {
    "threadId": "thread", "realtimeSessionId": "oracle-session", "sdp": "answer",
}}, "account", "receipt-matrix-connection")

with runtime.db() as connection:
    connection.execute("INSERT INTO voice_records(id,agent,session,kind,text,item_id,previous_item_id,payload,created) VALUES('oracle-speech','agent','oracle-session','orchestrator','hello','item','', '{}',100)")
voice.speech("agent", "oracle-speech", "oracle-session")
voice.end("agent", "oracle-session")

with runtime.db() as connection:
    tables = {}
    for table, order in (("voice_sessions", "id"), ("voice_records", "seq"),
                         ("voice_speech_requests", "id"), ("runtime_events", "created,id"),
                         ("sync_entities", "collection,id"), ("sync_documents", "scope,id,seq")):
        tables[table] = [dict(row) for row in connection.execute(f"SELECT * FROM {table} ORDER BY {order}")]
    agent = runtime.agent("agent", connection)
print(json.dumps({"tables": tables, "provider_calls": runtime.server.calls, "agent": agent}, ensure_ascii=False))
