"""Real Python monitor writers on a minimal store; no Runtime constructor or provider."""
import sys
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
sys.path.insert(0, sys.argv[1])
import base64
import copy
import json
import sqlite3
import threading
from contextlib import contextmanager
from unittest.mock import patch
from codex_runtime import Runtime, SubmissionRejected

NOW = 2000.0
ROOT = Path(sys.argv[2])

class Fixture:
    monitor_stall_probe = Runtime.monitor_stall_probe
    monitor_auto_approved = staticmethod(Runtime.monitor_auto_approved)
    monitor_input = Runtime.monitor_input
    monitor = Runtime.monitor
    cancel_monitor = Runtime.cancel_monitor
    output = Runtime.output
    monitors_tick = Runtime.monitors_tick
    _finish_monitor = Runtime._finish_monitor
    _monitor_exit_event = Runtime._monitor_exit_event
    _monitor_reattach_notice = Runtime._monitor_reattach_notice
    _recovery_event_pending = Runtime._recovery_event_pending
    enqueue_recovery_event = Runtime.enqueue_recovery_event
    def __init__(self):
        self.root = ROOT
        self.lock = threading.RLock()
        self.changed = threading.Event()
        self.closed = False
        self.probes = []
        self.submissions = []
        self.input_outcome = "acknowledged"
        fixture = self
        class Server:
            closed = False
            def on_result(self, *args): pass
            def wait(self, submitted):
                if fixture.input_outcome == "uncertain": raise RuntimeError("transport disconnected")
                if fixture.input_outcome == "failed": raise SubmissionRejected("native rejected")
                return {}
        self.server = Server()
        self.servers = {"default": self.server}
        self.connection_ids = {"default": "connection"}
        self.pool = type("Pool", (), {"submit": lambda _, fn, *args: self.probes.append(args)})()
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        for table in ("agents", "monitors", "requests"):
            self.connection.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
        self.connection.execute("CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT)")
        self.actor = {"id": "agent", "rootId": "agent", "epoch": 2, "autoWake": True, "status": "idle", "cwd": str(ROOT), "accountKey": "default"}
        self.put(self.connection, "agents", self.actor)
    @contextmanager
    def db(self):
        with self.connection:
            yield self.connection
    def put(self, db, table, record, **kwargs):
        db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES(?,?)", (record["id"], json.dumps(record)))
    def agent(self, identity, db=None):
        return json.loads((db or self.connection).execute("SELECT record FROM runtime_agents WHERE id=?", (identity,)).fetchone()[0])
    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table}")]
    def prepare(self, actor): return actor
    def connect(self, *args): return self.server
    def submit_reserved(self, server, method, params, operation_id=None):
        self.submissions.append(dict(method=method, params=params, identity=operation_id, record=copy.deepcopy(self.saved())))
        return "transport-ticket"
    def assert_workspace_available(self, *args): pass
    def launch_monitor(self, *args): pass
    def agent_connection(self, *args): return None
    def server_for(self, *args): return None
    def connection_current(self, *args): return True
    def enqueue(self, db, actor, kind, text, key):
        db.execute("INSERT OR IGNORE INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)", (key, actor["id"], kind, text, "pending" if actor["autoWake"] else "cancelled", NOW, actor["epoch"], None, None))
        return key
    def saved(self):
        return self.records(self.connection, "monitors")[0]
    def seed(self, changes):
        record = self.monitor("agent", {"command":"printf marker"}, key="monitor-corpus", approved=True)
        record.update(changes)
        self.put(self.connection, "monitors", record)
        return copy.deepcopy(record)

cases = []
def case(name, action, changes, argument=None, actor_changes=None):
    fixture = Fixture()
    before = fixture.seed(changes)
    actor = fixture.agent("agent")
    actor.update(actor_changes or {})
    fixture.put(fixture.connection, "agents", actor)
    probe_results = []
    def record_probe(frame, event, value):
        if event == "return" and frame.f_code is Runtime.monitor_stall_probe.__code__:
            probe_results.append(copy.deepcopy(frame.f_locals["result"]))
    if action == "legacy":
        previous = dict(before, status="lost", exitCode=None, error="Server restarted. Command outcome unknown; not rerun.")
        fixture.connection.execute("INSERT INTO runtime_events VALUES(?,?,?,?,?,?,?,?,?)", (f"monitor:{before['id']}", "agent", "monitor_exit", json.dumps(previous), "cancelled", 1500.0, 2, None, None))
    prior_events = [{**dict(row), "text": json.loads(row["text"])} for row in fixture.connection.execute("SELECT * FROM runtime_events ORDER BY id")]
    with patch("codex_runtime.time.time", return_value=NOW):
        if action == "create": result = fixture.monitor("agent", argument, key="create-case", approved=False)
        elif action == "cancel": result = fixture.cancel_monitor(before["id"])
        elif action in ("finish", "legacy"):
            code, error = argument
            result = fixture._finish_monitor(before["id"], code, error, finished=NOW)
        elif action == "output":
            result = fixture.output({"processId": before["id"], "deltaBase64": base64.b64encode(bytes(argument)).decode()})
        elif action == "tick": result = fixture.monitors_tick()
        elif action == "probe":
            sys.setprofile(record_probe)
            try: result = fixture.monitor_stall_probe(before["id"], argument, 1, "printf alive", "agent", 2)
            finally: sys.setprofile(None)
        elif action == "input":
            fixture.input_outcome = argument["outcome"]
            with patch("codex_rules.uuid.uuid4", return_value="ticket-1"), patch("codex_rules.time.time", return_value=NOW):
                try: result = fixture.monitor_input(before["id"], argument["input"])
                except (RuntimeError, SubmissionRejected): result = None
        else: raise AssertionError(action)
    events = [{**dict(row), "text": json.loads(row["text"])} for row in fixture.connection.execute("SELECT * FROM runtime_events ORDER BY id")]
    cases.append(dict(name=name, action=action, before=before, actor=actor, argument=argument, after=result if action == "create" else fixture.saved(), response=result, events=events, priorEvents=prior_events, probes=fixture.probes, probeResults=probe_results, submissions=fixture.submissions))
    fixture.connection.close()

