"""Execute Runtime.start's real submission transaction, without a Runtime/provider.

The extracted AST includes every guard, transcript write, frozen-input decision,
and Submitted bind through db.commit(). Runtime.item and execution reconciliation
are real functions. Only surrounding preparation services and the clock are fixed.
"""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).parent / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
import ast
import contextlib
import copy
import hashlib
import json
import sqlite3
from types import SimpleNamespace
from unittest.mock import patch

SERVER = Path(__file__).resolve().parents[3] / "apps/server/src"
sys.path.insert(0, str(SERVER))
import codex_runtime
import codex_execution
import codex_claude_input_recovery as recovery
from codex_native_errors import NativeRpcError

source = (SERVER / "codex_runtime.py").read_text()
tree = ast.parse(source)
start = next(n for n in ast.walk(tree) if isinstance(n, ast.FunctionDef) and n.name == "start")
blocks = [n for n in ast.walk(start) if isinstance(n, ast.With)
          and n.body and ast.unparse(n.body[0]).startswith("timing['transcriptLockedAt']")]
assert len(blocks) == 1
block = blocks[0]
assert ast.unparse(block.body[-1]) == "db.commit()"
# Guard the entire selected transaction against unnoticed boundary/policy drift.
digest = hashlib.sha256(ast.dump(block, include_attributes=False).encode()).hexdigest()
# Filled from this base's real AST; no rewritten policy in this oracle.
EXPECTED_DIGEST = "42d3ca118ec25123f589475ce26bab9b858a88a4f74848519827885922c270ba"
assert digest == EXPECTED_DIGEST, "Re-derive Runtime.start submission boundary"
fn = ast.parse("def submit(self, a, rows, server, attempt_id, epoch, busy_at_reservation, program):\n    timing={}\n").body[0]
fn.body.extend([copy.deepcopy(block), ast.parse("return params").body[0]])
from codex_wakeups import reconcile_start
from codex_daybreak import turn_params
from codex_time import append_message_clocks, message_clock
namespace = dict(vars(codex_runtime), reconcile_start=reconcile_start, turn_params=turn_params,
                 append_message_clocks=append_message_clocks, message_clock=message_clock)
exec(compile(ast.fix_missing_locations(ast.Module(body=[fn], type_ignores=[])), str(SERVER / "codex_runtime.py"), "exec"), namespace)
submit = namespace["submit"]

TABLES = ("runtime_agents", "runtime_execution_runs", "runtime_execution_attempts", "runtime_execution_inputs",
          "runtime_events", "runtime_event_meta", "runtime_items", "runtime_item_fulltext", "runtime_operation_receipts")

def snapshot(db):
    result = {}
    for table in TABLES:
        rows = [dict(r) for r in db.execute(f"SELECT * FROM {table} ORDER BY 1,2")]
        for row in rows:
            if "record" in row:
                row["record"] = json.loads(row["record"])
        result[table] = rows
    return result

class Cut(Exception):
    pass

class Connection:
    def __init__(self, db, cut):
        self.raw, self.cut = db, cut
    def __getattr__(self, name):
        return getattr(self.raw, name)
    def commit(self):
        if self.cut == "before_commit":
            raise Cut()
        self.raw.commit()
        if self.cut == "after_commit":
            raise Cut()

class Host:
    item = codex_runtime.Runtime.item
    closed = False
    lock = contextlib.nullcontext()
    changed = SimpleNamespace(set=lambda: None)
    def __init__(self, db, cut=None):
        self.connection = db
        self.cut = cut
        self.server = SimpleNamespace(closed=False, supervisor_mode=False)
    @contextlib.contextmanager
    def db(self):
        try:
            yield Connection(self.connection, self.cut)
        except BaseException:
            self.connection.rollback()
            raise
    def agent(self, key, db):
        return json.loads(db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()[0])
    def put(self, db, table, value):
        assert table == "agents"
        previous = self.agent(value["id"], db)
        codex_execution.reconcile_agent(db, value, previous)
        db.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(value), value["id"]))
    def connection_current(self, account, connection):
        return account == "account" and connection == "connection"
    def server_for(self, *_):
        return self.server
    def agent_connection(self, _):
        return "connection"
    def assert_workspace_available(self, *_):
        pass
    def _stage_event_resources(self, *_):
        pass
    def model_event_text(self, _):
        return "compiled original"
    def unanswered_complaints(self, *_):
        return []
    def model_turn_context(self, *_):
        return ""
    def message_inputs(self, agent, text, assets):
        return [{"type": "text", "text": text}]
    def turn_permissions(self, _):
        return {}
    def tool_definitions(self, _):
        return [{"name": "refreshed"}]
    def index_item(self, *_):
        pass
    def touch_ui(self, *_):
        pass

