"""Entity DTO, bounded tracking and shared sequence-space contract."""
import contextlib
import json
from pathlib import Path
import re
import sqlite3
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(root / "scripts"))
from codex_sync import SyncStore
from codex_sync_entities import AGENT_FIELDS, ensure_tables, project, put

with tempfile.TemporaryDirectory() as directory:
    path = Path(directory) / "state.sqlite3"

    @contextlib.contextmanager
    def connect():
        db = sqlite3.connect(path, timeout=5, isolation_level=None)
        try:
            with db:
                ensure_tables(db)
                db.execute("CREATE TABLE IF NOT EXISTS sync_identity(id TEXT PRIMARY KEY)")
                db.execute("INSERT OR IGNORE INTO sync_identity VALUES (?)", ("a" * 32,))
                yield db
        finally:
            db.close()

    visible = {
        "id": "agent-1", "name": "Worker", "status": "failed", "error": "visible error",
        "prompt": "P" * 5000, "lastAnswer": "R" * 5000, "lastCompletedTurn": "turn-1",
        "events": 500, "nativeRelease": {"phase": "released", "resetPending": True, "private": 1},
        "activity": {"phase": "tool", "private": 1}, "nativeStatus": {"error": "status error", "private": 1},
        "startAttempt": {"prepareError": "prepare failure", "private": 1},
        "provider": "codex", "nativeToolCatalog": {"secret": "private"}, "timing": {"private": 1},
    }
    projected = project("agent", visible)
    assert projected["error"] == "visible error"
    assert len(projected["overview"]["task"]) == 4000
    assert projected["overview"]["taskTruncated"] is True
    assert projected["nativeRelease"] == {"phase": "released", "resetPending": True}
    assert projected["activity"] == {"phase": "tool"}
    assert projected["nativeStatus"] == {"error": "status error"}
    assert projected["startAttempt"] == {"prepareError": "prepare failure"}
    assert "nativeToolCatalog" not in projected and "timing" not in projected
    question = project("request", {"id": "request-1", "method": "agent/asyncQuestion",
        "status": "pending", "params": {"questions": [{"question": "Which scope?"}]},
        "internal": "private"})
    assert question["params"]["questions"][0]["question"] == "Which scope?"
    assert "internal" not in question

    # Renderer fields used by the worker cards and team panel must stay on wire.
    renderer = (root / "web/src/components/WorkerOverview.tsx").read_text() + (
        root / "web/src/App.tsx"
    ).read_text() + (root / "web/src/types.ts").read_text() + (
        root / "web/src/nativeErrors.ts"
    ).read_text()
    for field in ("name", "status", "error", "overview", "nativeRelease", "nativeThreadBlock"):
        assert field in AGENT_FIELDS, f"renderer field {field} is missing from the agent DTO"
        assert field in renderer, f"contract expected a current renderer use of {field}"
    renderer_reads = set(re.findall(
        r"\b(?:agent|worker|member|thread|a)\??\.([A-Za-z_$][\w$]*)", renderer
    )) - {"path"}  # `a.path` in App is a project-folder row, not an Agent.
    assert renderer_reads <= AGENT_FIELDS, (
        f"renderer reads agent fields absent from the sync DTO: {sorted(renderer_reads - AGENT_FIELDS)}"
    )

    with connect() as db:
        db.execute("INSERT INTO sync_versions VALUES (40,'old','hash',0,0)")
        assert put(db, "agent", "agent-1", visible)
        first = db.execute("SELECT seq,payload FROM sync_entities WHERE collection='agent'").fetchone()
        assert first[0] == 41
        # Internal-only rewrites do not bump sequence or replace the DTO.
        assert not put(db, "agent", "agent-1", {**visible, "events": 501, "timing": {"private": 2}})
        assert db.execute("SELECT seq FROM sync_entities WHERE collection='agent'").fetchone()[0] == 41
        assert put(db, "agent", "agent-1", {**visible, "status": "completed"})
        assert put(db, "agent", "agent-1", {}, deleted=True)
        tombstone = db.execute("SELECT seq,deleted,payload FROM sync_entities WHERE collection='agent'").fetchone()
        assert tombstone[0] == 43 and tombstone[1] == 1
        assert json.loads(tombstone[2])["value"] == {}
        assert db.execute("SELECT count(*) FROM sync_entities WHERE collection='agent'").fetchone()[0] == 1

    snapshot = {"stateDir": directory, "threads": [], "chats": [], "nodes": [], "edges": [],
                "runtime": {"agents": [{"id": "a", "name": "A", "status": "running", "source": "managed"}],
                            "rooms": [], "tasks": [], "monitors": [], "complaints": [], "requests": []}}
    store = SyncStore(connect, lambda: snapshot, lambda _key: {})
    seeded = store.pull("state:entities:v1", after=0, limit=100)
    assert seeded["workspaceId"] == "a" * 32
    assert all(item["id"].startswith("entity:") for item in seeded["documents"])
    assert seeded["maxSeq"] >= seeded["checkpoint"]["seq"]
    checkpoint = seeded["checkpoint"]["seq"]
    with connect() as db:
        assert put(db, "agent", "a", {"id": "a", "name": "Updated", "status": "running", "source": "managed"})
    second_window = SyncStore(connect, lambda: snapshot, lambda _key: {})
    first_change = store.pull("state:entities:v1", after=checkpoint)
    replayed_change = second_window.pull("state:entities:v1", after=checkpoint)
    assert first_change["documents"] == replayed_change["documents"]
    changed_seq = first_change["checkpoint"]["seq"]
    assert json.loads(first_change["documents"][0]["payload"])["value"]["name"] == "Updated"
    with connect() as db:
        assert put(db, "agent", "a", {}, deleted=True)
    for client in (store, second_window):
        tombstone = client.pull("state:entities:v1", after=changed_seq)
        assert tombstone["documents"][0]["_deleted"]
    restarted = SyncStore(connect, lambda: snapshot, lambda _key: {})
    assert not restarted.pull("state:entities:v1", after=tombstone["checkpoint"]["seq"])["documents"]
    # The full state scope remains available to clients not yet reloaded.
    legacy = store.pull("state")
    assert "runtime" in json.loads(legacy["documents"][0]["payload"])
    print("sync entity contract passed")
