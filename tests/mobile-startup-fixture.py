#!/usr/bin/env python3
"""Production HTTP fixture with deterministic large work history, no user state."""
import base64
import json
import os
from pathlib import Path
import random
import runpy
import sys

sys.dont_write_bytecode = True
repo = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(repo / "scripts"))
import codex_canvas

original_make_server = codex_canvas.make_server
if os.environ.get("MOBILE_TEST_DIST"):
    codex_canvas.WEB = Path(os.environ["MOBILE_TEST_DIST"])


def with_history(canvas, *args, **kwargs):
    runtime = canvas.runtime
    random_bytes = random.Random(20260911)
    with runtime.lock, runtime.db() as db:
        lead = next(agent for agent in runtime.records(db, "agents")
                    if agent["name"] == "Release lead")
        for number in range(700):
            evidence = base64.b64encode(random_bytes.randbytes(6144)).decode()
            runtime.put(db, "work", {
                "id": f"mobile-perf-work-{number}", "rootId": lead["id"],
                "title": f"Retained task {number}", "owner": lead["id"],
                "description": f"Generated performance evidence {number}",
                "status": "review", "dependencies": [], "created": number,
                "updated": number, "version": 1, "decisions": [],
                "results": [{"id": f"result-{number}", "agent": lead["id"],
                             "text": evidence, "created": number}],
            })
    full = runtime.snapshot()
    work_bytes = len(json.dumps(full["work"]).encode())
    assert work_bytes > 4_000_000, work_bytes
    (canvas.root / "performance-fixture.json").write_text(json.dumps({
        "lead": lead["id"], "workBytes": work_bytes, "workCount": 700,
    }))
    return original_make_server(canvas, *args, **kwargs)


codex_canvas.make_server = with_history
runpy.run_path(str(repo / "tests/simple-ui-fixture.py"), run_name="__main__")
