"""Write payload and safety corpus records through Python and return oracle results."""
import json
import os
import sqlite3
import sys
from contextlib import contextmanager
from threading import RLock

from oracle_guard import install_guard

install_guard(block_processes=True)

sys.path.insert(0, sys.argv[2])
import codex_payloads
import codex_safety_buffering

with open(sys.argv[1], encoding="utf-8") as stream:
    corpus = json.load(stream)

db = sqlite3.connect(":memory:")
codex_payloads.ensure_payload_schema(db)
payload_results = []
state = sys.argv[3]
large_cases = list(corpus["largeRecords"])
result_cases = list(corpus["results"])
if os.environ.get("STUDIO_RUNTIME_CORPUS_FULL") == "1":
    large_cases.extend([
        {"name": "full_checkpoint", "table": "checkpoints", "field": "items", "repeat": 131072},
        {"name": "full_tool_request", "table": "tool_requests", "field": "result", "repeat": 9000,
         "objectKeys": ["key9", "key2", "key7", "key1", "key8", "key3", "key6", "key4", "key5"]},
    ])
    result_cases.append({"name": "full_result_content", "repeat": 140000})
for case in large_cases:
    text = "x" * case["repeat"]
    if "objectKeys" in case:
        value = {"contentItems": [{key: "z" * case["repeat"] for key in case["objectKeys"]}]}
    else:
        value = [text] if case["table"] == "checkpoints" else {"text": text}
    record = {"id": case["name"], case["field"]: value}
    input_json = json.dumps(record, ensure_ascii=False, separators=(",", ":"))
    externalized = codex_payloads.externalize_record(state, db, case["table"], record)
    payload_results.append({"name": case["name"], "table": case["table"], "inputJson": input_json,
                            "record": externalized,
                            "resolved": codex_payloads.resolve_record(state, externalized)})
for case in result_cases:
    text = "y" * case["repeat"]
    value = {"contentItems": [text]}
    input_json = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    externalized = codex_payloads.externalize_result(state, db, value)
    payload_results.append({"name": case["name"], "table": "results", "inputJson": input_json,
                            "record": externalized,
                            "resolved": codex_payloads.resolve_result(state, externalized)})

task_results = []
for case in corpus["tasks"]:
    record = {"tail": "a" * case["prefix"] + case["suffix"]}
    task_results.append({"name": case["name"], "input": record,
                         "trimmed": codex_payloads.trim_task(record)})

safety_records = []
for index, stage in enumerate(corpus["safetyStages"]):
    record = {
        "id": f"agent-{index}", "epoch": 4, "threadId": "thread", "turnId": "turn",
        "accountKey": "acct", "inFlight": True, "autoWake": True,
        "nativeSafetyBuffering": {
            "threadId": "thread", "turnId": "turn", "connectionId": "connection",
            "showBufferingUi": True, "fasterModel": "model",
        },
        "nativeSafetyRetry": {
            "id": f"agent-{index}:turn", "agent": f"agent-{index}", "epoch": 4,
            "turnId": "turn", "accountKey": "acct", "stage": stage,
        },
    }
    if stage == "turns":
        record["nativeSafetyRetry"]["accountKey"] = "foreign"
    safety_records.append(record)
