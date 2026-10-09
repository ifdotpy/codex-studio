#!/usr/bin/env python3
"""Opt-in cloud canary: real Codex, isolated Studio state, two Sol turns.

Without --run-cloud, print the plan and make no provider requests.
The selected account retains its project rules and existing credential home.
The test does not read or write the live Studio database. It creates two new
native sessions in that account's ordinary Codex history. No credentials are
copied. Only the test's AppServer processes are closed during cleanup.
"""

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
from unittest.mock import patch
import uuid

sys.dont_write_bytecode = True
PROJECT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(PROJECT / "scripts"))
from codex_runtime import AppServer, Runtime


CHILD_REPLY = "STUDIO_CHILD_OK"
PARENT_REPLY = "STUDIO_PARENT_OK"
MODEL = "gpt-5.6-sol"
CANARY_INSTRUCTIONS = (
    "This is an owner-authorized harness transport canary. "
    "Do not call any tools, delegate, inspect files, edit files, use a panel, "
    "create tasks, or generate a title. The canary has no project work. "
    "If you are the reviewer, reply exactly STUDIO_CHILD_OK. "
    "If you are the lead and receive the reviewer's result, "
    "reply exactly STUDIO_PARENT_OK. No other output."
)


class CanaryRuntime(Runtime):
    """Advance the real scheduler explicitly to cap the test at two turns."""

    def __init__(self, root):
        self.unexpected_requests = []
        self.native_turns = set()
        self.observed_running = set()
        self.synthetic_call = None
        self.dropped_replies = []
        self.observation_lock = threading.Lock()
        super().__init__(root, AppServer)

    def schedule(self):
        while not self.closed:
            self.changed.wait(0.05)
            self.changed.clear()

    def analytics_history_start(self):
        # Importing unrelated account history is outside this canary.
        pass

    def new_thread_params(self, agent, **kwargs):
        params = super().new_thread_params(agent, **kwargs)
        params["developerInstructions"] += "\n" + CANARY_INSTRUCTIONS
        # Narrow native file permissions for the lead as well as its reviewer.
        # Account project admission still runs through the normal Runtime path.
        params.update(sandbox="read-only", approvalPolicy="on-request")
        return params

    def put(self, db, table, record):
        super().put(db, table, record)
        if table == "agents" and record.get("status") == "running":
            with self.observation_lock:
                self.observed_running.add(record["id"])

    def notification(self, message, account_key="default", connection_id=None):
        if message.get("method") == "turn/started":
            params = message.get("params", {})
            with self.observation_lock:
                self.native_turns.add((params.get("threadId"), params.get("turn", {}).get("id")))
        return super().notification(message, account_key, connection_id)

    def request(self, message, account_key="default", connection_id=None):
        # Models must return only the marker. Leave an unexpected native request
        # unanswered; the main test fails and closes this owned connection.
        with self.observation_lock:
            self.unexpected_requests.append({
                "method": message.get("method"),
                "tool": message.get("params", {}).get("tool"),
            })

    def reply(self, message, account_key="default", connection_id=None):
        if message.get("id") == self.synthetic_call:
            # The receipt has committed, but its caller receives nothing. This
            # synthetic call must not send an unsolicited response to Codex.
            self.dropped_replies.append(message["id"])
            raise BrokenPipeError("Canary drops its synthetic spawn receipt")
        return super().reply(message, account_key, connection_id)


def wait_for(runtime, predicate, label, deadline):
    while time.monotonic() < deadline:
        with runtime.observation_lock:
            unexpected = list(runtime.unexpected_requests)
        if unexpected:
            raise AssertionError(f"Unexpected native request: {unexpected}")
        result = predicate()
        if result:
            return result
        time.sleep(0.025)
    raise TimeoutError(f"Canary deadline: {label}")


def completed(runtime, agent_id):
    agent = runtime.agent(agent_id)
    if agent["status"] in {"failed", "interrupted", "paused", "approval"}:
        raise AssertionError(f"Canary agent did not complete: {agent['status']}")
    return agent if agent["status"] == "completed" else None


