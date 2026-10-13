"""Extend the accepted callback oracle with complete answer and deletion rows."""
import contextlib
import io
import json
import os
from pathlib import Path
import runpy
import sys
from unittest.mock import patch

HERE = Path(__file__).resolve()
sys.path.insert(0, str(HERE.parent.parent / "turn_lifecycle"))
from oracle_guard import install_guard

if "--isolated" not in sys.argv:
    install_guard()
    server = HERE.parents[4] / "apps/server"
    sys.path.insert(0, str(server / "tests"))
    from http_backend_harness import BackendProcess
    scratch = Path(sys.argv[1]).resolve(strict=True)
    owner = BackendProcess(scratch)
    binary = owner._directory("disabled-provider") / "codex"
    binary.write_text("#!/bin/sh\nexit 99\n")
    binary.chmod(0o700)
    owner._provider_executable = binary
    os.execve(sys.executable, [sys.executable, "-B", str(HERE), "--isolated", str(scratch)], owner._environment())

# This invokes the entire real fixture/oracle, never extracted AST statements.
# Its own guard is installed before application imports.
output = io.StringIO()
with contextlib.redirect_stdout(output):
    fixture = runpy.run_path(str(HERE.parent.parent / "provider_callbacks/oracle.py"))
base = json.loads(output.getvalue())
runtime = fixture["runtime"]
# The source oracle closed its runtime after seeding. These synchronous calls
# model an open HTTP runtime; Quiet still forbids provider connections and start.
runtime.closed = False
lead = fixture["lead"]
# Mirror the real HTTP backend's lazy transcript invalidation initialization.
from codex_sync import SyncStore
sync_store = SyncStore(runtime.db, lambda _agent: None, runtime)
sync_store._ensure_versions()

