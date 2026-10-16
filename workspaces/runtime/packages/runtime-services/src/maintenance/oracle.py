"""Differential oracle for Python's real retired-review cleanup and _put path."""

from __future__ import annotations

import json
import ast
import hashlib
import sqlite3
import sys
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests/turn_lifecycle"))
from oracle_guard import install_guard

GUARD = install_guard(block_processes=True)
sys.path.insert(0, str(Path(__file__).resolve().parents[4] / "apps/server/src"))

from codex_state_cleanup import remove_review_assignments
from codex_sync_entities import put as sync_entity_put
from codex_scheduler_wake import record_needs_dispatch


def load_runtime_put():
    source = Path(__file__).resolve().parents[4] / "apps/server/src/codex_runtime.py"
    module = ast.parse(source.read_text(encoding="utf-8"))
    runtime_class = next(
        node for node in module.body
        if isinstance(node, ast.ClassDef) and any(
            isinstance(member, ast.FunctionDef) and member.name == "_put"
            for member in node.body
        )
    )
    method = next(
        member for member in runtime_class.body
        if isinstance(member, ast.FunctionDef) and member.name == "_put"
    )
    extracted = ast.Module(body=[method], type_ignores=[])
    ast.fix_missing_locations(extracted)
    namespace: dict[str, Any] = {
        "Any": Any,
        "json": json,
        "record_needs_dispatch": record_needs_dispatch,
        # The changed retired keys are outside both resource contracts.
        "transcript_agent_resource_changed": lambda _previous, _record: False,
        "workspace_agent_resource_changed": lambda _previous, _record: False,
    }
    exec(compile(extracted, str(source), "exec"), namespace)
    return namespace["_put"]


RUNTIME_PUT = load_runtime_put()


def seed(path: Path) -> sqlite3.Connection:
    db = sqlite3.connect(path)
    db.executescript(
        "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);"
        "CREATE TABLE runtime_rooms(id TEXT PRIMARY KEY,record TEXT NOT NULL);"
        "CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,"
        "created REAL,epoch INTEGER,turn_id TEXT,error TEXT);"
        "CREATE TABLE runtime_agent_record_generation(id INTEGER PRIMARY KEY,value INTEGER NOT NULL);"
        "INSERT INTO runtime_agent_record_generation VALUES(1,0);"
        "CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents "
        "BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; END;"
        "CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE OF record ON runtime_agents "
        "WHEN OLD.record IS NOT NEW.record "
        "BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; END;"
        "CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents "
        "BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; END;"
        "CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,"
        "deleted INTEGER,PRIMARY KEY(collection,id));"
        "CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY,scope TEXT,id TEXT,payload TEXT,"
        "deleted INTEGER,UNIQUE(scope,id));"
        "CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY,scope TEXT UNIQUE,hash TEXT,deleted INTEGER,updated REAL);"
        "CREATE TABLE sync_entity_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);"
    )
    db.executemany(
        "INSERT INTO runtime_agents(id,record) VALUES (?,?)",
        [
            ("agent-a", json.dumps({"id": "agent-a", "rootId": "agent-a", "epoch": 0, "status": "idle", "reviewSchedules": [{"id": "retired"}], "extra": 1})),
            ("agent-b", json.dumps({"id": "agent-b", "rootId": "agent-b", "epoch": 0, "status": "idle", "extra": 2})),
        ],
    )
    db.execute(
        "INSERT INTO runtime_rooms(id,record) VALUES (?,?)",
        ("room-a", json.dumps({"id": "room-a", "kind": "private", "members": ["agent-a"], "updated": 1, "userHidden": False, "reviewTargets": None, "extra": 3})),
    )
    db.executemany(
        "INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?,?,?)",
        [
            ("pending", "agent-a", "chat_review", "pending body", "pending", 1, 0, None, None),
            ("submitted", "agent-a", "chat_review", "submitted body", "submitted", 2, 0, None, "receipt"),
            ("uncertain", "agent-a", "chat_review", "uncertain body", "uncertain", 3, 0, None, "unknown"),
            ("other", "agent-a", "followup", "other body", "pending", 4, 0, None, None),
        ],
    )
    # These are the existing renderer DTO projections. The retired keys are
    # intentionally omitted because project() does not expose them.
    agent = json.loads(db.execute("SELECT record FROM runtime_agents WHERE id='agent-a'").fetchone()[0])
    sync_entity_put(db, "agent", "agent-a", {**agent, "kind": "agent", "source": "managed", "canSend": True})
    room = json.loads(db.execute("SELECT record FROM runtime_rooms WHERE id='room-a'").fetchone()[0])
    sync_entity_put(db, "room", "room-a", room)
    for collection, entity_id, field, retired_value in (
        ("agent", "agent-a", "reviewSchedules", [{"id": "retired"}]),
        ("room", "room-a", "reviewTargets", None),
    ):
        (payload,) = db.execute(
            "SELECT payload FROM sync_entities WHERE collection=? AND id=?",
            (collection, entity_id),
        ).fetchone()
        envelope = json.loads(payload)
        envelope["value"][field] = retired_value
        payload = json.dumps(envelope, sort_keys=True, separators=(",", ":"), ensure_ascii=False)
        db.execute(
            "UPDATE sync_entities SET payload=?,hash=? WHERE collection=? AND id=?",
            (payload, hashlib.sha256(payload.encode()).hexdigest(), collection, entity_id),
        )
    db.commit()
    return db