db.execute("CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
db.executemany(
    "INSERT INTO runtime_agents VALUES (?, ?)",
    [(record["id"], json.dumps(record, ensure_ascii=False, separators=(",", ":")))
     for record in safety_records],
)
stored_records = [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_agents ORDER BY id")]


class FakeRuntime:
    def __init__(self, agent):
        self.agent_record = agent
        self.lock = RLock()
        self.tables = {}

    @contextmanager
    def db(self):
        yield db

    def checked_actor(self, _db, key):
        assert key == self.agent_record["id"]
        return self.agent_record

    def connection_current(self, *_args):
        return True

    def put(self, _db, table, record):
        self.tables.setdefault(table, {})[record["id"]] = json.loads(json.dumps(record))

    def touch_ui(self, _key):
        pass

    def assert_workspace_available(self, *_args):
        pass

    def child_stopped_event(self, *_args):
        pass


def run_action(choice, agent_record, existing=None):
    runtime = FakeRuntime(json.loads(json.dumps(agent_record)))
    if existing is not None:
        db.execute("CREATE TABLE IF NOT EXISTS runtime_safety_retries (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
        db.execute("INSERT OR REPLACE INTO runtime_safety_retries VALUES (?, ?)",
                   (existing["id"], json.dumps(existing, separators=(",", ":"))))
    old_assert = codex_safety_buffering.assert_native_thread_open
    old_issue = codex_safety_buffering.issue
    old_time = codex_safety_buffering.time.time
    old_uuid = codex_safety_buffering.uuid.uuid4
    codex_safety_buffering.assert_native_thread_open = lambda _agent: None
    codex_safety_buffering.issue = lambda *_args: None
    codex_safety_buffering.time.time = lambda: 123.5
    codex_safety_buffering.uuid.uuid4 = lambda: "attempt-fixed"
    try:
        result = codex_safety_buffering.action(
            runtime, agent_record["id"], {"safety": choice, "turnId": "turn"})
        return {"result": result, "agent": runtime.agent_record,
                "operation": runtime.tables.get("safety_retries", {}).get(agent_record["id"] + ":turn")}
    except ValueError as error:
        return {"error": str(error)}
    finally:
        codex_safety_buffering.assert_native_thread_open = old_assert
        codex_safety_buffering.issue = old_issue
        codex_safety_buffering.time.time = old_time
        codex_safety_buffering.uuid.uuid4 = old_uuid


action_agent = json.loads(json.dumps(next(record for record in stored_records
                                         if record["id"] == "agent-5")))
action_agent["nativeSafetyRetry"] = None
wait_result = run_action("wait", action_agent)
retry_result = run_action("retry", action_agent)
existing_receipt = {
    "id": "agent-5:turn", "agent": "agent-5", "epoch": 4, "turnId": "turn",
    "accountKey": "acct", "model": "model", "stage": "fork", "created": 12.0,
    "updated": 12.0, "private": "not-public",
}
replay_result = run_action("retry", action_agent, existing_receipt)
cancel_result = run_action("cancel", action_agent, existing_receipt)

restart_cases = []
for stage in ("fork", "running", "failed", "cancelled"):
    agent_record = json.loads(json.dumps(action_agent))
    receipt = {"id": "agent-5:turn", "stage": stage, "epoch": 4, "accountKey": "acct"}
    agent_record["nativeSafetyRetry"] = receipt
    input_agent = json.loads(json.dumps(agent_record))
    runtime = FakeRuntime(agent_record)
    saved = {"id": receipt["id"], "agent": agent_record["id"], "stage": stage,
             "epoch": 4, "accountKey": "acct", "turnId": "turn"}
    db.execute("CREATE TABLE IF NOT EXISTS runtime_safety_retries (id TEXT PRIMARY KEY, record TEXT NOT NULL)")
    db.execute("INSERT OR REPLACE INTO runtime_safety_retries VALUES (?, ?)",
               (saved["id"], json.dumps(saved, separators=(",", ":"))))
    old_time = codex_safety_buffering.time.time
    codex_safety_buffering.time.time = lambda: 123.5
    try:
        restarted = codex_safety_buffering.recover_restart(runtime, db, agent_record)
    finally:
        codex_safety_buffering.time.time = old_time
    restart_cases.append({"stage": stage, "restarted": restarted,
                          "inputAgent": input_agent, "agent": agent_record,
                          "operation": runtime.tables.get("safety_retries", {}).get(receipt["id"]),
                          "saved": saved})
print(json.dumps({
    "payload": payload_results,
    "tasks": task_results,
    "safety": [{"record": record, "active": codex_safety_buffering.active(record)}
               for record in stored_records],
    "actions": {"wait": wait_result, "retry": retry_result, "replay": replay_result,
                "cancel": cancel_result},
    "restarts": restart_cases,
}, ensure_ascii=False, separators=(",", ":")))
