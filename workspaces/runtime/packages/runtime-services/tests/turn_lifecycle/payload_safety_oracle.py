"""Write payload and safety corpus records through Python and return oracle results."""
import json
import sqlite3
import sys
import tempfile

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
with tempfile.TemporaryDirectory(prefix="rust-payload-oracle-") as state:
    for case in corpus["largeRecords"]:
        text = "x" * case["repeat"]
        value = [text] if case["table"] == "checkpoints" else {"text": text}
        record = {"id": case["name"], case["field"]: value}
        externalized = codex_payloads.externalize_record(state, db, case["table"], record)
        payload_results.append({"name": case["name"], "record": externalized,
                                "resolved": codex_payloads.resolve_record(state, externalized)})
    for case in corpus["results"]:
        text = "y" * case["repeat"]
        value = {"contentItems": [text]}
        externalized = codex_payloads.externalize_result(state, db, value)
        payload_results.append({"name": case["name"], "record": externalized,
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
            "accountKey": "acct", "stage": stage,
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
print(json.dumps({
    "payload": payload_results,
    "tasks": task_results,
    "safety": [{"record": record, "active": codex_safety_buffering.active(record)}
               for record in stored_records],
}, ensure_ascii=False, separators=(",", ":")))
