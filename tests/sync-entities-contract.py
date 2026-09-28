"""Entity DTO, bounded tracking and shared sequence-space contract."""
import contextlib
import json
from pathlib import Path
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
        "events": 500, "nativeToolCatalog": {"secret": "private"}, "timing": {"private": 1},
    }
    projected = project("agent", visible)
    assert projected["error"] == "visible error"
    assert len(projected["overview"]["task"]) == 4000
    assert projected["overview"]["taskTruncated"] is True
    assert "nativeToolCatalog" not in projected and "timing" not in projected

    # Renderer fields used by the worker cards and team panel must stay on wire.
    renderer = (root / "web/src/components/WorkerOverview.tsx").read_text() + (
        root / "web/src/App.tsx"
    ).read_text() + (root / "web/src/types.ts").read_text() + (
        root / "web/src/nativeErrors.ts"
    ).read_text()
    for field in ("name", "status", "error", "overview", "nativeRelease", "nativeThreadBlock"):
        assert field in AGENT_FIELDS, f"renderer field {field} is missing from the agent DTO"
        assert field in renderer, f"contract expected a current renderer use of {field}"

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
    # The full state scope remains available to clients not yet reloaded.
    legacy = store.pull("state")
    assert "runtime" in json.loads(legacy["documents"][0]["payload"])
    print("sync entity contract passed")
