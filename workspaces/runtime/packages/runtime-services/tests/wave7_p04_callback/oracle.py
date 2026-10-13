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
lead = fixture["lead"]
full = []
for index, case in enumerate(base["answers"]):
    card = {**base["legacyCard"], "id":f"roundtrip-{index}", "rpcId":f"roundtrip-rpc-{index}", "method":case["method"], "params":case["params"]}
    with runtime.db() as db:
        runtime.put(db, "agents", lead)
        runtime.put(db, "requests", card)
    with patch("time.time", return_value=20.0):
        response = runtime.answer(card["id"], case["data"])
        replay = runtime.answer(card["id"], case["data"])
    with runtime.db() as db:
        after = json.loads(db.execute("SELECT record FROM runtime_requests WHERE id=?",(card["id"],)).fetchone()[0])
    full.append({"before":card,"data":case["data"],"after":after,"response":response,"replay":replay,"frame":runtime.replies[-1]})

deletes = []
for method in ["agent/asyncQuestion", "item/tool/requestUserInput", "mcpServer/elicitation/request"]:
    for status in ["pending", "answered", "resolved", "expired", "deleted", "answering", "uncertain"]:
        card = {**base["legacyCard"], "id":f"delete-{len(deletes)}", "rpcId":f"delete-rpc-{len(deletes)}", "method":method,"status":status,
                "params":{**base["legacyCard"]["params"],"mode":"form","questions":[],"requestedSchema":{"properties":{}}}}
        if status == "deleted": card["deletedAt"] = 10.0
        with runtime.db() as db:
            runtime.put(db, "agents", lead)
            runtime.put(db, "requests", card)
        replies_before = len(runtime.replies)
        try:
            with patch("time.time", return_value=20.0):
                response = runtime.delete_question(card["id"])
            expected = {"result":response}
        except ValueError as error:
            expected = {"error":str(error)}
        with runtime.db() as db:
            after = json.loads(db.execute("SELECT record FROM runtime_requests WHERE id=?",(card["id"],)).fetchone()[0])
        deletes.append({"before":card,"after":after,"expected":expected,"frames":runtime.replies[replies_before:]})
with runtime.db() as db:
    db.execute("DELETE FROM runtime_requests")
    runtime.put(db,"agents",lead)
base.update(fullAnswers=full,deletes=deletes)
print(json.dumps(base,ensure_ascii=False))
