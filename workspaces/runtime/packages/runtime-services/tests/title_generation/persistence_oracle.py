"""Run Python's real rename completion and return its persisted state."""
import json
import hashlib
import sqlite3
import sys
import threading
from contextlib import contextmanager
from pathlib import Path
from types import MethodType
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "turn_lifecycle"))
from oracle_guard import install_guard

install_guard(block_processes=True)
sys.path.insert(0, str(HERE.parents[3] / "apps/server/src"))
import codex_rename
import codex_runtime
from agent_progress_contract import install_projection_contract
install_projection_contract(codex_runtime.Runtime)
import codex_sync_entities


def snapshot(db):
    row = db.execute("SELECT record FROM runtime_agents WHERE id='title-agent'").fetchone()
    events = [list(row) for row in db.execute("SELECT * FROM runtime_events ORDER BY id")]
    projection = [list(row) for row in db.execute(
        "SELECT collection,id,seq,hash,payload,deleted FROM sync_entities "
        "WHERE collection='agent' AND id='title-agent'")]
    receipts = [list(row) for row in db.execute(
        "SELECT id,signature,result FROM runtime_operation_receipts ORDER BY id")]
    return {"agent": json.loads(row[0]), "events": events, "projection": projection, "receipts": receipts}


def run(value):
    db = sqlite3.connect(value["database"])
    db.row_factory = sqlite3.Row
    codex_sync_entities.register_functions(db)
    codex_sync_entities.ensure_tables(db)
    codex_sync_entities.install_bypass_triggers(db)
    request_id = "persisted-title"
    signature = hashlib.sha256(json.dumps(
        {"operation": "rename", "chat": "title-agent", "name": None}, sort_keys=True
    ).encode()).hexdigest()
    db.execute("INSERT OR REPLACE INTO runtime_operation_receipts(id,signature,result) VALUES(?,?,?)",
               (request_id, signature, json.dumps({"status": "pending"})))
    db.commit()

    runtime = codex_runtime.AppServer.__new__(codex_runtime.AppServer)
    runtime.lock = threading.RLock()
    runtime.closed = False
    runtime._project_server_id = None
    runtime.__dict__["_rename_jobs"] = {request_id}
    @contextmanager
    def database():
        try:
            yield db
            db.commit()
        except BaseException:
            db.rollback()
            raise
    runtime.db = database
    runtime.agent = MethodType(codex_runtime.Runtime.agent, runtime)
    runtime.put = MethodType(codex_runtime.Runtime._put, runtime)
    runtime.agent_entity_view = MethodType(codex_runtime.Runtime.agent_entity_view, runtime)
    runtime.latest_work_result_file = lambda *_args: None
    runtime.affected_agent_room_ids = lambda *_args: set()
    runtime.new_agent_room_ids = lambda *_args: set()
    runtime.sync_agent_rooms = lambda *_args: None
    runtime.sync_workspace_volatile = lambda *_args: None
    runtime.mark_agent_records_changed = lambda *_args: None
    runtime.touch_ui = lambda *_args, **_kwargs: None
    runtime._stage_transcript_resource = lambda *_args: None
    runtime._stage_resource_change = lambda *_args: None
    runtime._stage_task_resources_for_root = lambda *_args: None

    original = runtime.agent("title-agent", db)
    scenario = value["scenario"]
    if scenario == "manual":
        codex_rename._set_name(runtime, db, "title-agent", "Manual title")
    elif scenario == "tool":
        current = runtime.agent("title-agent", db)
        current.update(name="Tool title", needsTitle=False)
        runtime.put(db, "agents", current)
    elif scenario == "refusal":
        current = runtime.agent("title-agent", db)
        current["deletedAt"] = 1_791_936_000.0
        runtime.put(db, "agents", current)

    if scenario in {"manual", "tool", "refusal"}:
        expected = {"manual": "Manual title", "tool": "Tool title", "refusal": original["name"]}[scenario]
        assert runtime.agent("title-agent", db)["name"] == expected, (scenario, runtime.agent("title-agent", db))
        assert original["name"] == "Original conversation name", original

    with patch.object(codex_rename, "_generate", return_value="Generated title"):
        codex_rename._finish(runtime, "title-agent", request_id, original, "first", [])
    result = snapshot(db)
    db.close()
    return result


print(json.dumps(run({"database": sys.argv[1], "scenario": sys.argv[2]}), ensure_ascii=False))
