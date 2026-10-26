"""Run real Python relocation functions on disposable Python-written records."""
import copy
import json
import os
from pathlib import Path
import sqlite3
import sys
from types import SimpleNamespace, ModuleType
from datetime import datetime, timezone
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).parent.parent / "turn_lifecycle"))
from oracle_guard import install_guard

guard = install_guard(block_processes=True)
if "--guard-self-test" in sys.argv:
    guard.self_test()
    raise SystemExit(0)

root = Path(sys.argv[1])
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))
import ast
import hashlib
import math
import threading
import time
import uuid
from contextlib import contextmanager
from typing import Any, cast

source_root = Path(__file__).resolve().parents[4] / "apps/server/src"
pins = json.loads(Path(__file__).with_name("oracle-digests.json").read_text())


def actual(filename, owner, names, namespace=None):
    source = (source_root / filename).read_text()
    nodes = ast.parse(source).body
    if owner:
        nodes = next(node for node in nodes if isinstance(node, ast.ClassDef) and node.name == owner).body
    namespace = globals() if namespace is None else namespace
    for name in names:
        node = [node for node in nodes if isinstance(node, ast.FunctionDef) and node.name == name][-1]
        text = ast.get_source_segment(source, node)
        key = filename + ":" + (owner + "." if owner else "") + name
        assert hashlib.sha256(text.encode()).hexdigest() == pins[key], "Python oracle changed: " + key
        exec("from __future__ import annotations\n" + text, namespace)
    return namespace


def actual_constant(filename, name):
    source = (source_root / filename).read_text()
    node = next(node for node in ast.parse(source).body if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == name for target in node.targets))
    namespace = {}
    exec(ast.get_source_segment(source, node), namespace)
    return namespace[name]


actual("codex_multi_server_orchestration.py", None, ["identity", "encoded"])
actual("codex_project_locations.py", None, ["local_server", "normalize", "applied", "refresh_tool_receipt"])
actual("codex_time.py", None, ["clock_stamp", "stamp_tool_result"])
EXTERNALIZE_THRESHOLD = actual_constant("codex_payloads.py", "EXTERNALIZE_THRESHOLD")
actual("codex_payloads.py", None, ["externalize_result"])
for module_name, exports in [("codex_multi_server_orchestration", {"identity": identity}), ("codex_payloads", {"externalize_result": externalize_result}), ("codex_time", {"stamp_tool_result": stamp_tool_result})]:
    module = ModuleType(module_name)
    module.__dict__.update(exports)
    sys.modules[module_name] = module
work = actual("codex_work.py", "WorkMixin", ["operation_receipt", "save_receipt"], dict(globals()))
move_names = ["__init__", "_get", "_save", "_check_deadline", "_expire", "_hold", "_release_untransferred", "capture_cache", "status"]
DEADLINE = actual_constant("codex_agent_move.py", "DEADLINE")
move_namespace = actual("codex_agent_move.py", "AgentMoves", move_names, dict(globals()))
AgentMoves = type("AgentMoves", (), {name: move_namespace[name] for name in move_names})


class FixtureRuntime:
    # Only persistence primitives are faked: every decision calls its real Python function.
    operation_receipt = work["operation_receipt"]
    save_receipt = work["save_receipt"]

    def __init__(self, root):
        self.root = root
        root.mkdir(parents=True)
        self._project_server_id = "local-fixture"
        self.lock = threading.RLock()
        self.changed = threading.Event()
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        schema = json.loads((Path(__file__).resolve().parents[3] / "runtime-store/tests/fixtures/live-state-schema.json").read_text())
        shadow = {"runtime_search_next_config", "runtime_search_next_data", "runtime_search_next_docsize", "runtime_search_next_idx"}
        order = {"table": 0, "index": 1, "trigger": 2, "view": 3}
        for database, document in schema.items():
            with (self.connection if database == "canvas.sqlite3" else sqlite3.connect(root / database)) as db:
                statements = [entry["sql"] for entry in sorted(document["sqlite_master"], key=lambda v: (order.get(v["type"], 4), v["name"])) if entry.get("sql") and entry["name"] not in shadow]
                db.executescript("BEGIN;\n" + ";\n".join(statements) + ";\nCOMMIT;")
                if database != "canvas.sqlite3":
                    db.execute("PRAGMA journal_mode=WAL")

    @contextmanager
    def db(self):
        with self.connection:
            yield self.connection

    read_db = db

    def put(self, db, table, record):
        db.execute(f"INSERT INTO runtime_{table}(id,record) VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record", (record["id"], json.dumps(record)))

    def agent(self, key, db=None):
        if db is not None:
            return row(db, "runtime_agents", key)
        with self.db() as db:
            return row(db, "runtime_agents", key)

    def tool_result(self, db, key):
        return row(db, "runtime_tool_results", key, "result")

    def close(self):
        destination = sqlite3.connect(self.root / "canvas.sqlite3")
        self.connection.backup(destination)
        destination.execute("PRAGMA journal_mode=WAL")
        destination.close()
        self.connection.close()


