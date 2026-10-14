"""Real Runtime.send/receipts on Python-written seed backups; no provider or timer."""
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).absolute().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
_GUARD = install_guard()

import concurrent.futures
import json
import hashlib
import os
import shutil
import sqlite3
import threading
from unittest.mock import patch

SERVER = Path(__file__).resolve().parents[4] / "apps/server"
sys.path.insert(0, str(SERVER / "tests"))
sys.path.insert(0, str(SERVER / "src"))
from http_backend_harness import BackendProcess

scratch = Path(sys.argv[1])
backend = BackendProcess(scratch, initialize_workspace=False)
binary = backend._directory("disabled-provider") / "codex"
binary.write_text("#!/bin/sh\nexit 99\n")
binary.chmod(0o700)
backend._provider_executable = binary
environment = backend._environment()
os.environ.clear()
os.environ.update(environment)
import codex_accounts
import codex_runtime
from codex_agent_modes import mode_fields
from codex_canvas import Canvas
from types import SimpleNamespace
from studio_api.work.router import create_router
from studio_api.work.models import ManagedMessageBody

class QuietRuntime(codex_runtime.Runtime):
    def schedule(self):
        pass
    def start_review_archives(self):
        pass
    def resume_read_only_image_bases(self):
        pass
    def connect(self, *args, **kwargs):
        raise AssertionError("provider forbidden")
    connect_agent = connect


def state(root):
    with sqlite3.connect(Path(root) / "canvas.sqlite3") as db:
        db.row_factory = sqlite3.Row
        events = [dict(row) for row in db.execute("SELECT * FROM runtime_events ORDER BY id")]
        for event in events:
            assert isinstance(event.pop("created"), (int, float))
        metadata = {}
        for key, raw in db.execute("SELECT id,record FROM runtime_event_meta ORDER BY id"):
            record = json.loads(raw)
            assert isinstance(record.pop("acceptedAt"), (int, float))
            # Scheduling timing is generated independently by each runtime.
            record.pop("timing", None)
            metadata[key] = record
        result = {"events": events, "metadata": metadata,
                  "agents": {key: json.loads(raw) for key, raw in db.execute("SELECT id,record FROM runtime_agents ORDER BY id")}}
        for table in ("messages", "runtime_operation_receipts", "sync_entities", "sync_documents", "sync_versions", "runtime_capacity_retries"):
            if not db.execute("SELECT 1 FROM sqlite_master WHERE name=?", (table,)).fetchone():
                result[table] = []
                continue
            rows = [dict(row) for row in db.execute(f"SELECT * FROM {table} ORDER BY rowid")]
            for row in rows:
                if table == "sync_entities" and row.get("payload") is not None:
                    assert row["hash"] == hashlib.sha256(row["payload"].encode()).hexdigest()
                    row["hash"] = "verified payload hash"
                if table == "messages":
                    assert isinstance(row["at"], (int, float))
                    row["at"] = "generated timestamp"
                for key in ("record", "result", "signature", "payload", "deliveries"):
                    if isinstance(row.get(key), str):
                        row[key] = json.loads(row[key])
            if table == "sync_entities":
                for row in rows:
                    if row["collection"] == "event":
                        assert isinstance(row["payload"]["value"]["created"], (int, float))
                        row["payload"]["value"]["created"] = "generated timestamp"
            if table == "runtime_capacity_retries":
                for row in rows:
                    if row["record"].get("status") == "finished":
                        assert row["record"]["updatedAt"] > 1, "capacity reset must refresh updatedAt"
                        row["record"]["updatedAt"] = "generated timestamp"
            result[table] = rows
        return result



def configure(runtime, db, case):
    base = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='agent'").fetchone()[0])
    for fields in case.get("others", []):
        runtime.put(db, "agents", mode_fields({**base, **fields}))
    agent = {**base, **case.get("agent", {})}
    if "capacity" in case:
        retry = case["capacity"]
        agent.update(capacityRetry=retry, capacityRetryCount=2)
        db.execute("INSERT OR REPLACE INTO runtime_capacity_retries VALUES(?,?,?)", (retry["id"], "agent", json.dumps(retry)))
    runtime.put(db, "agents", mode_fields(agent))


