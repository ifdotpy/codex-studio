"""Guarded real-function oracle; no Runtime instance, start call, or provider.

Freeze statements are compiled directly from Runtime.start's AST because Python
has not extracted that pure block into a function. No rewritten Python policy is
used. Only a fake put/loaded host and an in-memory SQLite connection are supplied.
"""
import os
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).absolute().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard, IsolationViolation

_GUARD = install_guard(block_processes=True)
for key in ("HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"):
    if not os.environ[key].startswith("/var/tmp/"):
        raise IsolationViolation("disposable /var/tmp homes required")
if "--guard-test" in sys.argv:
    _GUARD.self_test()
    sys.exit(0)

import ast
import copy
import json
import sqlite3
import types

SERVER = Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(SERVER))
from codex_runtime import Runtime
from codex_team_isolation import assert_events
from codex_budget import budget_admission
from codex_daybreak import turn_program

# Extract only the existing preparation/revalidation and freeze statements.
_tree = ast.parse((SERVER / "codex_runtime.py").read_text())
_runtime = next(n for n in _tree.body if isinstance(n, ast.ClassDef) and n.name == "Runtime")
_start = next(n for n in _runtime.body if isinstance(n, ast.FunctionDef) and n.name == "start")
_freeze = [n for n in ast.walk(_start) if isinstance(n, ast.If)
           and ast.unparse(n.test) == "not attempt.get('settingsFixed')"]
assert len(_freeze) == 1, "Runtime.start freeze boundary changed"
_revalidate = [n for n in ast.walk(_start) if isinstance(n, ast.If)
               and ast.unparse(n.test) == "settings_account != account_key and (not a.get('pendingSettings'))"]
assert len(_revalidate) == 1, "Runtime.start revalidation boundary changed"


def compile_block(name, arguments, statements):
    function = ast.FunctionDef(name=name,
        args=ast.arguments(posonlyargs=[], args=[ast.arg(arg=s) for s in arguments],
            kwonlyargs=[], kw_defaults=[], defaults=[]),
        body=copy.deepcopy(statements), decorator_list=[])
    module = ast.fix_missing_locations(ast.Module(body=[function], type_ignores=[]))
    namespace = {}
    exec(compile(module, str(SERVER / "codex_runtime.py"), "exec"), namespace)
    return namespace[name]


freeze = compile_block("freeze", ["self", "db", "current", "attempt", "revalidated_execution"], _freeze)
# Return the local result created by the original revalidation statement.
revalidate = compile_block("revalidate", ["self", "a", "account_key", "settings_account"],
    [ast.parse("revalidated_execution = None").body[0], *_revalidate,
     ast.Return(value=ast.Name(id="revalidated_execution", ctx=ast.Load()))])


