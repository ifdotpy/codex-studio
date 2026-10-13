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
import contextlib
import hashlib
import json
import sqlite3

SERVER = Path(__file__).resolve().parents[4] / "apps/server/src"
sys.path.insert(0, str(SERVER))
from codex_runtime import Runtime
from codex_team_isolation import assert_events
from codex_peer_teams import _path
from codex_budget import budget_admission
from codex_daybreak import turn_program

# Extract only the existing preparation/revalidation and freeze statements.
_tree = ast.parse((SERVER / "codex_runtime.py").read_text())
_runtime = next(n for n in _tree.body if isinstance(n, ast.ClassDef) and n.name == "Runtime")
_start = next(n for n in _runtime.body if isinstance(n, ast.FunctionDef) and n.name == "start")
_prepare = next(n for n in _runtime.body if isinstance(n, ast.FunctionDef) and n.name == "prepare_locked")


def check_boundaries(start, prepare):
    # Include enclosing control flow and statement order, not just extracted nodes.
    for node, expected in ((start, "78ff53425f5ea723308533bfc81dbf0d5d19f989d3b6f9f1745c40c2c0f69cca"),
                           (prepare, "3d2ce92df593b4597882e2315e7753e5b09ba8f74f33653dd358616400b43eb6")):
        actual = hashlib.sha256(ast.dump(node, include_attributes=False).encode()).hexdigest()
        assert actual == expected, "Runtime control flow changed: re-derive the submit-gates oracle boundaries before updating the digest"


check_boundaries(_start, _prepare)
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
        self.connection, self.catalog_value = db, catalog
        self.lock = contextlib.nullcontext()
        self.loaded = {"recipient"}
        self.writes = 0

    def db(self):
        return contextlib.nullcontext(self.connection)

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


def authorization_records(case, db):
    for table, records in case["records"].items():
        for key, value in records.items():
            insert(db, table, key, value)
    for table, records in case.get("raw_rows", {}).items():
        for key, raw in records.items():
            db.execute(f"INSERT OR REPLACE INTO runtime_{table} VALUES (?,?)", (key, raw))
    db.execute("CREATE TABLE runtime_events(id TEXT, kind TEXT, text TEXT)")
    for event in case["events"]:
        db.execute("INSERT INTO runtime_events VALUES (?,?,?)", (event["id"], event["kind"], event["text"]))
    records = {table: dict(db.execute(f"SELECT id,record FROM runtime_{table}"))
               for table in ("agents", "rooms", "complaints", "projects", "event_meta")}
    paths = {}
    for table, field in (("agents", "cwd"), ("projects", "path")):
        for raw in records[table].values():
            try:
                value = json.loads(raw)
                path = value.get(field) if isinstance(value, dict) else None
                if isinstance(path, str) and path.strip():
                    paths[path] = _path(path)
            except ValueError:
                pass
    events = [dict(id=row[0], kind=row[1], text=row[2]) for row in db.execute("SELECT * FROM runtime_events")]
    return dict(records, recipient_id=case["recipient_id"], events=events, resolved_paths=paths)


def authorization(case, db):
    before = list(db.iterdump())
    try:
        assert_events(Host(db), db, {"id": case["recipient_id"]}, case["events"])
    finally:
        assert list(db.iterdump()) == before, "authorization wrote state"
    return {"allowed": True}


def catalog_value(value):
    if value is None:
        return None
    return {"data": [dict(model=row["model"], resolvedModel=row["resolved_model"], hidden=row["hidden"],
        supportedReasoningEfforts=[{"reasoningEffort": effort} for effort in row["supported_efforts"]],
        defaultReasoningEffort=row["default_effort"], availableAccessPrograms=row.get("access_programs"), serviceTiers=[{"id": "priority"}] if row["priority"] else [])
        for row in value["models"]]}


# Extract the actual provider effort and service-tier projection, not a rewrite.
_tier = [n for n in ast.walk(_start) if isinstance(n, ast.Assign)
         and any(ast.unparse(target) == "params['serviceTier']" for target in n.targets)]
_effort = [n for n in ast.walk(_start) if isinstance(n, ast.If)
           and ast.unparse(n.test) == "a.get('nativeEffort', a.get('effort')) is not None"]
assert len(_tier) == len(_effort) == 1, "provider settings projection changed"
provider_settings = compile_block("provider_settings", ["a"],
    [ast.parse("params = {}").body[0], *_tier, *_effort,
     ast.Return(value=ast.Name(id="params", ctx=ast.Load()))])


def execution(case, db):
    insert(db, "agents", "recipient", case["current"])
    host = Host(db, catalog_value(case.get("catalog")))
    observed = copy.deepcopy(case.get("observed", case["current"]))
    validated = revalidate(host, observed, observed.get("accountKey", "default"),
        observed.get("executionSettingsAccountKey", observed.get("accountKey", "default")))
    if "commit_record" in case:
        db.execute("UPDATE runtime_agents SET record=? WHERE id='recipient'",
                   (json.dumps(case["commit_record"]),))
    current = host.agent("recipient", db)
    freeze(host, db, current, current.get("startAttempt") or {}, validated)
    turn_program(host, current)
    after = host.agent("recipient", db)
    return {"allowed": True, "record_after": after, "provider_settings": provider_settings(after),
            "freeze": bool(host.writes), "invalidated_loaded": "recipient" not in host.loaded}


# Evidence for the separate preparation consumer, not part of freeze parity.
_prepare_effort = [n for n in ast.walk(_prepare) if isinstance(n, ast.If)
                   and ast.unparse(n.test) == "'nativeEffort' not in a"]
assert len(_prepare_effort) == 1, "preparation effort boundary changed"
prepare_effort = compile_block("prepare_effort", ["self", "a"], _prepare_effort)


def preparation_effort(case, db):
    insert(db, "agents", "recipient", case["current"])
    host = Host(db, catalog_value(case.get("catalog")))
    host.loaded.clear()
    prepare_effort(host, host.agent("recipient", db))
    after = host.agent("recipient", db)
    return {"allowed": True, "record_after": after, "provider_settings": provider_settings(after)}


def boundary_guard(case, db):
    class WrapFreeze(ast.NodeTransformer):
        def visit_If(self, node):
            if ast.unparse(node.test) == "not attempt.get('settingsFixed')":
                return ast.If(test=ast.Constant(False), body=[node], orelse=[])
            return self.generic_visit(node)
    changed = WrapFreeze().visit(copy.deepcopy(_start))
    try:
        check_boundaries(changed, _prepare)
    except AssertionError:
        return {"allowed": True}
    raise AssertionError("oracle accepted a changed enclosing freeze guard")


def validation(case, db):
    s = case["settings"]
    effort = Runtime.validate_execution(catalog_value(case["catalog"]), s["model"], s.get("effort"),
        s.get("fastMode", False), fallback_effort=case.get("fallback_effort", False))
    return {"allowed": True, "efforts": list(effort)}


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
        records = authorization_records(case, db) if case["gate"] == "authorization" else None
        try:
            result = {"authorization": authorization, "execution": execution, "budget": budget,
                      "validation": validation, "preparation-effort": preparation_effort,
                      "boundary-guard": boundary_guard}[case["gate"]](case, db)
        except (ValueError, TypeError, KeyError) as error:
            result = {"allowed": False, "message": str(error)}
        if records is not None:
            result["records"] = records
        return result


for line in sys.stdin:
    case = json.loads(line)
    if case.get("close"):
        break
    print(json.dumps(run(case)), flush=True)
