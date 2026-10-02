#!/usr/bin/env python3
"""The /api/state bounded runtime collections match their entity-sync materialization."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import random
import sys
import tempfile

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "tests"))
spec = importlib.util.spec_from_file_location("runtime_fixture", ROOT / "tests/runtime-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_sync_entities import (COLLECTION_FIELDS, ensure_tables, project, sync_event_window,
                                 sync_monitor_window, sync_task_window)


def materialized(db, collection):
    return {
        key: json.loads(payload)["value"]
        for key, payload in db.execute(
            "SELECT id,payload FROM sync_entities WHERE collection=? AND deleted=0", (collection,)
        )
    }


def assert_state(runtime, db, stage):
    # This is the runtime object placed directly under `runtime` by GET /api/state.
    state = runtime.snapshot(include_work=False, db=db)
    for public_key, collection in (("tasks", "task"), ("monitors", "monitor"), ("events", "event")):
        expected = {str(row["id"]): project(collection, row) for row in state[public_key]}
        actual = materialized(db, collection)
        assert actual == expected, (
            stage, public_key, len(actual), len(expected),
            sorted(actual.keys() ^ expected.keys())[:8],
        )
        assert all(set(value) <= COLLECTION_FIELDS[collection] for value in actual.values())


with tempfile.TemporaryDirectory(prefix="studio-entity-windows-") as tmp:
    runtime = fixture.Runtime(Path(tmp) / "state", fixture.FakeServer)
    rng = random.Random(20260929)
    try:
        agents = [f"agent-{index}" for index in range(5)]
        with runtime.lock, runtime.db() as db:
            ensure_tables(db)
            for key in agents:
                runtime.put(db, "agents", {
                    "id": key, "name": key, "status": "idle", "rootId": key,
                    "isLead": False, "cwd": tmp, "created": 1, "model": "fixture",
                })
            for index in range(390):
                agent = rng.choice(agents)
                status = "running" if rng.random() < .17 else rng.choice(("completed", "failed"))
                task = {
                    "id": f"task-{index}", "agent": agent, "kind": "command", "status": status,
                    "created": float(index), "command": f"cmd-{index}",
                    "tail": f"private-tail-{index}", "arguments": [index], "error": f"private-{index}",
                    "interactive": bool(index % 2),
                }
                db.execute("INSERT INTO runtime_tasks VALUES (?,?)", (task["id"], json.dumps(task)))
            for index in range(780):
                agent = rng.choice(agents)
                status = rng.choice(("running", "starting", "approval", "completed", "failed", "cancelled", "lost"))
                monitor = {
                    "id": f"monitor-{index}", "agent": agent, "status": status,
                    "created": float(index), "name": f"name-{index}", "command": f"cmd-{index}",
                    "tail": f"tail-{index}", "error": f"error-{index}",
                }
                db.execute("INSERT INTO runtime_monitors VALUES (?,?)", (monitor["id"], json.dumps(monitor)))
            for index in range(270):
                agent = rng.choice(agents)
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) "
                           "VALUES (?,?,?,?,?,?,?,?,?)",
                           (f"event-{index}", agent, "fixture", "private text", "delivered",
                            float(index), 1, None, f"event-error-{index}"))
            assert sync_task_window(db, force=True) >= 0
            sync_monitor_window(db)
            sync_event_window(db)
            assert_state(runtime, db, "initial randomized window")

            # New records move all three windows and evict prior terminal history.
            for index in range(390, 430):
                agent = rng.choice(agents)
                task = {"id": f"task-{index}", "agent": agent, "kind": "command",
                        "status": "completed", "created": float(index + 1000), "command": "new"}
                monitor = {"id": f"monitor-{index + 780}", "agent": agent, "status": "completed",
                           "created": float(index + 1000), "command": "new"}
                db.execute("INSERT INTO runtime_tasks VALUES (?,?)", (task["id"], json.dumps(task)))
                db.execute("INSERT INTO runtime_monitors VALUES (?,?)", (monitor["id"], json.dumps(monitor)))
                db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) "
                           "VALUES (?,?,?,?,?,?,?,?,?)",
                           (f"event-{index + 270}", agent, "fixture", "private text", "delivered",
                            float(index + 1000), 1, None, "new-error"))
            sync_task_window(db, force=True)
            sync_monitor_window(db)
            sync_event_window(db)
            assert_state(runtime, db, "after window movement")

            # Deleting an agent must retire its tasks/monitors and allow the task
            # archive window to refill from other agents.
            removed = runtime.agent(agents[0], db)
            removed["deletedAt"] = 2
            runtime.put(db, "agents", removed)
            sync_task_window(db, force=True)
            sync_monitor_window(db)
            sync_event_window(db)
            assert_state(runtime, db, "after agent deletion")
    finally:
        runtime.close()

print("PASS: randomized snapshot/entity windows agree across movement and agent deletion")
