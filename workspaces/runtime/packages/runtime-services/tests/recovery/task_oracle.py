"""Task receipt cases execute the real receipt selector and result resolver."""
import json
import sqlite3
from pathlib import Path
from types import SimpleNamespace
from oracle import writer, task

agent=dict(id="agent",rootId="agent",epoch=0,status="completed",provider="codex",accountKey="default",threadId="thread",turnId=None,inFlight=False)
work=dict(id="agent:item",agent="agent",itemId="item",turnId="turn",type="dynamicToolCall",kind="tool",status="running",created=1.0)
receipt=dict(id="receipt",agent="agent",accountKey="default",threadId="thread",turnId="turn",callId="item",finished=2.0,stage="completed",outcome="applied",result={"success":True,"contentItems":[{"type":"inputText","text":"saved output"}]})
cases=[("exact",receipt,[])]
for field in ("id","agent","accountKey","threadId","turnId","callId"):
    cases.append(("foreign-"+field,{**receipt,field:"foreign"},[]))
for stage in ("queued","running","failed","completed"):
    for outcome in ("unknown","applied","not_applied"):
        for success in (True,False):
            cases.append((f"{stage}-{outcome}-{success}",{**receipt,"stage":stage,"outcome":outcome,"result":{"success":success}},[]))
cases.extend([("unfinished",{**receipt,"finished":None},[]),("ambiguous",receipt,["receipt","foreign"]),("alias",receipt,["receipt"])])
output=[]
statement=writer()
with sqlite3.connect(":memory:") as db:
    for table in ("agents","tasks","tool_requests"): db.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT)")
    for name,candidate,aliases in cases:
        values={"agents":agent,"tasks":work,"tool_requests":candidate}
        for table,record in values.items(): exec(statement,{"db":db,"table":table,"record":record,"json":json})
        stored={table:json.loads(db.execute(f"SELECT record FROM runtime_{table} WHERE id=?",(record["id"],)).fetchone()[0]) for table,record in values.items()}
        raw=json.dumps(stored["tool_requests"])
        receipts=task.ReceiptState(tuple(aliases),(("receipt",raw),))
        selected=task._terminal_receipt(receipts,stored["agents"],stored["tasks"])
        job=SimpleNamespace(completed_receipt=selected,task=stored["tasks"])
        proof=task._receipt_proof(SimpleNamespace(root=Path.cwd()),job)
        output.append(dict(name=name,agent=stored["agents"],task=stored["tasks"],receipt=stored["tool_requests"],aliases=aliases,selected=selected,proof=proof))
if __name__ == "__main__": print(json.dumps(output))
