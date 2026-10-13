"""Real budget functions on one cloned in-memory template; no Runtime/provider.

File coverage uses a supplied in-memory observation through a Path test double.
Every returned state is read from the Python writers, including unknown fields.
"""
import os
from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).absolute().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard, IsolationViolation
_GUARD = install_guard(block_processes=True)
for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
    if not os.environ[key].startswith("/var/tmp/"):
        raise IsolationViolation("disposable homes required")
import contextlib
import copy
import io
import json
import sqlite3
from types import SimpleNamespace
from unittest.mock import patch
SERVER = Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(SERVER))
import codex_budget as budget

_TEMPLATE = sqlite3.connect(":memory:")
_TEMPLATE.executescript("""
CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
CREATE TABLE runtime_native_action_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,receipt TEXT NOT NULL,outcome TEXT NOT NULL);
CREATE TABLE analytics_history(id TEXT PRIMARY KEY,agent TEXT,record TEXT NOT NULL);
""")
budget.budget_init(_TEMPLATE)
_TEMPLATE.commit()

class Host:
    closed = False
    def __init__(self, db):
        self.connection = db
        self.lock = contextlib.nullcontext()
        self.changed = SimpleNamespace(set=lambda: None)
    def db(self): return contextlib.nullcontext(self.connection)
    def agent(self, key, db):
        return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])
    def team_agents(self, db, root, **kwargs):
        return [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_agents ORDER BY id")]
    def put(self, db, table, value):
        db.execute("INSERT OR REPLACE INTO runtime_"+table+" VALUES (?,?)", (value["id"], json.dumps(value)))

class File:
    def __init__(self, value): self.value, self.calls = value, 0
    def stat(self):
        if self.value is None: raise OSError("synthetic unavailable")
        self.calls += 1
        return SimpleNamespace(st_dev=self.value["identity"][0], st_ino=self.value["identity"][1],
            st_size=self.value["size"] if self.calls == 1 else self.value["final_size"])
    def open(self, mode):
        offset = self.value["size"] - len(self.value["tail"].encode())
        handle = io.BytesIO(b" " * max(offset,0) + self.value["tail"].encode())
        return handle

def state(db):
    return {"agents":{row[0]:json.loads(row[1]) for row in db.execute("SELECT * FROM runtime_agents ORDER BY id")},
        "states":{row[0]:json.loads(row[1]) for row in db.execute("SELECT * FROM runtime_budget ORDER BY id")},
        "usage":[dict(row) for row in db.execute("SELECT * FROM runtime_budget_usage ORDER BY id")],
        "events":[dict(row) for row in db.execute("SELECT * FROM runtime_events ORDER BY id")],
        "receipts":{row[0]:json.loads(row[1]) for row in db.execute("SELECT id,outcome FROM runtime_native_action_receipts ORDER BY id")}}

def run(case):
    db=sqlite3.connect(":memory:"); _TEMPLATE.backup(db);db.row_factory=sqlite3.Row
    host=Host(db)
    for a in case["agents"]: host.put(db,"agents",a)
    for key,value in case.get("states",{}).items(): db.execute("INSERT INTO runtime_budget VALUES (?,?)",(key,json.dumps(value)))
    for row in case.get("usage",[]): db.execute("INSERT INTO runtime_budget_usage VALUES (:id,:agent,:kind,:tokens,:at)",row)
    for row in case.get("events",[]): db.execute("INSERT INTO runtime_events VALUES (:id,:agent,:kind,:text,:status,:created,:epoch,:turn_id,:error)",row)
    for key,receipt in case.get("receipts",{}).items():
        db.execute("INSERT INTO runtime_native_action_receipts VALUES (?,?,?,?)",(key,"fixture",json.dumps(receipt["receipt"]),json.dumps(receipt["outcome"])))
    with patch.object(budget.time,"time",lambda:100.):
        for seed in case.get("seed_captures",[]):
            a=host.agent(seed.get("agent","a"),db)
            budget.budget_capture(db,a,seed["params"],at=seed.get("at",101.),source=seed.get("source","live"))
    records=state(db); outputs=[];denial=None
    for step in case["steps"]:
        now=step.get("now",100.)
        agent_id=step.get("agent","a")
        with patch.object(budget.time,"time",lambda:now):
            if step["op"]=="capture":
                a=step.get("historical_agent") or host.agent(agent_id,db)
                result=budget.budget_capture(db,a,step["params"],at=step.get("at",now),source=step.get("source","live"))
            elif step["op"]=="update":
                a=host.agent(agent_id,db)
                a.update(step.get("set",{}))
                for path,value in step.get("paths",{}).items():
                    parent=a
                    fields=path.strip("/").split("/")
                    for field in fields[:-1]: parent=parent[field]
                    parent[fields[-1]]=value
                for name in step.get("remove",[]): a.pop(name,None)
                host.put(db,"agents",a);result=None
            elif step["op"] in ("admission", "status"):
                db.execute("DELETE FROM analytics_history")
                observations=step.get("histories",{})
                files={}
                for key,observation in observations.items():
                    a=host.agent(key,db)
                    record=observation.get("history")
                    if record is not None:
                        record=copy.deepcopy(record);record["path"]=key
                        identity=key+":"+a.get("accountKey","default")+":"+a["threadId"]
                        db.execute("INSERT INTO analytics_history VALUES (?,?,?)",(identity,key,json.dumps(record)))
                    files[key]=observation.get("file")
                if step.get("history_table") is False: db.execute("DROP TABLE analytics_history")
                try:
                    with patch.object(budget,"Path",lambda path:File(files.get(path))):
                        status=(budget.budget_status(host,db,host.agent(agent_id,db),check_coverage=step.get("check_coverage",True)) if step["op"]=="status" else budget.budget_admission(host,db,host.agent(agent_id,db)))
                    result=status if step["op"]=="status" else {"status":status,"denial":None};denial=None
                except ValueError as error:
                    denial=error
                    result={"status":{k:v for k,v in error.budgetAdmission.items() if k not in ("agentId","epoch","accountKey","threadId","attemptId")},
                        "denial":{"message":str(error),"receipt":error.budgetAdmission}}
            elif step["op"]=="defer":
                error=denial
                if step.get("denial") is not None:
                    error=ValueError(step["denial"]["message"]);error.budgetAdmission=step["denial"]["receipt"]
                result=budget.defer_budget_start(host,agent_id,step.get("attempt_id","attempt"),error,unknown=step.get("unknown",False))
            elif step["op"]=="claim":
                result=budget.claim_budget_wait(host,db,host.agent(agent_id,db))
            else: raise AssertionError("unknown operation")
        outputs.append({"result":result,"stored":state(db)})
    db.close()
    return {"records":records,"outputs":outputs}

for line in sys.stdin:
    case=json.loads(line)
    if case.get("close"): break
    print(json.dumps(run(case),separators=(",",":")),flush=True)