operation = {"agent":"agent", "epoch":2, "accountKey":"default", "connectionId":"connection"}
with patch("codex_runtime.time.time", return_value=NOW):
    for actor_changes in ({}, {"autoWake":False}, {"deletedAt":1.0}, {"epoch":3}):
        for generation in (0,1):
            case(f"probe-{actor_changes}-{generation}", "probe", dict(status="running",operation=operation,activityAt=1,stallProbeGeneration=0), generation, actor_changes)
    case("probe-stale-reservation", "probe", dict(status="running",operation=operation,activityAt=1,activityGeneration=1,stallProbeGeneration=0), 1)
    for receipt in (
        {"reattachRecovery":{"status":"running","epoch":2,"accountKey":"default","cancelledNoticeLostAt":1600}},
        {},
    ):
        case(f"legacy-{bool(receipt)}", "legacy", dict(status="running",operation=operation,created=1000,**receipt), [0,None], {"threadId":"thread", "supervisorRestore":{"status":"restored","reason":"live_handle_resumed","epoch":2,"accountKey":"default","threadId":"thread","turnId":"turn","at":1700}})
    case("create-approval", "create", {}, dict(command="printf marker", success_exit_codes=[2,0,2],liveness_command="  printf alive  "))
    for outcome in ("acknowledged", "uncertain", "failed"):
        for data in ({"text":"hello"}, {"text":"hello", "closeStdin":True}, {"rows":43}, {"cols":110}):
            case(f"input-{outcome}-{data}", "input", dict(status="running", operation=operation, interactive=True), dict(outcome=outcome, input=data))
    for status in ("approval", "starting", "running", "completed", "failed", "cancelled", "lost"):
        for cancelled in (False, True):
            case(f"cancel-{status}-{cancelled}", "cancel", dict(status=status, cancelRequested=cancelled))
    for code in (0, 1, 2, 255, None):
        for error in (None, "failed"):
            if code is None and error is None: continue
            for cancelled in (False, True):
                case(f"finish-{code}-{error}-{cancelled}", "finish", dict(status="running", operation=operation, cancelRequested=cancelled, successExitCodes=[0, 2]), [code, error])
    for status in ("running", "completed"):
        for chunk in (b"", b"hello", b"x" * 12500, "\U0001f680".encode()*4000, bytes([0xff, 0xe2])):
            case(f"output-{status}-{len(chunk)}", "output", dict(status=status, operation=operation, tail="prefix"), list(chunk))
    for timeout in (0, 10, 1800):
        for generation in (0, 2):
            for prior in (-1, generation):
                for liveness in ("", "printf alive"):
                    case(f"tick-{timeout}-{generation}-{prior}-{liveness}", "tick", dict(status="running", operation=operation, activityAt=1, stallTimeoutSeconds=timeout, activityGeneration=generation, stallWakeGeneration=prior, livenessCommand=liveness))
    for changes in ({"autoWake":False}, {"deletedAt":1.0}, {"epoch":3}):
        case(f"tick-owner-{next(iter(changes))}", "tick", dict(status="running", operation=operation, activityAt=1), actor_changes=changes)
    for started in (NOW-89, NOW-90):
        case(f"tick-probe-expiry-{started}", "tick", dict(status="running", operation=operation, activityAt=1, stallProbeGeneration=0, stallProbeStarted=started, livenessCommand="printf alive"))
# One prepared Python-written template, copied by the Rust recovery/failpoint witnesses.
template = ROOT / "template"
template.mkdir()
schema = json.loads((Path(__file__).resolve().parents[3] / "runtime-store/tests/fixtures/live-state-schema.json").read_text())
for database in ("canvas.sqlite3", "analytics.sqlite3"):
    connection = sqlite3.connect(template / database)
    connection.execute("PRAGMA journal_mode=WAL")
    connection.execute("BEGIN")
    objects = schema[database]["sqlite_master"]
    order = {"table": 0, "index": 1, "trigger": 2, "view": 3}
    for item in sorted(objects, key=lambda item: (order.get(item["type"], 4), item["name"])):
        if item["name"] in {"runtime_search_next_config", "runtime_search_next_data", "runtime_search_next_docsize", "runtime_search_next_idx"}: continue
        if item["sql"]: connection.execute(item["sql"])
    connection.commit()
    connection.close()
print(json.dumps(cases))