class Host:
    validate_execution = staticmethod(Runtime.validate_execution)

    def __init__(self, db, catalog=None):
        self.db, self.catalog_value = db, catalog
        self.loaded = {"recipient"}
        self.writes = 0

    def catalog(self, account):
        if self.catalog_value is None:
            raise ValueError("catalog unavailable")
        return self.catalog_value

    def put(self, db, table, value):
        assert table == "agents"
        self.writes += 1
        db.execute("INSERT OR REPLACE INTO runtime_agents VALUES (?,?)",
                   (value["id"], json.dumps(value)))

    def agent(self, key, db):
        row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        return json.loads(row[0])

    def team_agents(self, db, root, **kwargs):
        return [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_agents")]


def database():
    db = sqlite3.connect(":memory:")
    for table in ("agents", "rooms", "complaints", "projects", "event_meta"):
        db.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
    return db


def insert(db, table, key, value):
    db.execute(f"INSERT INTO runtime_{table} VALUES (?,?)", (key, json.dumps(value)))


def authorization(case, db):
    snapshot = case["snapshot"]
    for key, agent in snapshot["agents"].items():
        insert(db, "agents", key, dict(id=agent["id"], rootId=agent["root_id"],
            parentId=agent["parent_id"], isLead=agent["is_lead"],
            deletedAt=1 if agent["deleted"] else None, cwd=agent["resolved_cwd"]))
    for key, room in snapshot["rooms"].items():
        insert(db, "rooms", key, dict(kind=room["kind"], rootId=room.get("root_id"), members=room.get("members")))
    for key, complaint in snapshot["complaints"].items():
        insert(db, "complaints", key, dict(author=complaint["author"], leadId=complaint["lead_id"]))
    for key, project in snapshot["projects"].items():
        insert(db, "projects", key, dict(path=project["resolved_path"], peerTeams=[
            dict(id=str(i), name="peers", members=members) for i, members in enumerate(project["teams"])]))
    events = []
    for i, event in enumerate(case["events"]):
        key = str(i)
        if event["kind"] == "followup" and event["sender_present"]:
            insert(db, "event_meta", key, {"senderId": event.get("sender")})
        payload = dict(sender=event.get("sender"), room=event.get("room"),
            complaint_id=event.get("complaint_id"), responder=event.get("responder"),
            response={"author": event.get("response_author")})
        events.append(dict(id=key, kind=event["kind"], text=json.dumps(payload)))
    before = list(db.iterdump())
    try:
        assert_events(Host(db), db, {"id": snapshot["recipient_id"]}, events)
    finally:
        assert list(db.iterdump()) == before, "authorization wrote state"
    return {"allowed": True}


def settings(value):
    return dict(model=value["model"], effort=value["effort"], nativeEffort=value["native_effort"],
                fastMode=value["fast_mode"], daybreakEnabled=value["daybreak_enabled"])


def agent(snapshot):
    result = dict(id="recipient", provider=snapshot.get("provider", "codex"), accountKey=snapshot["account_key"], **settings(snapshot["settings"]),
                  startAttempt={"settingsFixed": snapshot["settings_fixed"]})
    if snapshot["execution_settings_account_key"] is not None:
        result["executionSettingsAccountKey"] = snapshot["execution_settings_account_key"]
    if snapshot["pending"] is not None:
        result["pendingSettings"] = settings(snapshot["pending"]["settings"])
        if snapshot["pending"]["account_key"] is not None:
            result["pendingSettingsAccountKey"] = snapshot["pending"]["account_key"]
    return result


def catalog_value(value):
    if value is None:
        return None
    return {"data": [dict(model=row["model"], resolvedModel=row["resolved_model"], hidden=row["hidden"],
        supportedReasoningEfforts=[{"reasoningEffort": effort} for effort in row["supported_efforts"]],
        defaultReasoningEffort=row["default_effort"], availableAccessPrograms=row.get("access_programs"), serviceTiers=[{"id": "priority"}] if row["priority"] else [])
        for row in value["models"]]}


def execution(case, db):
    current, observed = agent(case["current"]), agent(case.get("observed", case["current"]))
    host = Host(db, catalog_value(case.get("catalog")))
    # Saved pending settings go through the real validator before the original
    # start-time freeze block. Freshness/account mismatches are Rust-only holds.
    if not current["startAttempt"]["settingsFixed"] and current.get("pendingSettings"):
        pending = current["pendingSettings"]
        host.validate_execution(host.catalog(current["accountKey"]), pending["model"], pending["effort"], pending["fastMode"])
    validated = revalidate(host, observed, observed["accountKey"],
                          observed.get("executionSettingsAccountKey", observed["accountKey"]))
    freeze(host, db, current, current["startAttempt"], validated)
    turn_program(host, current)
    fields = dict(model=current["model"], effort=current["effort"], native_effort=current["nativeEffort"],
                  fast_mode=current["fastMode"], daybreak_enabled=current["daybreakEnabled"])
    patch = None if not host.writes else dict(expected=case["current"], settings=fields,
        execution_settings_account_key=current.get("executionSettingsAccountKey"),
        consume_pending=case["current"]["pending"] is not None,
        invalidate_loaded="recipient" not in host.loaded, settings_fixed=current["startAttempt"]["settingsFixed"])
    return {"allowed": True, "selection": {"settings": fields, "freeze": patch}}


def budget(case, db):
    root = dict(id="recipient", rootId="recipient", tokenBudget=case.get("budget"),
                tokensUsed=case.get("spent", 0), created=0)
    insert(db, "agents", "recipient", root)
    # Existing budget state avoids unrelated sync table writers; admission still
    # runs the real aggregation and coverage checks against these observations.
    db.execute("CREATE TABLE runtime_budget(id TEXT PRIMARY KEY, record TEXT NOT NULL)")
    insert(db, "budget", "recipient", dict(spent=root["tokensUsed"], floor=root["tokensUsed"],
        noticeSpent=0, before=0, after=0, faults=[], ambiguousNotices=[], historicalNotices=0))
    budget_admission(Host(db), db, root)
    return {"allowed": True}


def run(case):
    with database() as db:
        try:
            return {"authorization": authorization, "execution": execution, "budget": budget}[case["gate"]](case, db)
        except ValueError as error:
            return {"allowed": False, "message": str(error)}


for line in sys.stdin:
    case = json.loads(line)
    if case.get("close"):
        break
    print(json.dumps(run(case)), flush=True)
