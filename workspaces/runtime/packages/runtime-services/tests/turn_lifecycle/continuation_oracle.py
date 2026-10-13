"""Differential oracle: real continuation functions, disposable SQLite, injected observations.

No Runtime construction, provider, process launch, or reset-credit operation is allowed.
"""
import copy
import contextlib
import json
import os
from pathlib import Path
import sqlite3
import sys
import threading
from types import SimpleNamespace
from unittest.mock import patch
from oracle_guard import install_guard

install_guard(block_processes=True)
ROOT = Path(__file__).resolve().parents[6]
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_usage_resume import UsageResumeMixin
from codex_capacity_retry import CapacityRetryMixin
import codex_claude_auth_wait as auth
import codex_wakeups as wake
from codex_runtime import Runtime
from codex_work import WorkMixin

NOW = 1000.0


def agent():
    return dict(id="a", rootId="a", epoch=1, status="failed", accountKey="default",
                threadId="thread", lastCompletedTurn="turn", autoWake=True,
                nativeFailureHold=True, cwd="/fixture", isLead=True)


def resume():
    return dict(id="resume", accountKey="default", threadId="thread", epoch=1,
                turnId="turn", cause="usage_limit", status="scheduled", failedAt=900.0,
                authAttempt=0, authRefreshMarker=None, dueAt=990.0, resetAt=None,
                plannedAt=None, reason=None, taskClaims=[])


def retry():
    return dict(id="retry", accountKey="default", threadId="thread", epoch=1,
                turnId="turn", cause="serverOverloaded", status="scheduled", dueAt=990.0,
                attempt=1, maxAttempts=4, cwd="/fixture", settings={}, taskClaims=[])


def event(identity="event", kind="user", payload=None, status="pending"):
    return dict(id=identity, agent="a", epoch=1, kind=kind,
                text=json.dumps(payload if payload is not None else {}), status=status,
                created=1.0, turn_id=None, error=None)


def snapshot(a=None, r=None, **extra):
    a = copy.deepcopy(a or agent())
    if r is not None:
        a["capacityRetry" if "maxAttempts" in r else "usageResume"] = copy.deepcopy(r)
    return dict(agent=a, record=copy.deepcopy(r), ancestors=[], claims=[], events=[],
                metadata={}, work={}, complaints={}, assets={}, **extra)