def snapshot(db: sqlite3.Connection) -> dict:
    return {
        "agents": [
            {"id": row[0], "record": json.loads(row[1])}
            for row in db.execute("SELECT id,record FROM runtime_agents ORDER BY id")
        ],
        "rooms": [
            {"id": row[0], "record": json.loads(row[1])}
            for row in db.execute("SELECT id,record FROM runtime_rooms ORDER BY id")
        ],
        "events": [
            dict(zip(("id", "agent", "kind", "text", "status", "created", "epoch", "turn_id", "error"), row))
            for row in db.execute("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id")
        ],
        "sync_entities": [
            dict(zip(("collection", "id", "seq", "hash", "payload", "deleted"), row))
            for row in db.execute("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities ORDER BY collection,id")
        ],
        "sync_documents": [
            dict(zip(("seq", "scope", "id", "payload", "deleted"), row))
            for row in db.execute("SELECT seq,scope,id,payload,deleted FROM sync_documents ORDER BY seq")
        ],
        "sync_versions": [
            dict(zip(("seq", "scope", "hash", "deleted", "updated"), row))
            for row in db.execute("SELECT seq,scope,hash,deleted,updated FROM sync_versions ORDER BY seq")
        ],
        "agent_generation": db.execute(
            "SELECT value FROM runtime_agent_record_generation WHERE id=1"
        ).fetchone()[0],
    }


class RuntimeProxy:
    """Minimal startup host that routes _put through its actual DTO writers."""

    _put = RUNTIME_PUT

    def __init__(self) -> None:
        self.effects = {"agent_cache_invalidations": 0, "ui_touches": 0, "resources": 0}

    def agent_entity_view(self, _db: sqlite3.Connection, record: dict) -> dict:
        return {**record, "kind": "agent", "source": "managed", "canSend": True}

    def sync_room_entity(self, db: sqlite3.Connection, room_id: str, **_kwargs: object) -> bool:
        row = db.execute("SELECT record FROM runtime_rooms WHERE id=?", (room_id,)).fetchone()
        return bool(row and sync_entity_put(db, "room", room_id, json.loads(row[0])))

    def sync_agent_rooms(self, _db: sqlite3.Connection, _room_ids: object) -> None:
        pass

    def affected_agent_room_ids(self, _db: sqlite3.Connection, _previous: dict, _record: dict) -> set[str]:
        return set()

    def sync_workspace_volatile(self, _db: sqlite3.Connection) -> bool:
        return False

    def mark_agent_records_changed(self, _agent_id: str) -> None:
        self.effects["agent_cache_invalidations"] += 1

    def touch_ui(self, _agent_id: str, _db: sqlite3.Connection, *, publish_resource: bool) -> None:
        assert not publish_resource
        self.effects["ui_touches"] += 1

    def _stage_resource_change(self, _db: sqlite3.Connection, _resource: object) -> None:
        self.effects["resources"] += 1


def main() -> None:
    request = json.load(sys.stdin)
    rust_db = seed(Path(request["rust_db"]))
    python_db = seed(Path(request["python_db"]))
    before = snapshot(python_db)
    runtime = RuntimeProxy()
    counts = remove_review_assignments(runtime, python_db)
    python_db.commit()
    result = {"counts": counts, "before": before, "state": snapshot(python_db), "effects": runtime.effects}
    print(json.dumps(result, sort_keys=True))
    rust_db.close()
    python_db.close()


if __name__ == "__main__":
    main()
