"""Real Python predicates over rows written by Runtime.put; no provider starts."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)

import ast
import hashlib
import json
import sqlite3
SERVER = Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(SERVER))
import codex_connection_recovery as connection
import codex_turn_recovery as turn
import codex_task_recovery as task

FUNCTIONS = {
    "orphanBusy": turn.orphan_busy,
    "unconfirmedStart": turn.unconfirmed_start,
    "preparation": connection.preparation_eligible,
    "disconnectedPreparation": connection.disconnected_preparation_eligible,
    "restartTurn": connection.restart_turn_pending,
    "queuedRestart": connection.queued_restart_eligible,
    "queuedActiveWait": connection.queued_active_wait_eligible,
    "transportTurn": connection.transport_turn_eligible,
    "connection": connection.eligible,
    "taskOwner": task._eligible_owner,
}


def writer():
    # Isolate the real storage statement from unrelated renderer publication.
    # Any change to the enclosing production function invalidates this fixture.
    source = (SERVER / "codex_runtime.py").read_text()
    fn = next(node for node in ast.walk(ast.parse(source))
              if isinstance(node, ast.FunctionDef) and node.name == "_put")
    assert hashlib.sha256(ast.get_source_segment(source, fn).encode()).hexdigest() == "186fe83791ed77a1f8878e7be911e7d0adeaa49553a064177a62ea78e4c07d8e", "Runtime._put changed; re-audit the extracted writer"
    statements = [node for node in fn.body if isinstance(node, ast.Expr)
                  and "INSERT INTO runtime_" in ast.get_source_segment(source, node)]
    assert len(statements) == 1
    return compile(ast.Module(body=statements, type_ignores=[]), "Runtime._put", "exec")


def main():
    cases = json.loads(Path(__file__).with_name("cases.json").read_text())
    output = []
    statement = writer()
    with sqlite3.connect(":memory:") as db:
        db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
        for case in cases:
            agent = dict(case["agent"])
            for key in case.get("absent", []): agent.pop(key, None)
            exec(statement, {"db":db,"table":"agents","record":agent,"json":json})
            raw = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent["id"],)).fetchone()[0]
            stored = json.loads(raw)
            output.append({"name":case["name"],"agent":stored,"expected":{name:bool(fn(stored)) for name,fn in FUNCTIONS.items()}})
    operations = []
    for kind in ("commandExecution", "dynamicToolCall", "mcpToolCall", "fileChange", "computerToolCall", "agentMessage"):
        for status in ("completed", "failed", "declined", "inProgress", None):
            for exit_code in (0, 1, None, True):
                native = {"id":"turn", "status":"completed", "items":[{"id":"item", "type":kind, "status":status, "exitCode":exit_code}]}
                operations.append({"turn":native, "expected":connection.native_operations_settled(native)})
    completion = []
    with sqlite3.connect(":memory:") as db:
        db.row_factory = sqlite3.Row
        db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
        db.execute("CREATE TABLE runtime_events(agent TEXT,epoch INTEGER,status TEXT)")
        for table in ("tasks", "monitors", "tool_requests"):
            db.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT)")
        base = dict(id="agent", rootId="agent", epoch=0, status="interrupted", accountKey="default",threadId="thread",turnId="turn",turnEpoch=0,
                    disconnectRecovery=dict(epoch=0,accountKey="default",threadId="thread",turnId="turn",autoWake=True))
        variants = [("exact", base)]
        for field,value in (("epoch",1),("accountKey","foreign"),("threadId","foreign"),("turnId","foreign"),("turnEpoch",1),("nativeFailureHold",True)):
            variants.append((field,{**base,field:value}))
        variants.append(("no-auto-wake",{**base,"disconnectRecovery":{**base["disconnectRecovery"],"autoWake":False}}))
        for name, agent in variants:
            for status in ("completed","failed","interrupted"):
                for uncertain in (False,True):
                    db.execute("DELETE FROM runtime_events")
                    if uncertain: db.execute("INSERT INTO runtime_events VALUES('agent',0,'uncertain')")
                    exec(statement,{"db":db,"table":"agents","record":agent,"json":json})
                    stored=json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='agent'").fetchone()[0])
                    native=dict(id="turn",status=status,items=[])
                    unresolved=connection.unresolved_turn_receipts(db,stored,"turn")
                    completion.append(dict(name=name,agent=stored,turn=native,unresolved=unresolved,expected=connection.can_deliver_completion(db,stored,native)))
    print(json.dumps({"eligibility":output,"operations":operations,"completion":completion}))

if __name__ == "__main__": main()
