"""Execute real Python recovery with guarded, Python-written disposable state."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "recovery"))
from oracle import writer, connection
import copy
import json
import sqlite3
import threading
from contextlib import nullcontext
from types import SimpleNamespace
from unittest.mock import patch
from codex_runtime import Runtime as ProductionRuntime

statement = writer()

class Runtime:
    closed = False
    lock = threading.RLock()
    loaded = set()
    item = ProductionRuntime.item
    def __init__(self, db, server): self.connection = db; self.server = server
    def db(self): return nullcontext(self.connection)
    def agent(self, key, db=None): return json.loads(self.connection.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])
    def put(self, db, table, record, **kwargs): exec(statement, dict(db=db, table=table, record=record, json=json))
    def connection_current(self, *args): return True
    def server_for(self, *args): return self.server
    def agent_connection(self, agent): return "connection"
    def index_item(self, *args): pass
    def touch_ui(self, *args): pass
    def child_stopped_event(self, *args): pass
    def worker_continuation_pending(self, agent): return False


def corpus():
    cases = []
    base = dict(id="agent", rootId="agent", epoch=0, status="interrupted", provider="codex", accountKey="default", threadId="thread", turnId="turn", inFlight=False, autoWake=False, error="Codex disconnected. Review the transcript before resuming.")
    for outcome in ("completed", "failed", "interrupted"):
        for text in ("", "final result", "x" * 20001):
            for phase in (None, "commentary", "final_answer"):
                with sqlite3.connect(":memory:") as db:
                    db.row_factory = sqlite3.Row
                    for table in ("agents", "tasks", "monitors", "tool_requests", "rooms"):
                        db.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT)")
                    db.execute("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL)")
                    db.execute("CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT)")
                    db.execute("CREATE TABLE runtime_events(id TEXT,agent TEXT,epoch INTEGER,status TEXT)")
                    db.execute("CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY)")
                    agent = copy.deepcopy(base)
                    exec(statement, dict(db=db, table="agents", record=agent, json=json))
                    before = json.loads(db.execute("SELECT record FROM runtime_agents").fetchone()[0])
                    turn = dict(id="turn", status=outcome, items=[dict(id="answer",type="agentMessage",text=text,phase=phase)])
                    server = object(); runtime = Runtime(db, server)
                    with patch("time.time", return_value=1000.0):
                        result = connection.apply_result(runtime, before, "connection", server, turn)
                    cases.append(dict(name=f"{outcome}-{len(text)}-{phase}",before=before,turn=turn,result=result,
                        after=runtime.agent("agent"),items=[json.loads(r[0]) for r in db.execute("SELECT record FROM runtime_items ORDER BY id")],
                        fulltext=[list(r) for r in db.execute("SELECT * FROM runtime_item_fulltext ORDER BY id")],
                        completed=[r[0] for r in db.execute("SELECT id FROM runtime_completed_turns")]))
    # Native and local unresolved receipts are checked with the real predicate.
    unresolved = []
    for table in ("tasks", "monitors", "tool_requests"):
        for scope in (None, "turn", "foreign"):
            for state in ("running", "completed", "lost", "unknown"):
                with sqlite3.connect(":memory:") as db:
                    db.row_factory = sqlite3.Row
                    db.execute("CREATE TABLE runtime_events(agent TEXT,epoch INTEGER,status TEXT)")
                    for name in ("tasks", "monitors", "tool_requests"):
                        db.execute(f"CREATE TABLE runtime_{name}(id TEXT PRIMARY KEY,record TEXT)")
                    row = dict(id="operation",agent="agent",turnId=scope,status=state,stage=state,outcome=state)
                    exec(statement,dict(db=db,table=table,record=row,json=json))
                    unresolved.append(dict(table=table,record=row,expected=connection.unresolved_turn_receipts(db,base,"turn")))
    return dict(connection=cases,unresolved=unresolved)

if __name__ == "__main__": print(json.dumps(corpus()))
