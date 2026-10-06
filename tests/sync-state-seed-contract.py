"""A Canvas chat node is seeded only as a chat entity."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
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
                    seed(db, builder)
                self.assertEqual(len(calls), 1)
                with runtime.lock, runtime.db() as db:
                    db.execute("BEGIN IMMEDIATE")
                    seed(db, builder)
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
                    seed(db, builder.build)
                    value = json.loads(db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='complaint' AND id=?",
                        (complaint["id"],)).fetchone()[0])["value"]
                    self.assertTrue(value["needsUserResponse"])

                    # Simulate an older marker and payload before the same upgrade runs again.
                    db.execute("UPDATE sync_entity_meta SET value='1' WHERE key='agent_organization_fields'")
                    from codex_sync_entities import put as entity_put
                    entity_put(db, "complaint", complaint["id"], {
                        key: field for key, field in value.items() if key != "needsUserResponse"})
                    seed(db, builder.build)
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
                with runtime.lock:
                    store = SyncStore(runtime.db, builder, lambda _key: {})
                for _ in range(5):
                    store.pull("state:entities:v1", fresh=True, reset_support=True)
                self.assertEqual(calls, [])
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
