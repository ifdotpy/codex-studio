"""Entity DTO, bounded tracking and shared sequence-space contract."""
import contextlib
import json
from pathlib import Path
import random
import re
import sqlite3
import sys
import tempfile

root = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(root / "scripts"))
from codex_sync import SyncStore
from codex_sync_entities import (AGENT_FIELDS, ensure_tables, project, put,
                                sync_task_agent_change, sync_task_window, sync_task_write)

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
    task = project("task", {"id": "task-1", "status": "failed", "command": "echo ok",
        "tail": "old output", "arguments": ["large"], "error": "old error"})
    assert task == {"id": "task-1", "status": "failed", "command": "echo ok"}

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
        plan = " ".join(str(item) for item in db.execute("""EXPLAIN QUERY PLAN
            SELECT id FROM sync_entities WHERE collection='task' AND deleted=0
              AND json_extract(payload,'$.value.status')!='running'
            ORDER BY CAST(json_extract(payload,'$.value.created') AS REAL) DESC LIMIT 100"""))
        assert "sync_entities_collection_deleted" in plan, plan
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

    # A legacy task table is reduced to all running tasks and the newest 100
    # non-running tasks. Retirement is bounded and leaves ordinary tombstones.
    with connect() as db:
        db.executescript("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);"
                         "CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, record TEXT NOT NULL);")
        db.execute("INSERT INTO runtime_agents VALUES ('agent',?)", (json.dumps({"id":"agent","deletedAt":None}),))
        for index in range(107):
            status = "running" if index >= 105 else "completed"
            record = {"id":f"task-{index}","agent":"agent","status":status,"created":index,
                      "command":"echo ok","tail":"x" * 10000,"arguments":["large"],"error":"private"}
            raw = json.dumps(record)
            db.execute("INSERT INTO runtime_tasks VALUES (?,?)", (record["id"], raw))
            # Emulate unbounded entities written by the prior implementation.
            legacy = json.dumps({"collection":"task","id":record["id"],"value":record},
                                sort_keys=True,separators=(",",":"))
            db.execute("INSERT INTO sync_entities VALUES ('task',?, ?, ?, ?, 0)",
                       (record["id"],100+index,"legacy",legacy))
        assert sync_task_window(db, batch_size=3) == 3
        assert db.execute("SELECT count(*) FROM sync_entities WHERE collection='task' AND deleted=1").fetchone()[0] == 3
        while sync_task_window(db, batch_size=3):
            pass
        assert db.execute("SELECT count(*) FROM sync_entities WHERE collection='task' AND deleted=0").fetchone()[0] == 102
        assert db.execute("SELECT value FROM sync_entity_meta WHERE key='task_window_migrated'").fetchone() == ("1",)
        sample = json.loads(db.execute("SELECT payload FROM sync_entities WHERE collection='task' AND id='task-106'").fetchone()[0])["value"]
        assert sample["status"] == "running" and "tail" not in sample and "arguments" not in sample and "error" not in sample
        db.execute("UPDATE runtime_agents SET record=? WHERE id='agent'", (json.dumps({"id":"agent","deletedAt":1}),))
        while sync_task_window(db, batch_size=3, force=True):
            pass
        assert db.execute("SELECT count(*) FROM sync_entities WHERE collection='task' AND deleted=0").fetchone()[0] == 0
        db.execute("DELETE FROM sync_entities WHERE collection='task'")
        db.execute("DELETE FROM runtime_tasks")
        db.execute("DELETE FROM runtime_agents")

    # Steady-state writes must keep the live entity set equal to recent_tasks().
        rng = random.Random(60829)
        agents = [f"agent-{i}" for i in range(4)]
        for agent in agents:
            db.execute("INSERT INTO runtime_agents VALUES (?,?)",
                       (agent, json.dumps({"id":agent,"deletedAt":None})))

        def assert_matches_recent_tasks():
            expected_rows = db.execute("""SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
            ON json_extract(t.record,'$.agent')=a.id WHERE json_extract(a.record,'$.deletedAt') IS NULL
            AND json_extract(t.record,'$.status')='running'
            UNION ALL SELECT record FROM (SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
            ON json_extract(t.record,'$.agent')=a.id WHERE json_extract(a.record,'$.deletedAt') IS NULL
            AND json_extract(t.record,'$.status')!='running'
            ORDER BY json_extract(t.record,'$.created') DESC LIMIT 100)""").fetchall()
            expected = {json.loads(raw[0])["id"]: project("task", json.loads(raw[0]))
                    for raw in expected_rows}
            actual = {key: json.loads(payload)["value"] for key, payload in db.execute(
            "SELECT id,payload FROM sync_entities WHERE collection='task' AND deleted=0")}
            assert actual == expected, (current_step, current_op, len(actual), len(expected),
                [(key, expected.get(key, {}).get("status"), expected.get(key, {}).get("created"),
                  expected.get(key, {}).get("agent"), actual.get(key, {}).get("agent"))
                 for key in sorted(actual.keys() ^ expected.keys())[:12]])

        tasks = {}
        current_step = "initial"
        current_op = ""
        for index in range(240):
            record = {"id":f"rand-{index}","agent":rng.choice(agents),
                  "status":"running" if index % 7 == 0 else "completed",
                  "created":float(index),"command":f"cmd-{index}","tail":"discard"}
            tasks[record["id"]] = record
            raw = json.dumps(record)
            db.execute("INSERT INTO runtime_tasks VALUES (?,?)", (record["id"], raw))
            sync_task_write(db, record)
        assert_matches_recent_tasks()
        for step in range(500):
            current_step = step
            if step % 31 == 0:
                agent = rng.choice(agents)
                current_op = f"agent {agent}"
                row = db.execute("SELECT record FROM runtime_agents WHERE id=?", (agent,)).fetchone()
                value = json.loads(row[0])
                value["deletedAt"] = None if value.get("deletedAt") else 1
                db.execute("UPDATE runtime_agents SET record=? WHERE id=?", (json.dumps(value), agent))
                sync_task_agent_change(db, agent, bool(value.get("deletedAt")))
            else:
                key = rng.choice(list(tasks))
                record = dict(tasks[key])
                current_op = f"task {key} -> {record['status']}"
                record["status"] = "running" if rng.random() < 0.28 else rng.choice(["completed", "failed"])
                record["command"] = f"cmd-step-{step}"
                tasks[key] = record
                db.execute("UPDATE runtime_tasks SET record=? WHERE id=?", (json.dumps(record), key))
                sync_task_write(db, record)
            assert_matches_recent_tasks()
        db.execute("DROP TABLE runtime_tasks")
        db.execute("DROP TABLE runtime_agents")

    with connect() as db:
        db.execute("DELETE FROM sync_entities")
        db.execute("DELETE FROM sync_entity_meta")

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