class Fixture(UsageResumeMixin, CapacityRetryMixin):
    continuation_work_claims = staticmethod(WorkMixin.continuation_work_claims)
    continuation_work_claims_valid = staticmethod(WorkMixin.continuation_work_claims_valid)
    complaint_needs_response = staticmethod(Runtime.complaint_needs_response)

    def __init__(self, data, limits=None, marker=None):
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        self.connection.executescript("""
        CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
        CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY,agent TEXT,record TEXT);
        CREATE TABLE runtime_capacity_retries(id TEXT PRIMARY KEY,agent TEXT,record TEXT);
        CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
        """)
        for table in ("work", "event_meta", "assets", "complaints", "requests"):
            self.connection.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT)")
        self.put(self.connection, "agents", data["agent"])
        for a in data["ancestors"]:
            self.put(self.connection, "agents", a)
        for table in ("work", "assets", "complaints", "metadata"):
            for identity, record in data[table].items():
                if record is not None:
                    self.connection.execute(f"INSERT INTO runtime_{'event_meta' if table == 'metadata' else table} VALUES (?,?)", (identity,json.dumps(record)))
        for identity in data["claims"]:
            self.connection.execute("INSERT OR REPLACE INTO runtime_work VALUES (?,?)", (identity,json.dumps(dict(id=identity,owner="a",rootId="a",status="running"))))
        for e in data["events"]:
            self.connection.execute("INSERT INTO runtime_events VALUES (:id,:agent,:kind,:text,:status,:created,:epoch,:turn_id,:error)",e)
        if data["record"]:
            r=data["record"]
            self.connection.execute(f"INSERT INTO runtime_{'capacity_retries' if 'maxAttempts' in r else 'usage_resumes'} VALUES (?,?,?)",(r["id"],"a",json.dumps(r)))
        self.lock=contextlib.nullcontext()
        self.changed=threading.Event()
        self.closed=False
        self.observation=limits or dict(accountKey="default")
        self.marker=marker
        self.holds=[]
        self.starts=[]
        self.loaded=set()
        # Capture the post-commit submission intent; never invoke its callback.
        self.pool=SimpleNamespace(submit=lambda callback,key,attempt:self.starts.append([key,copy.deepcopy(attempt)]))
        self.settings={}
        self.accounts=SimpleNamespace(lock=contextlib.nullcontext(),_row=lambda _: self.pin)
        self.pin={}

    def db(self): return contextlib.nullcontext(self.connection)
    read_db=db
    def agent(self,key,db=None): return json.loads(self.connection.execute("SELECT record FROM runtime_agents WHERE id=?",(key,)).fetchone()[0])
    def put(self,db,table,r): db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)",(r["id"],json.dumps(r)))
    def limits(self,*a,**kw): return self.observation
    def rate_limits_for(self,*a): return self.observation
    def usage_resume_auth_marker(self,*a): return self.marker
    def preparation_settings(self,a): return self.settings
    def records(self,db,table): return [json.loads(r[0]) for r in db.execute(f"SELECT record FROM runtime_{table}")]
    def assert_workspace_available(self,db,a): pass
    def team_agents(self,db,root_id,**kwargs): return [self.agent(root_id,db)]
    def dispatch_active_slots(self,db): return []
    def permanent_worker_hold(self,db,a,operation,transition,reason): self.holds.append([operation,transition,reason])
    def enqueue(self,db,a,kind,text,identity):
        db.execute("INSERT OR IGNORE INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",(identity,a["id"],kind,text,"pending",NOW,a["epoch"],None,None))
    def output(self,data,batch=None,capacity=False):
        table="capacity_retries" if capacity or data["record"] and "maxAttempts" in data["record"] else "usage_resumes"
        row=self.connection.execute(f"SELECT record FROM runtime_{table} ORDER BY id LIMIT 1").fetchone()
        return dict(agent=self.agent("a"),record=json.loads(row[0]) if row else None,
                    events=[dict(r) for r in self.connection.execute("SELECT * FROM runtime_events ORDER BY created,rowid")],
                    metadata={r[0]:json.loads(r[1]) for r in self.connection.execute("SELECT id,record FROM runtime_event_meta ORDER BY id")},holds=self.holds,batch=batch or [],starts=self.starts)


