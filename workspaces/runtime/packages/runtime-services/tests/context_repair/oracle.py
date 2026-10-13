"""Synthetic historical receipt corpus; no Runtime, provider, or real history.

The recurring decision block is extracted verbatim and pinned by the digest of
its enclosing Python function. Historical capture and acceptance call the real
Python functions on disposable SQLite records.
"""
import ast
import copy
import hashlib
import json
import os
import re
from pathlib import Path
import sqlite3
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
import codex_historical_input_receipts as historical


def base():
    agent = dict(id="agent", rootId="agent", epoch=1, accountKey="default", threadId="thread", provider="codex", model="gpt-fixture", yoloMode=True, status="waiting", autoWake=True, inFlight=False,
                 startAttempt=dict(id="attempt", events=["event"], submitted=True),
                 contextRepairWait=dict(error="Context repair waits for a confirmed input receipt: event", source=dict(id="agent",epoch=1,accountKey="default",threadId="thread",attemptId=None)))
    event = dict(id="event",agent="agent",epoch=1,kind="user",text="hello",status="uncertain",created=1.0,turn_id=None,error="unknown",metadata=None)
    turn = dict(id="native-turn",startOutcome="accepted",clientUserMessageId="event",items=[])
    return dict(agent=agent,events=[event],turns=[turn],state="idle",operations=[],batches=[])


def reducer():
    path = ROOT / "workspaces/runtime/apps/server/src/codex_context_repair.py"
    source = path.read_text()
    module = ast.parse(source)
    original = next(node for node in module.body if isinstance(node,ast.FunctionDef) and node.name == "recover_unconfirmed_inputs")
    digest = hashlib.sha256(ast.get_source_segment(source,original).encode()).hexdigest()
    expected = "c54e97f909052fa767538058656dbd58678c61e1bc4702663c62b3470848e714"
    assert digest == expected, ("Python recovery oracle changed",digest)
    start = next(i for i,node in enumerate(original.body) if isinstance(node,ast.Assign) and any(isinstance(target,ast.Name) and target.id == "found" for target in node.targets))
    end = next(i for i in range(start,len(original.body)) if isinstance(original.body[i],ast.With))
    body = original.body[start:end]
    code = compile(ast.fix_missing_locations(ast.Module(body=body,type_ignores=[])),str(path),"exec")
    def run(case):
        env = dict(turns=case["turns"],state=case["state"],events=case["events"],historical_wait=False,batch_proofs={},ids=[event["id"] for event in case["events"]],current_attempt=case["agent"].get("startAttempt"),marker_attempt_snapshot=case["agent"].get("restartRecovery",{}).get("startAttempt"))
        exec(code,env)
        return env["decisions"]
    return run


def recurring():
    run = reducer()
    cases=[]
    for outcome in (None,"accepted","preparing","not_applied"):
        for state in ("idle","notLoaded","active"):
            for submitted in (True,False,None):
                case=base(); case["name"]=f"{outcome}-{state}-{submitted}"
                case["turns"][0]["startOutcome"]=outcome;case["state"]=state
                case["agent"]["startAttempt"]["submitted"]=submitted
                case["agent"]["contextRepairWait"]["source"]["attemptId"] = None if submitted else "attempt"
                case["expected"]=run(case);cases.append(case)
    for name,change in (
        ("missing-history",lambda c:c.update(turns=[])),
        ("missing-history-unsent",lambda c:(c.update(turns=[]),c["agent"]["startAttempt"].update(submitted=False),c["agent"]["contextRepairWait"]["source"].update(attemptId="attempt"))),
        ("other-identified-submitted",lambda c:c["turns"][0].update(clientUserMessageId="other")),
        ("other-identified-unsent",lambda c:(c["turns"][0].update(clientUserMessageId="other"),c["agent"]["startAttempt"].update(submitted=False),c["agent"]["contextRepairWait"]["source"].update(attemptId="attempt"))),
        ("unidentified-input",lambda c:(c["turns"][0].update(startOutcome="not_applied"),c["turns"].append(dict(id="other",items=[dict(type="userMessage",content=[])])))),
        ("item-identity",lambda c:c["turns"][0].update(clientUserMessageId=None,items=[dict(type="userMessage",clientId="event")])),
        ("missing-turn-id",lambda c:c["turns"][0].update(id=None)),
        ("absent-start-outcome",lambda c:c["turns"][0].pop("startOutcome")),
        ("failed-turn-after-acceptance",lambda c:c["turns"][0].update(status="failed")),
    ):
        case=base();case["name"]=name;change(case);case["expected"]=run(case);cases.append(case)
    return cases


