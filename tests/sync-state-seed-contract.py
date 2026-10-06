"""A Canvas chat node is seeded only as a chat entity."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import sqlite3
import sys
import tempfile
import unittest
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
from studio_api.testing import read_runtime_state

from codex_canvas import Canvas
from codex_runtime import Runtime
from codex_sync import SyncStore
from studio_api.context import ApiContext
from studio_api.testing import read_legacy_snapshot_field
from codex_sync_entities import seed


def runtime_fixture():
    path = Path(__file__).with_name("runtime-contract.py")
    spec = importlib.util.spec_from_file_location("sync_seed_runtime_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CanvasChatSeedContract(unittest.TestCase):
    def test_completed_seed_does_not_resolve_full_snapshot_builder_again(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-seed-builder-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                               draft=True, defer=True)
                from codex_sync_entities import seed
                calls = []

                def builder():
                    calls.append(None)
                    with runtime.read_db() as db:
                        value = runtime.snapshot(include_work=False, db=db)
                    return {"runtime": value, "stateDir": str(root)}

                with runtime.lock, runtime.db() as db:
                    db.execute("BEGIN IMMEDIATE")
                    seed(db, builder, runtime_owner=runtime)
                self.assertEqual(len(calls), 1)
                with runtime.lock, runtime.db() as db:
                    db.execute("BEGIN IMMEDIATE")
                    seed(db, builder, runtime_owner=runtime)
                self.assertEqual(len(calls), 1)
            finally:
                runtime.close()

    def test_first_seed_and_reseed_use_the_complaint_entity_view(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-complaint-seed-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                complaint = {"id": "seed-complaint", "leadId": lead["id"], "author": "user",
                             "status": "open", "created": 1.0, "updated": 1.0, "readAt": None,
                             "text": "Review this", "responses": [], "recipient": "user",
                             "version": 1, "sourceType": "user_task"}
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "complaints", complaint)

                    class Builder:
                        def __init__(self, owner):
                            self.runtime = owner

                        def build(self):
                            return {"runtime": self.runtime.snapshot(db=db),
                                    "stateDir": str(self.runtime.root)}

                    builder = Builder(runtime)
                    seed(db, builder.build, runtime_owner=runtime)
                    value = json.loads(db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='complaint' AND id=?",
                        (complaint["id"],)).fetchone()[0])["value"]
                    self.assertTrue(value["needsUserResponse"])

                    # Simulate an older marker and payload before the same upgrade runs again.
                    db.execute("UPDATE sync_entity_meta SET value='1' WHERE key='agent_organization_fields'")
                    from codex_sync_entities import put as entity_put
                    entity_put(db, "complaint", complaint["id"], {
                        key: field for key, field in value.items() if key != "needsUserResponse"})
                    seed(db, builder.build, runtime_owner=runtime)
                    value = json.loads(db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='complaint' AND id=?",
                        (complaint["id"],)).fetchone()[0])["value"]
                    self.assertTrue(value["needsUserResponse"])
            finally:
                runtime.close()

    def test_marker_one_without_runtime_owner_does_not_build_snapshots(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-seed-no-owner-") as directory:
            runtime = Runtime(Path(directory), fixture.FakeServer)
            try:
                runtime.create({"name": "Lead", "cwd": str(runtime.root), "prompt": ""},
                               draft=True, defer=True)
                calls = []
                def builder():
                    calls.append(None)
                    return {"runtime": runtime.snapshot(include_work=False),
                            "stateDir": str(runtime.root)}

                with runtime.lock, runtime.db() as db:
                    from codex_sync_entities import ensure_tables
                    ensure_tables(db)
                    db.execute("INSERT OR REPLACE INTO sync_entity_meta(key,value) VALUES('seeded','1')")
                    db.execute("INSERT OR REPLACE INTO sync_entity_meta(key,value) "
                               "VALUES('agent_organization_fields','1')")
                    db.execute("INSERT OR REPLACE INTO sync_entity_meta(key,value) "
                               "VALUES('task_window_migrated','1')")
                    event_sequence = db.execute(
                        "SELECT COALESCE(MAX(seq),0) FROM sync_entities WHERE collection='event'").fetchone()[0]
                    db.execute("INSERT OR REPLACE INTO sync_entity_meta(key,value) VALUES('event_window_seq',?)",
                               (str(event_sequence),))
                    from codex_sync_entities import put as entity_put
                    row = db.execute("SELECT payload FROM sync_entities WHERE collection='agent' LIMIT 1").fetchone()
                    old_value = json.loads(row[0])["value"]
                    old_value.pop("epoch", None)
                    agent_id = db.execute("SELECT id FROM sync_entities WHERE collection='agent' LIMIT 1").fetchone()[0]
                    entity_put(db, "agent", agent_id, old_value)
                with runtime.lock:
                    canvas = Canvas(runtime.root)
                    context = ApiContext(canvas)
                    context.snapshot = lambda include_work=True: builder()
                    store = context.sync()
                store._ensure_versions()
                blocker = sqlite3.connect(runtime.db_path, timeout=0)
                blocker.execute("BEGIN IMMEDIATE")
                try:
                    for _ in range(5):
                        store.pull("state:entities:v1", fresh=True, reset_support=True)
                finally:
                    blocker.rollback()
                    blocker.close()
                self.assertEqual(calls, [])
                with runtime.db() as db:
                    before = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                canvas.runtime = runtime
                self.assertIs(context.sync(), store)
                store.pull("state:entities:v1", fresh=True, reset_support=True)
                self.assertEqual(calls, [None])
                with runtime.db() as db:
                    marker = db.execute("SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone()[0]
                    after = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                self.assertEqual(marker, "2")
                self.assertTrue(any(after[key] > before[key] for key in before if key in after))
                calls.clear()
                stable = {**after}
                store.pull("state:entities:v1", fresh=True, reset_support=True)
                with runtime.db() as db:
                    self.assertEqual(stable, {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")})
                self.assertEqual(calls, [])
            finally:
                if "context" in locals():
                    context.close()
                runtime.close()

    def test_first_seed_agent_entities_match_the_runtime_put_projection(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-agent-seed-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                worker = runtime.create({"name": "Worker", "cwd": str(root), "prompt": "Task"},
                                       parent=lead["id"], draft=True, defer=True)
                with runtime.lock, runtime.db() as db:
                    record = runtime.agent(worker["id"], db)
                    record.update(status="completed", lastCompletedTurn="turn-seed",
                                  lastAnswer="Seeded answer", turnId=None, inFlight=False)
                    runtime.put(db, "agents", record)
                    runtime.put(db, "work", {"id": "seed-result", "owner": worker["id"],
                        "rootId": lead["id"], "status": "review", "results": [
                            {"agent": worker["id"], "created": 1, "resultFile": "seed.md"}]})

                    class Builder:
                        def __init__(self, owner):
                            self.runtime = owner

                        def build(self):
                            return {"runtime": self.runtime.snapshot(db=db),
                                    "stateDir": str(self.runtime.root)}

                    from codex_sync_entities import project, seed, upgrade_agent_organization
                    builder = Builder(runtime)
                    seed(db, builder.build, runtime_owner=runtime)
                    agent_ids = [row[0] for row in db.execute("SELECT id FROM runtime_agents")]
                    for agent_id in agent_ids:
                        record = runtime.agent(agent_id, db)
                        expected = project("agent", runtime.agent_entity_view(db, record))
                        stored = db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                                            (agent_id,)).fetchone()[0]
                        self.assertEqual(json.loads(stored)["value"], expected)
                    before = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                    self.assertEqual(upgrade_agent_organization(db, builder.build, runtime_owner=runtime), 0)
                    after = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                    self.assertEqual(after, before)
            finally:
                runtime.close()

    def test_snapshot_chat_node_is_not_also_seeded_as_agent(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-seed-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                canvas = Canvas(root)
                canvas.runtime = runtime
                chat_id = str(uuid.uuid4())
                canvas.create_chat("Planning", [lead["id"]], chat_id)
                snapshot = {
                    **canvas.snapshot(runtime_snapshot=read_runtime_state(runtime, include_work=False)),
                    "runtime": read_runtime_state(runtime, include_work=False),
                }
                self.assertIn(chat_id, {node["id"] for node in snapshot["nodes"]})

                # Keep the established node fallback covered alongside the
                # actual Canvas output, where chats also appear in nodes.
                fallback_id = "orphan-agent-fallback"
                snapshot["nodes"] = [
                    *snapshot["nodes"],
                    {"id": fallback_id, "name": "Recovered agent", "status": "unknown",
                     "kind": "agent", "source": "registered", "canSend": False,
                     "launcherAlive": False},
                ]
                store = SyncStore(canvas.connect, lambda: snapshot, lambda _key: {})
                store.pull("state:entities:v1", fresh=True, reset_support=True)

                with canvas.connect() as db:
                    entities = {
                        (row["collection"], row["id"]): json.loads(row["payload"])["value"]
                        for row in db.execute(
                            "SELECT collection,id,payload FROM sync_entities WHERE deleted=0"
                        )
                    }
                self.assertIn(("chat", chat_id), entities)
                self.assertNotIn(("agent", chat_id), entities)
                self.assertIn(("agent", fallback_id), entities)
            finally:
                runtime.close()

    def test_snapshot_streams_active_work_and_keeps_index_tie_winner(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-work-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                worker_id = str(uuid.uuid4())
                worker = dict(lead)
                worker.update(id=worker_id, name="Worker", isLead=False,
                              parentId=lead["id"], rootId=lead["id"],
                              prompt="worker task", deletedAt=None)
                inactive_owner = str(uuid.uuid4())
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "agents", worker)
                    # Insert review first so the existing owner/status index's
                    # status ordering, rather than insertion order, breaks the tie.
                    runtime.put(db, "work", {
                        "id": "review-result", "owner": worker_id, "rootId": lead["id"],
                        "status": "review", "results": [
                            {"agent": worker_id, "created": 7, "resultFile": "review.md",
                             "text": "x" * 32000, "checks": "y" * 32000},
                        ],
                    })
                    runtime.put(db, "work", {
                        "id": "accepted-result", "owner": worker_id, "rootId": lead["id"],
                        "status": "accepted", "results": [
                            {"agent": worker_id, "created": 7, "resultFile": "accepted.md"},
                        ],
                    })
                    runtime.put(db, "work", {
                        "id": "inactive-result", "owner": inactive_owner, "rootId": lead["id"],
                        "status": "review", "results": [
                            {"agent": inactive_owner, "created": 99, "resultFile": "inactive.md"},
                        ],
                    })
                    expected = runtime.latest_work_result_file(db, worker_id)

                self.assertEqual(expected, "accepted.md")
                snapshot = read_runtime_state(runtime, include_work=False)
                self.assertEqual(read_legacy_snapshot_field(
                    lambda: snapshot, "agents",
                    next(index for index, agent in enumerate(snapshot["agents"])
                         if agent["id"] == worker_id),
                    "overview", "resultFile"), expected)
                self.assertNotIn("work", snapshot)
                full_snapshot = read_runtime_state(runtime, include_work=True)
                self.assertEqual(read_legacy_snapshot_field(
                    lambda: full_snapshot, "agents",
                    next(index for index, agent in enumerate(full_snapshot["agents"])
                         if agent["id"] == worker_id),
                    "overview", "resultFile"), expected)
                work_ids = {work["id"] for work in full_snapshot["work"]}
                self.assertEqual(work_ids, {"review-result", "accepted-result", "inactive-result"})
            finally:
                runtime.close()


if __name__ == "__main__":
    unittest.main()