def corpus():
    cases=[]
    def add(name,family,data,**args):
        data=copy.deepcopy(data)
        f=Fixture(data,args.get("limits"),args.get("marker"))
        with patch("time.time",return_value=NOW):
            a=f.agent("a"); r=copy.deepcopy(data["record"]); batch=None
            if family=="usage":f.usage_resume_tick()
            elif family=="create":
                r=f.usage_resume_record(a,args["turn"],args["error"],auth_attempt=args["attempt"]);r["taskClaims"]=[]
                f.usage_resume_save(f.connection,a,r)
                if r["cause"]=="auth":a["authResumeAttempt"]=args["attempt"]+1
                f.put(f.connection,"agents",a)
            elif family=="limits":f.usage_resume_limits_changed("default",f.observation)
            elif family=="enable":f.usage_resume_action("a",r["id"],args["enabled"])
            elif family=="capacity-create":
                f.capacity_completed(f.connection,a,args["turn"],True)
                f.put(f.connection,"agents",a)
            elif family=="capacity":
                action=args["action"]
                if action=="claim":
                    f.capacity_retry("a",r["id"],"retry")
                    a=f.agent("a")
                elif action=="error":f.capacity_error(f.connection,a,args["attempt"],ValueError(args["reason"]),args["unknown"])
                elif action=="restart":f.capacity_restart(f.connection,a)
                elif action=="reset":f.capacity_reset(f.connection,a,args["reason"])
                elif action=="wait":f.capacity_wait(f.connection,a,r,args["reason"])
                elif action=="accepted":f.capacity_started(f.connection,a,args["attempt"],args["turn"])
                f.put(f.connection,"agents",a)
            elif family=="wake":batch=wake.pending_batch(f,f.connection,a)
            elif family=="reconcile":wake.reconcile_start(f,f.connection,a,[e for e in data["events"] if e["id"] in args["ids"]])
            elif family=="auth":
                f.pin=args["pin"]
                token=object();f._claude_auth_wait_jobs={"default":token}
                import codex_claude
                with patch.object(codex_claude,"auth_metadata",return_value=args["result"]):
                    auth._probe(f,"default",auth._account_pin(f.pin),token)
            else:raise AssertionError(family)
            expected=f.output(data,batch,capacity=family=="capacity-create")
        cases.append(dict(name=name,family=family,input=data,args=args,expected=expected))
        f.connection.close()
    inputs=json.loads((Path(__file__).parent.parent/"continuation/cases.json").read_text())
    witnesses={"capacity-claim-success","capacity-error-known","capacity-error-unknown","usage-observation-stale-True","create-usageLimitExceeded","create-authExpired","usage-percent-100","usage-percent-99.9","usage-observation-at-994.9","usage-observation-at-995.0","usage-observation-accountKey-foreign","auth-marker-old","auth-marker-new","auth-backoff-0","auth-backoff-50","usage-task-changed","usage-identity-epoch","usage-embedded-already-started","limits-floor-990.0","disable-scheduled","capacity-restart-unknown-True","capacity-restart-starting-False","capacity-reset-starting-True","capacity-wait-20","capacity-accepted-None","progress-(1, 2)","progress-(2, 2)","reconcile-resolved-a","review-submitted-True","review-submitted-False","claude-proof-readyaccount","claude-proof-readyforeign","claude-input-text","claude-account-pin-changed","claude-wrong-credential"}
    if os.environ.get("STUDIO_RUNTIME_CORPUS_FULL")!="1":inputs=[case for case in inputs if case["name"] in witnesses or case["name"] in {"capacity-create-server-0","capacity-create-network-7"}]
    for case in inputs:add(case["name"],case["family"],case["input"],**case["args"])
    return cases


def template(path, kind="usage"):
    path.mkdir(parents=True,exist_ok=True)
    schema=json.loads((ROOT/"workspaces/runtime/packages/runtime-store/tests/fixtures/live-state-schema.json").read_text())
    for name in ("canvas.sqlite3","analytics.sqlite3"):
        db=sqlite3.connect(path/name)
        db.execute("PRAGMA journal_mode=WAL")
        db.execute("PRAGMA synchronous=FULL")
        db.execute("BEGIN")
        objects=schema[name]["sqlite_master"]
        order={"table":0,"index":1,"view":2,"trigger":3}
        triggers=[]
        for o in sorted(objects,key=lambda o:(order.get(o["type"],4),o["name"])):
            if o["type"]=="trigger":
                triggers.append(o["sql"]);continue
            if o["name"] in {"runtime_search_next_config","runtime_search_next_data","runtime_search_next_docsize","runtime_search_next_idx"}:continue
            if o.get("sql"):db.execute(o["sql"])
        if name=="canvas.sqlite3":
            a=agent();r=resume()
            with patch("time.time",return_value=800.0):UsageResumeMixin.usage_resume_save(None,db,a,r)
            if kind=="capacity":
                with patch("time.time",return_value=800.0):CapacityRetryMixin.capacity_save(None,db,a,retry())
            db.execute("INSERT INTO runtime_agents VALUES (?,?)",("a",json.dumps(a)))
            if kind=="wake":
                cases=json.loads((Path(__file__).parent.parent/"continuation/cases.json").read_text())
                data=next(c["input"] for c in cases if c["name"]=="progress-(1, 2)")
                fixture=Fixture(data)
                for e in fixture.connection.execute("SELECT * FROM runtime_events"):
                    db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",tuple(e))
                fixture.connection.close()
        for sql in triggers:db.execute(sql)
        db.commit();db.close()


if __name__=="__main__":
    if len(sys.argv)>1 and sys.argv[1]=="template":template(Path(sys.argv[2]), sys.argv[3] if len(sys.argv)>3 else "usage")
    else:print(json.dumps(corpus(),allow_nan=False))