runtime = FixtureRuntime(root)
agent = {"id": "agent-fixture", "rootId": "agent-fixture", "epoch": 1, "status": "waiting", "autoWake": True, "inFlight": False, "threadId": "native-thread", "accountKey": "default", "cwd": str(root)}
actor = agent["id"]
local = runtime._project_server_id
moves = AgentMoves(SimpleNamespace(runtime=runtime, server_id=local))
now = 1000.125


def row(db, table, key, column="record"):
    found = db.execute(f"SELECT {column} FROM {table} WHERE id=?", (key,)).fetchone()
    return json.loads(found[0]) if found else None


def save_agent(value):
    with runtime.db() as db:
        runtime.put(db, "agents", value)


cases = []
for spec in json.loads(Path(__file__).with_name("cases.json").read_text()):
    case = copy.deepcopy(spec)
    name, family = case["name"], case["family"]
    if family == "location":
        request = {"action": case.get("action", "add_location"), "project": "/home-project", "server": "peer", "path": "/requested" if case.get("action", "add_location") == "add_location" else None}
        key = identity("project-location", "ui", name)
        project = {"id": "/home-project", "path": "/home-project", "name": "東京", "created": 10,
                   "homeServerId": local, "locations": [{"serverId": local, "path": "/home-project", "projectId": "/home-project"}, {"serverId": "peer", "path": "/old", "projectId": "/old"}], "locationsRevision": 3, "extension": {"nested": [1, None, "🦀"]}}
        if case.get("legacy"):
            for field in ("homeServerId", "locations", "locationsRevision"):
                project.pop(field)
        with runtime.db() as db:
            signature, _ = runtime.operation_receipt(db, key, request)
            envelope = {"requestId": key, "action": request["action"], "payload": {"project": project["id"], "name": project["name"], "path": request["path"], "request": request, "signature": signature}}
            receipt = {"requestId": key, "outcome": "applied", "value": {"serverId": "peer", "path": request["path"], "canonicalPath": case.get("canonical", "/canonical"), "projectId": case.get("canonical", "/canonical"), "gitOrigin": None, "homeProjectId": project["id"], "requestId": key}} if request["action"] == "add_location" else {"requestId": key, "outcome": "applied", "value": {"removed": True, "serverId": "peer"}}
            runtime.put(db, "projects", project)
            if case.get("deleted"):
                db.execute("DELETE FROM runtime_projects WHERE id=?", (project["id"],))
            db.execute("INSERT INTO runtime_server_outbox(id,server,body,state) VALUES(?,?,?,'queued')", (key, "peer", encoded(envelope)))
            registration = "project-registration:" + project["id"]
            db.execute("DELETE FROM runtime_operation_receipts WHERE id=?", (registration,))
            runtime.save_receipt(db, registration, "registration", {"outcome": "unknown", "projectId": project["id"], "requestId": key})
            before = row(db, "runtime_projects", project["id"])
            case.update(outbox={"id": key, "server": "peer", "body": envelope, "state": "queued", "result": None}, project=before, receipt=receipt)
            with patch("time.time", return_value=now):
                applied(runtime, db, envelope, "peer", receipt)
            case["expected"] = {"project": row(db, "runtime_projects", project["id"]), "receipt": row(db, "runtime_operation_receipts", key, "result"), "registration": row(db, "runtime_operation_receipts", registration, "result")}
    elif family == "tool":
        tool_id = "tool:" + name
        key = identity("project-location", actor, tool_id)
        request = {"action": "remove_location", "project": "/home-project", "server": "peer", "path": None}
        record = {"id": tool_id, "agent": actor, "epoch": 1, "accountKey": "default", "connectionId": "connection-a", "tool": "orchestration_servers", "stage": "completed", "outcome": case.get("oldOutcome", "unknown"), "updated": 1}
        previous = {"success": True, "contentItems": [{"type": "inputText", "text": json.dumps({"requestId": key, "outcome": case.get("previous", "unknown")})}]}
        record["result"] = previous
        receipt = {"requestId": key, "outcome": case.get("outcome", "applied"), "value": {"removed": True, "serverId": "peer"}}
        with runtime.db() as db:
            signature, _ = runtime.operation_receipt(db, key, request)
            envelope = {"requestId": key, "action": "remove_location", "payload": {"project": "/home-project", "name": "project", "path": None, "signature": signature, "request": request}}
            db.execute("INSERT INTO runtime_server_outbox(id,server,body,state,result) VALUES(?,?,?, ?,?)", (key, "peer", encoded(envelope), case.get("state", "complete"), encoded(receipt)))
            db.execute("INSERT INTO runtime_tool_results(id,result) VALUES(?,?)", (tool_id, json.dumps(previous)))
            runtime.put(db, "tool_requests", record)
            case.update(record=copy.deepcopy(record), previous=previous, outbox={"id": key, "server": "peer", "body": envelope, "state": case.get("state", "complete"), "result": receipt})
            with patch("time.time", return_value=now):
                case["changed"] = refresh_tool_receipt(runtime, db, record)
            case["expected"] = {"record": row(db, "runtime_tool_requests", tool_id), "result": row(db, "runtime_tool_results", tool_id, "result")}
    elif family in {"release", "expire", "cache", "status"}:
        key = identity("move", actor, name)
        current = copy.deepcopy(agent)
        current.update(status="queued", inFlight=False, executionMove={"id": key, "phase": "unknown", "server": "peer"})
        operation = {"id": key, "side": "source", "agent": actor, "epoch": current["epoch"], "server": "peer", "phase": "unknown", "uncertainPhase": case.get("uncertain", "waiting"), "created": 700.0, "deadline": case.get("deadline", 900.0), "fingerprint": hashlib.sha256(encoded({"server": "peer", "cwd": "/target", "request_id": name}).encode()).hexdigest(), "result": {"status": "accepted", "requestId": key}}
        if family == "expire":
            operation["phase"] = case.get("phase", "waiting")
            operation.pop("uncertainPhase")
            current["executionMove"]["phase"] = operation["phase"]
            if case.get("missing_deadline"):
                operation.pop("deadline")
        if family == "cache":
            operation = {"id": identity(key, "target"), "move": key, "side": "target", "principal": "peer", "phase": case.get("phase", "active"), "created": 700.0, "deadline": 900.0, "descriptor": {"agent": copy.deepcopy(agent), "acceptedResult": {"status": "accepted", "requestId": key}}}
            current.update(movedFrom={"move": key, "server": "peer"}, inFlight=case.get("inFlight", True), turnId="turn-a")
            if case.get("already"):
                operation["firstTurnCache"] = {"cachedInputTokens": 23, "inputTokens": 34}
        if family == "status":
            operation["phase"] = case.get("phase", "waiting")
            operation["firstTurnCache"] = {"cachedInputTokens": 23}
        save_agent(current)
        with runtime.db() as db:
            moves._save(db, operation)
            before = row(db, "runtime_agent_moves", operation["id"])
        case.update(operation=before, agent=current)
        with patch("time.time", return_value=now):
            if family == "release":
                target = {"phase": case.get("target", "not_started")}
                case["targetReceipt"] = target
                case["changed"] = moves._release_untransferred(operation, target)
            elif family == "expire":
                case["changed"] = moves._expire(operation)
            elif family == "cache":
                notification = case.get("notification", {"turnId": "turn-a", "requestUsage": {"cachedInputTokens": 31, "inputTokens": 42, "cacheWriteInputTokens": 3}})
                case["notification"] = notification
                with runtime.db() as db:
                    moves.capture_cache(db, current, notification)
            else:
                case["status"] = moves.status(actor, name)
        with runtime.db() as db:
            case["expected"] = {"operation": row(db, "runtime_agent_moves", operation["id"]), "agent": row(db, "runtime_agents", actor)}
    cases.append(case)

# Leave one prepared Python database template; Rust clones it for each transactional case.
with runtime.db() as db:
    db.execute("DELETE FROM runtime_agent_moves")
    db.execute("DELETE FROM runtime_server_outbox")
    db.execute("DELETE FROM runtime_operation_receipts")
    db.execute("DELETE FROM runtime_projects")
    db.execute("DELETE FROM runtime_tool_requests")
    db.execute("DELETE FROM runtime_tool_results")
    runtime.put(db, "agents", agent)
runtime.close()
print(json.dumps({"local": local, "actor": actor, "now": now, "cases": cases}, ensure_ascii=False))