def reservation(db, attempt, retry=False):
    agent = json.loads(db.execute("SELECT record FROM runtime_agents").fetchone()[0])
    current = dict(id=attempt, epoch=2, accountKey="account", threadId="thread", events=["one", "two"],
                   submitted=False, created=170.0 if retry else 110.0, settingsFixed=True,
                   requestTicket="request:" + attempt, connectionGeneration=7)
    if retry:
        current["claudeRetryOf"] = "attempt"
    agent.update(startAttempt=current, status="starting", inFlight=True)
    Host(db).put(db, "agents", agent)
    db.execute("UPDATE runtime_events SET status='reserved', turn_id=NULL,error=NULL")
    db.commit()
    return agent

def seed(path):
    db = sqlite3.connect(path)
    db.row_factory = sqlite3.Row
    db.executescript("""
      CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);
      CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,epoch INTEGER,kind TEXT,text TEXT,created REAL,status TEXT,turn_id TEXT,error TEXT);
      CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);
      CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,record TEXT,created REAL);
      CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT);
      CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);
      CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT);
    """)
    codex_execution.ensure_tables(db)
    agent = dict(id="agent",rootId="agent",epoch=2,status="starting",provider="claude",accountKey="account",
                 threadId="thread",autoWake=True,cwd="/fixture",role="lead",isLead=True,model="default",
                 yoloMode=False,compactions=0,events=0,complaintsPresented=[],cyberAccessProgram=None)
    db.execute("INSERT INTO runtime_agents VALUES('agent',?)", (json.dumps(agent),))
    for i, key in enumerate(("one", "two")):
        db.execute("INSERT INTO runtime_events VALUES (?,'agent',2,'user',?,?,'reserved',NULL,NULL)", (key,"original " + key,100+i))
        db.execute("INSERT INTO runtime_event_meta VALUES (?,?)", (key,json.dumps(dict(modelEventProjection=1))))
    reservation(db, "attempt")
    return db

def run(db, cut=None):
    host = Host(db, cut)
    agent = host.agent("agent", db)
    rows = [dict(r) for r in db.execute("SELECT * FROM runtime_events ORDER BY created")]
    # Fixed transcript clock makes complete rows comparable. Rust additionally
    # passes a later preparation clock on retry, which must not alter its receipt.
    with patch("time.time", return_value=150.0):
        return submit(host, agent, rows, host.server, agent["startAttempt"]["id"], 2, True, None)

def reject(db):
    host = Host(db)
    agent = host.agent("agent", db)
    result = recovery.recover_rejected_start(host, db, agent, agent["startAttempt"],
        account_key="account", connection_id="connection",
        error=NativeRpcError(dict(code=-32000,data=dict(turnStartOutcome="not_applied"))))
    # Runtime's existing settlement persists this agent in the same transaction.
    db.execute("UPDATE runtime_agents SET record=? WHERE id='agent'", (json.dumps(agent),))
    db.commit()
    assert result == "retry"

root = Path(sys.argv[1])
base = seed(root / "fresh.sqlite3")
cases = []
for retry in (False, True):
    if retry:
        run(base)
        reject(base)
        reservation(base, "retry", True)
    for fault in (None, "configuration", "before_commit", "after_commit", "bind"):
        if fault == "configuration" and not retry:
            continue
        path = root / f"{'retry' if retry else 'fresh'}-{fault}.sqlite3"
        db = sqlite3.connect(path)
        db.row_factory = sqlite3.Row
        base.backup(db)
        if fault == "configuration":
            db.execute("UPDATE runtime_agents SET record=json_set(record,'$.model','changed')")
            db.commit()
        before = snapshot(db)
        if fault == "bind":
            db.execute("CREATE TRIGGER cut BEFORE UPDATE ON runtime_agents WHEN json_extract(NEW.record,'$.startAttempt.submitted')=1 BEGIN SELECT RAISE(ABORT,'bind cut'); END")
        failed = False
        try:
            params = run(db, fault)
        except (Cut, sqlite3.Error, ValueError):
            failed = True
            params = None
        after = snapshot(db)
        if fault == "bind":
            db.execute("DROP TRIGGER cut")
        # Restore the exact prestate for Rust. Expected snapshots came from the
        # real transaction, including its explicit commit and failure rollback.
        db.close()
        db = sqlite3.connect(path)
        base.backup(db)
        if fault == "configuration":
            db.execute("UPDATE runtime_agents SET record=json_set(record,'$.model','changed')")
        db.commit()
        db.close()
        cases.append(dict(path=str(path),retry=retry,fault=fault,before=before,after=after,failed=failed,params=params))
base.close()
print(json.dumps(dict(digest=digest,cases=cases)))
