"""Real Runtime.send/receipts on Python-written seed backups; no provider or timer."""
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).absolute().parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard
_GUARD = install_guard()

import concurrent.futures
import copy
import json
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
        return {"events": events, "metadata": metadata,
                "agents": {key: json.loads(raw) for key, raw in db.execute("SELECT id,record FROM runtime_agents ORDER BY id")}}


def run(runtime, template, command):
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
    # Live workspace operations start after store startup recovery has run.
    with runtime.db() as db:
        agent = runtime.agent("agent", db)
        agent.update(case.get("agent", {}))
        runtime.put(db, "agents", agent)
    runtime.__dict__.pop("_agent_records_cache", None)
    results = []
    for step in case["steps"]:
        if "status" in step:
            with runtime.db() as db:
                db.execute("UPDATE runtime_events SET status=?,error=? WHERE id='message'", (step["status"], step.get("error")))
            continue
        try:
            results.append(runtime.send("agent", step.get("text", " hello "), "message",
                                        delivery=step.get("delivery", "queue"), assets=step.get("assets", [])))
        except ValueError as error:
            results.append({"error": str(error)})
    return {"results": results, "state": state(runtime.root),
            "receipts": runtime.user_delivery_receipts("agent", ["message", "missing", "message"])}

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
    template = sqlite3.connect(":memory:")
    with runtime.read_db() as db:
        db.backup(template)
    for line in sys.stdin:
        command = json.loads(line)
        if command.get("close"):
            break
        try:
            if command.get("snapshot"):
                result = state(command["snapshot"])
            else:
                result = run(runtime, template, command)
            print(json.dumps(result), flush=True)
        except Exception:
            import traceback
            print(json.dumps({"oracleError": traceback.format_exc()}), flush=True)
    runtime.close()
    template.close()