def run(runtime, template, endpoint, command):
    runtime.close_notification_database()
    runtime.__dict__.pop("_agent_records_cache", None)
    with sqlite3.connect(runtime.db_path) as db:
        template.backup(db)
    case = command["case"]
    root = Path(command["rust"])
    root.mkdir(parents=True)
    shutil.copytree(runtime.root / "multi-server", root / "multi-server")
    with runtime.read_db() as source, sqlite3.connect(root / "canvas.sqlite3") as target:
        source.backup(target)
    with sqlite3.connect(runtime.analytics_db_path) as source, sqlite3.connect(root / "analytics.sqlite3") as target:
        source.backup(target)
    before = state(runtime.root)
    assert state(root) == before
    # Live policy changes are applied identically after each store starts.
    with runtime.db() as db:
        configure(runtime, db, case)
    runtime.__dict__.pop("_agent_records_cache", None)
    results = []
    operation_receipts = []
    for step in case["steps"]:
        if "status" in step:
            with runtime.db() as db:
                db.execute("UPDATE runtime_events SET status=?,error=? WHERE id='message'", (step["status"], step.get("error")))
            continue
        try:
            if "startOperation" in step:
                # Python has no out-of-transaction filesystem snapshot phase.
                # Start the operation immediately before its locked send, which
                # observes the same durable state as Rust's racing writer.
                with runtime.db() as db:
                    agent = runtime.agent("agent", db)
                    agent["workspaceOperation"] = step["startOperation"]
                    runtime.put(db, "agents", agent)
            result = endpoint(None, ManagedMessageBody.model_construct(room=step.get("room", "agent"), text=step.get("text", " hello "), id="message", delivery=step.get("delivery", "queue"), assets=step.get("assets", [])))
            results.append(result)
            if result["status"] == "queued" and not operation_receipts:
                # Rust additionally persists its initial admission receipt. Python
                # replays from runtime_events; validate this auxiliary row against
                # the actual Python first response and original request signature.
                operation_receipts.append({"id": "message", "signature": {"agent": step.get("room", "agent"), "text": step.get("text", " hello ").strip(), "assets": step.get("assets", [])}, "result": result})
        except ValueError as error:
            results.append({"error": str(error)})
    try:
        receipts = runtime.user_delivery_receipts("agent", ["message", "missing", "message"])
    except ValueError as error:
        receipts = {"error": str(error)}
    return {"results": results, "state": state(runtime.root), "operationReceipts": operation_receipts,
            "receipts": receipts}

with patch.object(codex_accounts, "codex_home", lambda: Path(os.environ["CODEX_HOME"])), \
     patch.object(codex_accounts, "auth_metadata", lambda *_: {"status": "signedOut", "accountId": None, "email": None, "plan": None}), \
     patch.object(concurrent.futures.ThreadPoolExecutor, "submit", lambda *_a, **_kw: concurrent.futures.Future()), \
     patch.object(threading.Thread, "start", lambda self: None), \
     patch.object(threading.Thread, "join", lambda self, *a, **k: None), \
     patch.object(codex_runtime, "_RuntimeWalKeeper", lambda *a: None):
    runtime = QuietRuntime(backend.state, lambda *_a, **_kw: None)
    original = runtime.create({"name": "Message ingress", "cwd": str(backend.cwd), "prompt": "seed"}, defer=True)
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_events")
        db.execute("DELETE FROM runtime_event_meta")
        db.execute("DELETE FROM runtime_agents")
        original.update(id="agent", rootId="agent", autoWake=False, status="idle", inFlight=False,
                        threadId=None, turnId=None, error=None)
        runtime.put(db, "agents", original)
    canvas = Canvas(runtime.root)
    canvas.runtime = runtime
    context = SimpleNamespace(runtime=runtime, canvas=canvas, send=lambda request, value: value)
    endpoint = next(route.endpoint for route in create_router(context).routes if route.path == "/api/messages" and "POST" in route.methods)
    template = sqlite3.connect(":memory:")
    with runtime.read_db() as db:
        db.backup(template)
    for line in sys.stdin:
        command = json.loads(line)
        if command.get("close"):
            break
        try:
            if command.get("configure"):
                from codex_sync_entities import register_functions
                with sqlite3.connect(Path(command["configure"]) / "canvas.sqlite3") as db:
                    register_functions(db)
                    configure(runtime, db, command["case"])
                result = {}
            elif command.get("snapshot"):
                result = state(command["snapshot"])
            else:
                result = run(runtime, template, endpoint, command)
            print(json.dumps(result), flush=True)
        except Exception:
            import traceback
            print(json.dumps({"oracleError": traceback.format_exc()}), flush=True)
    runtime.close()
    template.close()
