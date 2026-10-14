"""Real Runtime constructor oracle for startup request and monitor recovery."""
from __future__ import annotations

import json
import copy
import os
from pathlib import Path
import runpy
import sqlite3
import sys
import threading
from unittest.mock import patch

HERE = Path(__file__).resolve()
sys.path.insert(0, str(HERE.parents[1] / "turn_lifecycle"))
from oracle_guard import install_guard

# Runtime startup needs the isolated Node crypto helper to initialize its local
# server identity. Provider calls are supplied by ScriptedProvider below.
install_guard()
server_root = HERE.parents[4] / "apps" / "server"
sys.path.insert(0, str(server_root / "tests"))
sys.path.insert(0, str(server_root / "src"))
fixture = runpy.run_path(str(server_root / "tests" / "runtime-contract.py"))
Runtime = fixture["Runtime"]
FakeServer = fixture["FakeServer"]
make_runtime = __import__("runtime_test_environment").make_runtime
STATE = Path(sys.argv[2]).resolve()
NOW = 2_000_000_000.0
WAIT_SECONDS = 30


class QuietRuntime(Runtime):
    def schedule(self):
        pass

    def schedule_fast_dispatch(self, *_args, **_kwargs):
        pass

    def start_review_archives(self):
        pass


class ScriptedProvider(FakeServer):
    turn_started = threading.Event()
    command_started = threading.Event()
    release_command = threading.Event()

    def call(self, method, params, timeout=60):
        if method == "turn/start":
            result = super().call(method, params, timeout)
            type(self).turn_started.set()
            return result
        if method == "command/exec":
            self.calls.append((method, params))
            type(self).command_started.set()
            if not type(self).release_command.wait(WAIT_SECONDS):
                raise RuntimeError("scripted monitor provider timed out")
            return {"exitCode": 0, "stdout": "", "stderr": ""}
        return super().call(method, params, timeout)


def snapshot(runtime):
    result = {"agents": [], "requests": [], "monitors": [], "events": [], "sync_entities": []}
    with runtime.db() as db:
        for key, table in (("agents", "runtime_agents"), ("requests", "runtime_requests"),
                           ("monitors", "runtime_monitors")):
            result[key] = [json.loads(row[0]) for row in db.execute(
                f"SELECT record FROM {table} ORDER BY id")]
        for row in db.execute(
            "SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id"):
            try:
                event_text = json.loads(row[3])
            except (TypeError, ValueError):
                event_text = row[3]
            result["events"].append({
                "id": row[0], "agent": row[1], "kind": row[2], "text": event_text,
                "status": row[4], "created": row[5], "epoch": row[6],
                "turn_id": row[7], "error": row[8],
            })
        result["sync_entities"] = [
            {"collection": row[0], "id": row[1], "payload": json.loads(row[2]), "deleted": row[3]}
            for row in db.execute("SELECT collection,id,payload,deleted FROM sync_entities ORDER BY collection,id")
        ]
    return result


if sys.argv[1] == "seed":
    ScriptedProvider.turn_started.clear()
    ScriptedProvider.command_started.clear()
    ScriptedProvider.release_command.clear()
    with patch("time.time", return_value=NOW):
        runtime = make_runtime(QuietRuntime, STATE, ScriptedProvider)
        lead = runtime.create({"name": "Lead", "cwd": str(STATE), "prompt": "Persist a turn"})
        runtime.dispatch_all(maintenance=False)
        if not ScriptedProvider.turn_started.wait(WAIT_SECONDS):
            raise RuntimeError("scripted provider did not start the turn")
        lead = runtime.agent(lead["id"])
        runtime.notification({"method": "item/completed", "params": {
            "threadId": lead["threadId"], "turnId": lead.get("turnId"),
            "item": {"type": "agentMessage", "id": "retained-question", "text": "Choose",
                     "questions": [{"title": "Keep this question", "options": ["yes", "no"]}]}}})
        runtime.request({"id": "provider-approval", "method": "item/commandExecution/requestApproval",
                         "params": {"threadId": lead["threadId"], "turnId": lead.get("turnId"),
                                    "itemId": "approval-item", "command": "printf safe"}})
        runtime.monitor(lead["id"], {"command": "printf needs approval"}, key="pending-monitor")
        runtime.server.request({"id": "orphan-request", "method": "mcpServer/elicitation/request",
                                "params": {"mode": "form", "requestedSchema": {"properties": {}}}})
        runtime.monitor(lead["id"], {"command": "printf running"}, key="running-monitor", approved=True)
        if not ScriptedProvider.command_started.wait(WAIT_SECONDS):
            raise RuntimeError("scripted provider did not persist a running monitor")
        with runtime.lock, runtime.db() as db:
            active = next(m for m in runtime.records(db, "monitors")
                          if m.get("status") == "running" and m.get("command") == "printf running")
            rule_monitor = copy.deepcopy(active)
            rule_monitor.update(id="rule-linked-monitor", ruleId="rule-1")
            runtime.put(db, "monitors", rule_monitor)
    # Simulate process loss with the exact submitted monitor still in running state.
    os._exit(0)

if sys.argv[1] == "recover":
    with patch("time.time", return_value=NOW):
        runtime = make_runtime(QuietRuntime, STATE, ScriptedProvider)
        print(json.dumps(snapshot(runtime), sort_keys=True))
        runtime.close()
else:
    raise ValueError("unknown oracle command")