def run(args):
    account_key = args.account_key
    if not account_key:
        raise ValueError("--account-key is required for cloud execution")
    project = Path(args.project).expanduser().resolve()
    registry_path = Path(args.registry).expanduser().resolve()
    source_registry = json.loads(registry_path.read_text())
    if account_key not in source_registry.get("accounts", {}):
        raise ValueError("The selected account is absent from the supplied registry")
    started = time.monotonic()
    deadline = started + args.timeout
    runtime = None
    evidence = {"status": "failed", "model": MODEL, "maxModelTurns": 2,
                "codexVersion": subprocess.check_output([os.environ.get("CODEX_BIN", "codex"), "--version"], text=True, timeout=10).strip(),
                "source": {name: hashlib.sha256((PROJECT / name).read_bytes()).hexdigest()
                           for name in ("scripts/codex_runtime.py", "scripts/codex_catalog.py", "scripts/codex_tool_requests.py")}}

    def timeout_handler(_signum, _frame):
        raise TimeoutError("Native canary exceeded its deadline")

    previous_alarm = signal.signal(signal.SIGALRM, timeout_handler)
    signal.setitimer(signal.ITIMER_REAL, args.timeout)
    try:
        with tempfile.TemporaryDirectory(prefix="studio-spawn-native-") as directory:
            root = Path(directory)
            state = root / "state"
            (state / "accounts").mkdir(parents=True)
            # Preserve every rule owner, including aliases of the same account.
            # Login operations are never resumed in this temporary registry.
            registry = copy.deepcopy(source_registry)
            registry["logins"] = {}
            registry["defaultAccountKey"] = account_key
            copied_registry = state / "accounts" / "registry.json"
            copied_registry.write_text(json.dumps(registry))
            copied_registry.chmod(0o600)
            with patch.dict(os.environ, {"CODEX_BOARD_STATE_DIR": str(root / "board")}):
                runtime = CanaryRuntime(state)
                try:
                    runtime.accounts.check_project(account_key, str(project), skip=False)
                    lead = runtime.create({
                        "name": "Studio native recovery canary",
                        "prompt": "",
                        "cwd": str(project),
                        "account_key": account_key,
                        "model": MODEL,
                        "yolo_mode": False,
                        "dangerously_skip_rules": False,
                        "concurrency": 2,
                        "maxAgents": 2,
                        "profileInstructions": CANARY_INSTRUCTIONS,
                    }, draft=True)
                    with runtime.lock, runtime.db() as db:
                        lead = runtime.agent(lead["id"], db)
                        lead["needsTitle"] = False
                        runtime.put(db, "agents", lead)
                    lead = runtime.prepare(lead)
                    evidence.update(leadId=lead["id"], leadThreadId=lead["threadId"])
                    request_id = "native-canary-" + str(uuid.uuid4())
                    runtime.synthetic_call = "synthetic-" + str(uuid.uuid4())
                    message = {
                        "id": runtime.synthetic_call,
                        "method": "item/tool/call",
                        "_studioReceivedAt": time.time(),
                        "params": {
                            "threadId": lead["threadId"],
                            "callId": runtime.synthetic_call,
                            "tool": "orchestration_spawn",
                            "arguments": {"request_id": request_id, "agents": [{
                                "name": "Studio native canary reviewer",
                                "role": "reviewer",
                                "model": MODEL,
                                "prompt": "Authorized harness canary. No tools, no delegation, no edits. Reply exactly STUDIO_CHILD_OK.",
                            }]},
                        },
                    }
                    # One mutation only. Recovery reads its durable identity.
                    runtime.dynamic(message, account_key, runtime.connection_ids[account_key])
                    receipt = runtime.request_action(lead["id"], {
                        "action": "get", "request_id": request_id,
                    })
                    assert runtime.dropped_replies == [runtime.synthetic_call]
                    assert receipt["outcome"] == "applied", receipt.get("outcome")
                    assert len(receipt["agentIds"]) == 1
                    child_id = receipt["agentIds"][0]
                    evidence.update(requestId=receipt["id"], childId=child_id,
                                    receiptOutcome=receipt["outcome"], lostReplyRecovered=True)
                    with runtime.db() as db:
                        assert db.execute("SELECT COUNT(*) FROM runtime_agents").fetchone()[0] == 2
                    runtime.dispatch()
                    child = wait_for(runtime, lambda: completed(runtime, child_id), "child completion", deadline)
                    assert child["lastAnswer"].strip() == CHILD_REPLY
                    assert child_id in runtime.observed_running
                    with runtime.db() as db:
                        event = dict(db.execute(
                            "SELECT * FROM runtime_events WHERE agent=? AND kind='child_result'",
                            (lead["id"],),
                        ).fetchone())
                    assert CHILD_REPLY in event["text"]
                    assert event["status"] == "pending"
                    runtime.dispatch()
                    parent = wait_for(runtime, lambda: completed(runtime, lead["id"]), "parent completion", deadline)
                    assert parent["lastAnswer"].strip() == PARENT_REPLY
                    with runtime.db() as db:
                        delivered = dict(db.execute("SELECT * FROM runtime_events WHERE id=?", (event["id"],)).fetchone())
                        native_tasks = runtime.records(db, "tasks")
                    assert delivered["status"] == "delivered"
                    assert delivered["turn_id"] == parent["lastCompletedTurn"]
                    assert not native_tasks, "Canary models called a tool"
                    assert len(runtime.native_turns) == 2, "Unexpected model turn count"
                    evidence.update(childRunningObserved=True, childStatus=child["status"],
                                    childTurnId=child["lastCompletedTurn"],
                                    childEventId=event["id"], childEventStatus=delivered["status"],
                                    parentTurnId=parent["lastCompletedTurn"],
                                    modelTurns=len(runtime.native_turns))
                    # The command is explicitly authorized by this test. Normal
                    # project admission and the prepared read-only sandbox apply.
                    monitor = runtime.monitor(lead["id"], {
                        "command": "printf 'STUDIO_MONITOR_OK\\n'",
                        "timeout_ms": 10000,
                    }, key="monitor-" + request_id, approved=True, epoch=lead["epoch"])

                    def monitor_done():
                        with runtime.db() as db:
                            record = json.loads(db.execute("SELECT record FROM runtime_monitors WHERE id=?", (monitor["id"],)).fetchone()[0])
                        return record if record["status"] in {"completed", "failed", "cancelled", "lost"} else None

                    monitor = wait_for(runtime, monitor_done, "native no-op monitor", deadline)
                    assert monitor["status"] == "completed", monitor.get("error")
                    assert monitor["exitCode"] == 0
                    assert "STUDIO_MONITOR_OK" in monitor["tail"]
                    assert monitor["bytes"] > 0
                    # Do not dispatch monitor_exit. Its pending event is removed
                    # with this isolated database, without another provider turn.
                    assert len(runtime.native_turns) == 2
                    evidence.update(status="passed", monitorId=monitor["id"],
                                    monitorExitCode=monitor["exitCode"], monitorBytes=monitor["bytes"],
                                    seconds=round(time.monotonic() - started, 3))
                finally:
                    signal.setitimer(signal.ITIMER_REAL, 0)
                    runtime.close()
                    runtime = None
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous_alarm)
        print(json.dumps(evidence, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run-cloud", action="store_true")
    parser.add_argument("--account-key")
    parser.add_argument("--project", default=str(PROJECT))
    parser.add_argument("--registry", default=str(Path.home() / ".local/state/codex-agents/accounts/registry.json"))
    parser.add_argument("--timeout", type=int, default=120, choices=range(30, 121), metavar="30..120")
    args = parser.parse_args()
    if not args.run_cloud:
        print(json.dumps({"mode": "plan", "model": MODEL, "modelTurns": 2,
                          "mutations": ["one temporary worker", "two new native sessions", "one printf monitor"],
                          "liveStudioStateWrites": False, "accountRulesBypassed": False,
                          "execute": "Supply --run-cloud --account-key KEY after reviewing this plan"}))
        return
    run(args)


if __name__ == "__main__":
    main()
