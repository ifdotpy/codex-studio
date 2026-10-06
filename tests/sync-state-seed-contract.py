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

from codex_canvas import Canvas
from codex_runtime import Runtime
from codex_sync import SyncStore
from studio_api.test_helpers import read_legacy_snapshot_field


def runtime_fixture():
    path = Path(__file__).with_name("runtime-contract.py")
    spec = importlib.util.spec_from_file_location("sync_seed_runtime_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CanvasChatSeedContract(unittest.TestCase):
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
                    **canvas.snapshot(runtime_snapshot=runtime.snapshot(include_work=False)),
                    "runtime": runtime.snapshot(include_work=False),
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
                snapshot = runtime.snapshot(include_work=False)
                self.assertEqual(read_legacy_snapshot_field(
                    lambda: snapshot, "agents",
                    next(index for index, agent in enumerate(snapshot["agents"])
                         if agent["id"] == worker_id),
                    "overview", "resultFile"), expected)
                self.assertNotIn("work", snapshot)
                full_snapshot = runtime.snapshot(include_work=True)
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