def snapshot():
    # Full logical write set: every application table, plus published entities.
    # Rust's protected transport outbox and sync sequence/hash bookkeeping have
    # no Python equivalent; the Rust tests assert their invariants separately.
    result = {}
    with runtime.db() as db:
        names = [row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
        for name in names:
            if name == "runtime_operation_receipts" or name.startswith("sync_") and name != "sync_entities":
                continue
            columns = "collection,id,payload,deleted" if name == "sync_entities" else "*"
            rows = []
            for row in db.execute(f'SELECT {columns} FROM "{name}"'):
                values = []
                for value in row:
                    if isinstance(value, str):
                        try: value = json.loads(value)
                        except (ValueError, TypeError): pass
                    if isinstance(value, bytes): value = {"blobHex": value.hex()}
                    values.append(value)
                rows.append(values)
            result[name] = sorted(rows, key=lambda row: json.dumps(row, sort_keys=True))
    return result


def changes(before, after):
    result = {}
    for table in before.keys() | after.keys():
        if table == "runtime_agent_record_generation":
            delta = after[table][0][1] - before[table][0][1]
            if delta: result[table] = {"increment": delta}
            continue
        old = {json.dumps(row, sort_keys=True): row for row in before.get(table, [])}
        new = {json.dumps(row, sort_keys=True): row for row in after.get(table, [])}
        removed = [old[key] for key in sorted(old.keys() - new.keys())]
        added = [new[key] for key in sorted(new.keys() - old.keys())]
        if removed or added: result[table] = {"removed": removed, "added": added}
    return result


def seeded(card):
    with runtime.db() as db:
        runtime.put(db, "agents", {**lead, "status": "approval"})
        runtime.put(db, "requests", card)
    return snapshot()


def result_rows(card, before):
    with runtime.db() as db:
        after = json.loads(db.execute("SELECT record FROM runtime_requests WHERE id=?", (card["id"],)).fetchone()[0])
        actor = runtime.agent(lead["id"], db)
    return {"after": after, "actor": {**lead, "status": "approval"}, "afterActor": actor, "writes": changes(before, snapshot()), "counters": {table: before[table] for table in ["runtime_agent_record_generation", "runtime_scheduler_agent_change_state", "runtime_scheduler_agent_changes", "runtime_transcript_revisions"]}}


full = []
for index, case in enumerate(base["answers"]):
    card = {**base["legacyCard"], "id": f"roundtrip-{index}", "rpcId": f"roundtrip-rpc-{index}", "method": case["method"], "params": case["params"]}
    before = seeded(card)
    with patch("time.time", return_value=20.0):
        response = runtime.answer(card["id"], case["data"])
        replay = runtime.answer(card["id"], case["data"])
    full.append({"before": card, "data": case["data"], **result_rows(card, before), "response": response, "replay": replay, "frame": runtime.replies[-1]})

deletes = []
for method in ["agent/asyncQuestion", "item/tool/requestUserInput", "mcpServer/elicitation/request"]:
    for status in ["pending", "answered", "resolved", "expired", "deleted", "answering", "uncertain"]:
        card = {**base["legacyCard"], "id": f"delete-{len(deletes)}", "rpcId": f"delete-rpc-{len(deletes)}", "method": method, "status": status,
                "params": {**base["legacyCard"]["params"], "mode": "form", "questions": [], "requestedSchema": {"properties": {}}}}
        if status == "deleted": card["deletedAt"] = 10.0
        before = seeded(card)
        replies_before = len(runtime.replies)
        try:
            with patch("time.time", return_value=20.0):
                response = runtime.delete_question(card["id"])
            expected = {"result": response}
        except ValueError as error:
            expected = {"error": str(error)}
        deletes.append({"before": card, **result_rows(card, before), "expected": expected, "frames": runtime.replies[replies_before:]})

from codex_runtime import SubmissionRejected
failures = []
for operation in ["answer", "delete"]:
    for outcome in ["notSent", "unknown"]:
        card = {**base["legacyCard"], "id": f"failure-{operation}-{outcome}", "rpcId": f"failure-rpc-{operation}-{outcome}",
                "method": "item/tool/requestUserInput", "params": {**base["legacyCard"]["params"], "questions": []}}
        before = seeded(card)
        error = SubmissionRejected("not sent") if outcome == "notSent" else RuntimeError("lost receipt")
        with patch.object(runtime, "reply", side_effect=error), patch("time.time", return_value=20.0):
            try:
                if operation == "answer": runtime.answer(card["id"], {"answers": {}})
                else: runtime.delete_question(card["id"])
                raise AssertionError("delivery unexpectedly succeeded")
            except type(error): pass
        failures.append({"before": card, "operation": operation, "outcome": outcome, **result_rows(card, before)})

invalid = []
for method in ["item/commandExecution/requestApproval", "item/fileChange/requestApproval", "mcpServer/elicitation/request", "item/tool/requestUserInput"]:
    choices = [{}, {"answers": None}, {"answers": {}}] if method == "item/tool/requestUserInput" else [{}, {"decision": None}, {"decision": "invalid"}]
    for data in choices:
        card = {**base["legacyCard"], "id": f"invalid-{len(invalid)}", "rpcId": f"invalid-rpc-{len(invalid)}", "method": method}
        before = seeded(card)
        try:
            with patch("time.time", return_value=20.0): response = runtime.answer(card["id"], data)
            expected = {"result": response}
        except ValueError as error: expected = {"error": str(error)}
        invalid.append({"before": card, "data": data, "expected": expected, **result_rows(card, before)})

# Python historically overwrites completion/resolution during reply(). The
# reviewed Rust contract deliberately preserves the durable winning transition.
# Capture both the actual race winner and Python's subsequent overwrite.
from native_notifications.dispatch import consume_native_notification
for case in base["resolved"]:
    with runtime.db() as db:
        db.execute("DELETE FROM runtime_requests")
        runtime.put(db, "agents", case["actor"])
        runtime.put(db, "requests", case["card"])
    before = snapshot()
    case["before"] = case["card"]
    case["counters"] = {table: before[table] for table in [
        "runtime_agent_record_generation",
        "runtime_scheduler_agent_change_state",
        "runtime_scheduler_agent_changes",
        "runtime_transcript_revisions",
    ]}
    with patch("time.time", return_value=20.0):
        consume_native_notification(runtime, {"method":"serverRequest/resolved","params":{"threadId":case["actor"]["threadId"],"requestId":case["card"]["rpcId"]}}, "default", "connection")
    case["writes"] = changes(before, snapshot())
races = []
for outcome in ["completed", "failed", "resolved"]:
    card = {**base["legacyCard"], "id": f"race-{outcome}", "rpcId": f"race-rpc-{outcome}", "method": "item/tool/requestUserInput", "params": {**base["legacyCard"]["params"], "questions": []}}
    before = seeded(card)
    with runtime.db() as db: before_actor = runtime.agent(lead["id"], db)
    winner = {}
    def during_reply(*args, **kwargs):
        if outcome == "resolved":
            before_resolution = snapshot()
            consume_native_notification(runtime, {"method": "serverRequest/resolved", "params": {"threadId": lead["threadId"], "requestId": card["rpcId"]}}, "default", "connection")
            resolution_writes = changes(before_resolution, snapshot())
        else:
            with runtime.db() as db: runtime.put(db, "agents", {**lead, "status": outcome, "turnId": None, "inFlight": False})
            resolution_writes = {}
        winner.update(result_rows(card, before))
        winner["resolutionWrites"] = resolution_writes
    with patch.object(runtime, "reply", during_reply), patch("time.time", return_value=20.0):
        response = runtime.answer(card["id"], {"answers": {}})
    races.append({"before": card, "beforeActor": before_actor, "outcome": outcome, "winner": winner, "pythonAfter": result_rows(card, before), "response": response})
with runtime.db() as db:
    db.execute("DELETE FROM runtime_requests")
    runtime.put(db,"agents",lead)
sync_store._version_reader.close()
del sync_store._version_reader
runtime.closed = True
import sqlite3
from contextlib import closing
with closing(sqlite3.connect(Path(base["template"]) / "canvas.sqlite3")) as db:
    db.execute("PRAGMA wal_checkpoint(TRUNCATE)")
base.update(fullAnswers=full,deletes=deletes,failures=failures,invalid=invalid,races=races)
print(json.dumps(base,ensure_ascii=False))