def outcome_contract():
    # Enumerate assignments in the actual writers, not consumer allow-lists.
    projection = ast.parse((ROOT / "workspaces/runtime/apps/server/src/codex_native_input_projection.py").read_text())
    python_values = {value.value for node in ast.walk(projection) if isinstance(node, ast.Dict)
                     for key, value in zip(node.keys, node.values)
                     if isinstance(key, ast.Constant) and key.value == "startOutcome"
                     and isinstance(value, ast.Constant)}
    bridge = (ROOT / "workspaces/providers/apps/claude-bridge/bridge.mjs").read_text()
    bridge_values = set(re.findall(r'(?:startOutcome\s*:\s*|\.startOutcome\s*=\s*)["\']([^"\']+)["\']', bridge))
    assert python_values == {"accepted"}, python_values
    assert bridge_values == {"accepted", "preparing", "not_applied"}, bridge_values
    run = reducer()
    malformed = []
    for outcome in ("failed", "completed", "cancelled", "interrupted", "unknown", ""):
        case = base()
        case["name"] = "unwritten-start-outcome-" + outcome
        case["turns"][0]["startOutcome"] = outcome
        case["expected"] = run(case)
        malformed.append(case)
    return dict(written=sorted(python_values | bridge_values), malformed=malformed)


def batch_cases():
    cases=[]
    for name,text,alter in [("exact","hello",None),("unicode","zażółć 東京 🦀",None),("content-changed","hello","text"),("digest-changed","hello","digest"),("duplicate-operation","hello","duplicate"),("foreign-account","hello","account"),("preparing","hello","preparing"),("missing-operation","hello","missing"),("element-annotation","hello","elements"),("request-id-changed","hello","request")]:
        agent=base()["agent"]
        primary=dict(id="primary",agent="agent",epoch=1,kind="user",text=text,status="delivered",created=1.0,turn_id="native-turn",error=None)
        secondary=dict(id="event",agent="agent",epoch=1,kind="work_review",text="secondary",status="uncertain",created=2.0,turn_id=None,error="unknown")
        inputs=[dict(id=row["id"],text=row["text"],at=row["created"],kind=row["kind"]) for row in [primary,secondary]]
        item=dict(id="agent:primary",role="user",text=text+"\nsecondary",inputs=inputs,assets=[])
        members=[]
        db=sqlite3.connect(":memory:");db.row_factory=sqlite3.Row
        db.executescript("CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT); CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT); CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,epoch INTEGER,kind TEXT,text TEXT,status TEXT,created REAL,turn_id TEXT,error TEXT); CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);")
        db.execute("INSERT INTO runtime_items VALUES(?,?,?)",(item["id"],"agent",json.dumps(item)))
        for row in [primary,secondary]:
            meta=dict(transcriptItemId="agent:primary",modelEventProjection=1,contextManifest=dict(epoch=["thread",0],sequence=1,versions={}))
            if row["id"] == "primary":meta["acceptedAt"]=1.5
            db.execute("INSERT INTO runtime_events VALUES(:id,:agent,:epoch,:kind,:text,:status,:created,:turn_id,:error)",row)
            db.execute("INSERT INTO runtime_event_meta VALUES(?,?)",(row["id"],json.dumps(meta)))
            members.append(dict(row,metadata=meta))
        event=dict(secondary,metadata=json.dumps(members[1]["metadata"]))
        captures=historical.capture_batches(db,agent,[event]);assert len(captures)==1
        capture=captures[0]
        turns=[dict(id="native-turn",startOutcome="accepted",clientUserMessageId="primary",items=[dict(id="item",type="userMessage",clientId="primary",content=copy.deepcopy(capture["params"]["input"]))])]
        operations=[dict(handle="account:default",operation_id="turn:agent:primary:attempt:00000000-0000-4000-8000-000000000001",digest=capture["digest"],native_id=17,accepted=2.0)]
        handle="account:default"
        if alter=="text":turns[0]["items"][0]["content"][0]["text"]+="different"
        if alter=="digest":operations[0]["digest"]="bad"
        if alter=="duplicate":operations.append(dict(operations[0],operation_id="turn:agent:primary:attempt:00000000-0000-4000-8000-000000000002"))
        if alter=="account":handle="account:other"
        if alter=="preparing":turns[0]["startOutcome"]="preparing"
        if alter=="missing":operations=[]
        if alter=="elements":turns[0]["items"][0]["content"][0]["text_elements"]=[{}]
        if alter=="request":operations[0]["operation_id"]="turn:agent:foreign:attempt:00000000-0000-4000-8000-000000000001"
        with tempfile.TemporaryDirectory() as root:
            with sqlite3.connect(Path(root)/"supervisor.sqlite3") as journal:
                journal.execute("PRAGMA synchronous=OFF")
                journal.execute("CREATE TABLE operations(handle TEXT,operation_id TEXT,digest TEXT,native_id INTEGER,accepted REAL)")
                journal.executemany("INSERT INTO operations VALUES(:handle,:operation_id,:digest,:native_id,:accepted)",operations)
            expected=historical.accepted_batches(dict(stateDir=root,handle=handle),captures,turns)
        cases.append(dict(name=name,agent=agent,source=dict(item=item,row=dict(db.execute("SELECT * FROM runtime_items WHERE id=?",(item["id"],)).fetchone()),fulltext=None,members=members),turns=turns,operations=operations,handle=handle,expected=expected,capture=capture,params=capture["params"],digest=capture["digest"]))
        db.close()
    return cases

