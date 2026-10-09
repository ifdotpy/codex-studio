#!/usr/bin/env python3
"""Production HTTP fixture with deterministic large work history, no user state."""
from codex_layout import REPOSITORY_ROOT, SERVER_SOURCE_ROOT, SERVER_TESTS_ROOT

import base64
import json
import os
from pathlib import Path
import random
import runpy
import sys
import time
import uuid

sys.dont_write_bytecode = True
repo = REPOSITORY_ROOT
sys.path.insert(0, str(SERVER_TESTS_ROOT))
from test_isolation import isolate_api_schema_cache
isolate_api_schema_cache()
sys.path.insert(0, str(SERVER_SOURCE_ROOT))
from studio_api.testing import read_runtime_state
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
        template = next(agent for agent in runtime.records(db, "agents")
                        if agent.get("parentId") == lead["id"])
        # Keep the scale fixture in this isolated performance server only.
        # Runtime.create enforces product team limits, so seed the DTO rows via
        # Runtime.put, the same projection path used by normal entity sync.
        current_agents = db.execute("SELECT count(*) FROM runtime_agents").fetchone()[0]
        target_agents = 1500
        switch_targets = []
        for number in range(max(0, target_agents - current_agents)):
            agent_id = str(uuid.uuid5(uuid.NAMESPACE_URL, f"studio-mobile-agent-{number}"))
            agent = {
                **template,
                "id": agent_id,
                "name": f"Mobile agent {number:04}",
                "parentId": None if number < 2 else lead["id"],
                "rootId": agent_id if number < 2 else lead["id"],
                "isLead": number < 2,
                "created": time.time() - number,
                "updated": time.time() - number,
                "status": "completed",
                "turnId": None,
                "inFlight": False,
                "lastAnswer": "Fixture agent history.",
            }
            runtime.put(db, "agents", agent)
            if number < 2:
                switch_targets.append({"id": agent_id, "name": agent["name"]})
        entity_count = db.execute("SELECT count(*) FROM sync_entities").fetchone()[0]
        for number in range(max(0, 2000 - entity_count)):
            runtime.put(db, "rules", {
                "id": f"mobile-perf-rule-{number}", "agent": lead["id"],
                "name": f"Fixture rule {number}", "enabled": True,
                "description": "Generated mobile performance entity.",
            })
        # The lead chat includes more than 1,000 persisted transcript rows.
        for number in range(1500):
            runtime.item(
                db, lead["id"], f"mobile-perf-message-{number:04}",
                "user" if number % 2 == 0 else "assistant",
                f"Fixture transcript message {number:04}. "
                + "A realistic saved message for mobile history scrolling. " * 3,
                title="You" if number % 2 == 0 else "Agent",
                index_search=False,
            )
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
    full = read_runtime_state(runtime)
    work_bytes = len(json.dumps(full["work"]).encode())
    assert work_bytes > 4_000_000, work_bytes
    with runtime.db() as db:
        agent_count = db.execute("SELECT count(*) FROM runtime_agents").fetchone()[0]
        entity_count = db.execute("SELECT count(*) FROM sync_entities").fetchone()[0]
        transcript_count = db.execute(
            "SELECT count(*) FROM runtime_items WHERE agent=?", (lead["id"],)
        ).fetchone()[0]
    assert agent_count >= 1500, agent_count
    assert entity_count >= 2000, entity_count
    assert transcript_count >= 1500, transcript_count
    (canvas.root / "performance-fixture.json").write_text(json.dumps({
        "lead": lead["id"], "workBytes": work_bytes, "workCount": 700,
        "agentCount": agent_count, "entityCount": entity_count,
        "transcriptCount": transcript_count, "switchTargets": switch_targets,
    }))
    return original_make_server(canvas, *args, **kwargs)


codex_canvas.make_server = with_history
runpy.run_path(str(SERVER_TESTS_ROOT / "simple-ui-fixture.py"), run_name="__main__")
