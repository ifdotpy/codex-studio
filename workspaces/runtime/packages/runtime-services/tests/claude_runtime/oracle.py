"""Real Python Claude decisions, with no Runtime construction or provider access."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
import copy
import json
import sqlite3
from types import SimpleNamespace

ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(ROOT / "apps/server/src"))
# parents[6] is the repository, resolved without touching provider homes.
sys.path.insert(0, str(Path(__file__).resolve().parents[6] / "workspaces/runtime/apps/server/src"))
import codex_claude_input_recovery as oracle
from codex_native_errors import NativeRpcError


def case(name, modify=lambda a,t,s,e,c,r: None, turn=False, followup=None):
    db = sqlite3.connect(":memory:")
    db.row_factory = sqlite3.Row
    db.executescript("""
    CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,epoch INTEGER,kind TEXT,text TEXT,created REAL,status TEXT,turn_id TEXT,error TEXT);
    CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);
    CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT);
    CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT);
    CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,created REAL,record TEXT);
    CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT);
    CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT);
    """)
    a = dict(id="agent", rootId="agent",epoch=2,status="starting",provider="claude",accountKey="account",threadId="thread",autoWake=True,
             cwd="/fixture",role="lead",isLead=True,model="default",yoloMode=False,compactions=0)
    events = [dict(id=k,agent="agent",epoch=2,kind="user",text="original "+k,created=100.0+i,status="dispatching",turn_id=None,error=None,metadata={}) for i,k in enumerate(["one","two"])]
    for e in events:
        db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",tuple(e[k] for k in ("id","agent","epoch","kind","text","created","status","turn_id","error")))
        db.execute("INSERT INTO runtime_event_meta VALUES (?,?)",(e["id"],json.dumps(e["metadata"])))
    t = dict(id="attempt",epoch=2,accountKey="account",threadId="thread",connectionId="connection",nativeOperationId="turn:agent:one:attempt:attempt",events=["one","two"],submitted=True,supervisorIdentity=None,created=110.0)
    t["claudeInputRequest"] = oracle.capture_input(db,a,list(db.execute("SELECT * FROM runtime_events ORDER BY created")),
        dict(input=[dict(type="text",text="compiled original")],clientUserMessageId="one",threadId="thread",dynamicTools=[{"name":"fresh"}]),"compiled original")
    saved=copy.deepcopy(t)
    context=dict(accountKey="account",connectionId="connection",supervisorIdentity=None,closed=False,runtimeClosed=False,turnHasWork=False)
    rejection = dict(kind="rpc",code=-32000,data=dict(turnStartOutcome="not_applied"))
    if turn:
        a["turnId"]="turn"; t["turnId"]="turn"
        rejection=dict(kind="turn",turn=dict(id="turn",status="failed",startOutcome="not_applied",error=dict(data=dict(turnStartOutcome="not_applied"))))
    modify(a,t,saved,events,context,rejection)
    a["startAttempt"]=t
    for e in events:
        db.execute("UPDATE runtime_events SET agent=?,epoch=?,kind=?,text=?,created=?,status=?,turn_id=?,error=? WHERE id=?",tuple(e[k] for k in ("agent","epoch","kind","text","created","status","turn_id","error","id")))
        db.execute("UPDATE runtime_event_meta SET record=? WHERE id=?",(json.dumps(e["metadata"]),e["id"]))
    db.execute("INSERT INTO runtime_execution_runs VALUES ('run','agent')")
    db.execute("INSERT INTO runtime_execution_attempts VALUES ('attempt','run',?)",(json.dumps(saved),))
    if context["turnHasWork"]:
        db.execute("INSERT INTO runtime_items VALUES ('work','agent',110,?)",(json.dumps(dict(role="assistant",turnId="turn")),))
    server=SimpleNamespace(closed=context["closed"],supervisor_mode=context["supervisorIdentity"] is not None)
    if server.supervisor_mode:
        server.proc=SimpleNamespace(root=context["supervisorIdentity"]["stateDir"],handle=context["supervisorIdentity"]["handle"],generation=context["supervisorIdentity"]["generation"])
    rt=SimpleNamespace(closed=context["runtimeClosed"],connection_current=lambda key,conn:key==context["accountKey"] and conn==context["connectionId"],server_for=lambda *_:server,agent_connection=lambda _:context["connectionId"])
    inputs=copy.deepcopy(dict(agent=a,attempt=t,saved=saved,events=events,transport={k:v for k,v in context.items() if k not in {"runtimeClosed","turnHasWork"}},runtimeClosed=context["runtimeClosed"],turnHasWork=context["turnHasWork"],rejection=rejection))
    if name=="rpc_retry" and "--template" in sys.argv:
        path=sys.argv[sys.argv.index("--template")+1]
        db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT)")
        db.execute("INSERT INTO runtime_agents VALUES (?,?)",(a["id"],json.dumps(a)))
        db.commit()
        target=sqlite3.connect(path);db.backup(target);target.close()
    outcome=oracle.recover_rejected_start(rt,db,a,t,account_key=context["accountKey"],connection_id=context["connectionId"],
        **(dict(turn=rejection["turn"]) if rejection["kind"]=="turn" else dict(error=NativeRpcError(dict(code=rejection["code"],data=rejection["data"])))))
    expected=None
    if outcome:
        output_events=[]
        for e in events:
            row=dict(db.execute("SELECT * FROM runtime_events WHERE id=?",(e["id"],)).fetchone())
            row["metadata"]=json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?",(e["id"],)).fetchone()[0]);output_events.append(row)
        expected=dict(outcome=outcome,agent=a,attempt=json.loads(db.execute("SELECT record FROM runtime_execution_attempts").fetchone()[0]),events=output_events)
    if followup is not None:
        assert expected is not None
        mode,mutate=followup
        t=json.loads(db.execute("SELECT record FROM runtime_execution_attempts WHERE id='attempt'").fetchone()[0])
        item=dict(id="agent:one",role="user",text="compiled original")
        db.execute("INSERT INTO runtime_items VALUES ('agent:one','agent',100,?)",(json.dumps(item),))
        rt.put=lambda *args:None
        reservation=None
        later=False
        if mode=="input":a["startAttempt"]=dict(id="retry",epoch=2,accountKey="account",events=["one","two"],submitted=False,claudeRetryOf="attempt")
        if mode=="resume":a.update(nativeFailureHold=True,status="failed")
        if mode=="stop":
            a.update(epoch=3,status="paused")
            db.execute("UPDATE runtime_events SET status='cancelled'")
        context["laterUser"]=False
        mutate(a,t,item,context)
        if context["laterUser"]:
            db.execute("INSERT INTO runtime_events VALUES ('later','agent',2,'user','new',120,'pending',NULL,NULL)")
            db.execute("INSERT INTO runtime_event_meta VALUES ('later',?)",(json.dumps(dict(acceptedAt=120)),))
        db.execute("UPDATE runtime_execution_attempts SET record=? WHERE id='attempt'",(json.dumps(t),))
        db.execute("UPDATE runtime_items SET record=? WHERE id='agent:one'",(json.dumps(item),))
        rows=[]
        for key in t['events']:
            row=dict(db.execute("SELECT * FROM runtime_events WHERE id=?",(key,)).fetchone());row['metadata']=json.loads(db.execute("SELECT record FROM runtime_event_meta WHERE id=?",(key,)).fetchone()[0]);rows.append(row)
        inputs=dict(agent=copy.deepcopy(a),attempt=t,saved=t,events=rows,transport={k:context[k] for k in ('accountKey','connectionId','supervisorIdentity','closed')},runtimeClosed=context['runtimeClosed'],turnHasWork=False,transcript=item,laterUser=context['laterUser'])
        error=False
        try:
            if mode=="batch":result=oracle.retry_batch(rt,db,a);result=None if result is None else [r['id'] for r in result]
            elif mode=="input":result=oracle.retry_input(rt,db,a,rows)
            elif mode=="resume":oracle._resume_held_retry(rt,db,a);result=a
            else:result=dict(retired=oracle.retire_stopped_retry(rt,db,a),agent=a)
        except ValueError:error=True;result=None
        db.close()
        return dict(name=name,mode=mode,input=inputs,error=error,expected=result)
    db.close()
    return dict(name=name,input=inputs,expected=expected)


cases=[case("rpc_retry"),case("turn_retry",turn=True)]
for key,value in [("provider","codex"),("autoWake",False),("status","paused"),("deletedAt",1),("agentArchive",{"at":1,"epoch":2}),("nativeFailureHold",True),("accountTransferId","move"),("workspaceOperation","branch"),("epoch",3),("turnEpoch",4),("threadId","foreign"),("turnId","busy")]:
    cases.append(case("agent_"+key,lambda a,t,s,e,c,r,k=key,v=value:a.update({k:v})))
for key,value in [("submitted",False),("accountKey","foreign"),("connectionId","stale"),("nativeOperationId","foreign"),("events",["one","one"]),("id","foreign"),("action","compact"),("activeAtReservation",True)]:
    cases.append(case("attempt_"+key,lambda a,t,s,e,c,r,k=key,v=value:t.update({k:v})))
for key,value in [("closed",True),("runtimeClosed",True),("accountKey","foreign"),("connectionId","stale")]:
    cases.append(case("transport_"+key,lambda a,t,s,e,c,r,k=key,v=value:c.update({k:v})))
for key,value in [("text","changed"),("status","pending"),("turn_id","foreign"),("epoch",3),("agent","foreign")]:
    cases.append(case("event_"+key,lambda a,t,s,e,c,r,k=key,v=value:e[0].update({k:v})))
cases += [case("retry_used",lambda a,t,s,e,c,r:e[0]["metadata"].update(claudePreInputRetryUsed=True)),
    case("account_failure",lambda a,t,s,e,c,r:r["data"].update(claudePreparationFailure="account_validation")),
    case("input_submitted",lambda a,t,s,e,c,r:r["data"].update(inputSubmitted=True)),
    case("input_submitted_null",lambda a,t,s,e,c,r:r["data"].update(inputSubmitted=None)),
    case("unknown_outcome",lambda a,t,s,e,c,r:r["data"].update(turnStartOutcome="unknown")),
    case("wrong_code",lambda a,t,s,e,c,r:r.update(code=-1)),
    case("assets_changed",lambda a,t,s,e,c,r:e[0]["metadata"].update(assets=[{"id":"asset"}])),
    case("native_foreign",lambda a,t,s,e,c,r:e[0]["metadata"].update(native={"agent":"foreign"})),
    case("saved_changed",lambda a,t,s,e,c,r:s.update(connectionId="foreign")),
    case("source_changed",lambda a,t,s,e,c,r:a.update(yoloMode=True)),
    case("turn_has_work",lambda a,t,s,e,c,r:c.update(turnHasWork=True),turn=True),
    case("thread_block",lambda a,t,s,e,c,r:a.update(nativeThreadBlock={"threadId":"thread","error":{}}))]
followups=[]
for mode in ('batch','input','resume','stop'):
    for name,change in [('same',lambda a,t,i,c:None),('wrong_thread',lambda a,t,i,c:a.update(threadId='foreign')),('later_user',lambda a,t,i,c:c.update(laterUser=True)),('stale_connection',lambda a,t,i,c:c.update(connectionId='stale')),('changed_text',lambda a,t,i,c:i.update(text='changed')),('changed_source',lambda a,t,i,c:a.update(yoloMode=True))]:
        followups.append(case(mode+'_'+name,followup=(mode,change)))
if '--followups' in sys.argv:print(json.dumps(followups))
else:print(json.dumps(cases))