if __name__ == "__main__":
    print(json.dumps(dict(recurring=recurring(),batches=batch_cases(),outcomes=outcome_contract()),ensure_ascii=False))

# Optional adapter fixture: one Python-written template, cloned by each case.
if len(sys.argv) == 2:
    root=Path(sys.argv[1]);root.mkdir(parents=True,exist_ok=True)
    schema=json.loads((ROOT/"workspaces/runtime/packages/runtime-store/tests/fixtures/live-state-schema.json").read_text())
    from codex_sync_entities import register_functions
    for database in ("canvas.sqlite3","analytics.sqlite3"):
        with sqlite3.connect(root/database) as db:
            register_functions(db)
            db.execute("PRAGMA journal_mode=WAL")
            db.execute("PRAGMA synchronous=OFF")
            db.execute("BEGIN")
            objects=sorted(schema[database]["sqlite_master"],key=lambda o:({"table":0,"index":1,"trigger":2}.get(o["type"],3),o["name"]))
            for obj in objects:
                if obj["name"] in {"runtime_search_next_config","runtime_search_next_data","runtime_search_next_docsize","runtime_search_next_idx"}:continue
                if obj.get("sql"):db.execute(obj["sql"])
            if database=="canvas.sqlite3":
                case=base()
                db.execute("INSERT INTO runtime_agents(id,record) VALUES(?,?)",("agent",json.dumps(case["agent"])))
                event=case["events"][0]
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES(:id,:agent,:kind,:text,:status,:created,:epoch,:turn_id,:error)",event)
                db.execute("INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('agent','agent',1,'fixture',?,0)",(json.dumps(dict(collection="agent",id="agent",value=case["agent"])),))

        db.close()
