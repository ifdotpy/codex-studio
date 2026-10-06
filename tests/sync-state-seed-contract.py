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
from codex_canvas import Canvas
from codex_runtime import Runtime
from codex_sync import SyncStore
from studio_api.context import ApiContext
from studio_api.testing import read_runtime_state
from studio_api.testing import read_legacy_snapshot_field
from codex_sync_entities import seed


def runtime_fixture():
    path = Path(__file__).with_name("runtime-contract.py")
    spec = importlib.util.spec_from_file_location("sync_seed_runtime_fixture", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CanvasChatSeedContract(unittest.TestCase):
    def test_seed_and_current_reseed_do_not_build_a_snapshot(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-seed-builder-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                               draft=True, defer=True)
                canvas = Canvas(root)
                canvas.runtime = runtime
                runtime.snapshot = lambda **_kwargs: (_ for _ in ()).throw(AssertionError("snapshot called"))

                with runtime.lock, runtime.db() as db:
                    db.execute("BEGIN IMMEDIATE")
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                with runtime.lock, runtime.db() as db:
                    db.execute("BEGIN IMMEDIATE")
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
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
                canvas = Canvas(root)
                canvas.runtime = runtime
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "complaints", complaint)
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    value = json.loads(db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='complaint' AND id=?",
                        (complaint["id"],)).fetchone()[0])["value"]
                    self.assertTrue(value["needsUserResponse"])

                    # Simulate an older marker and payload before the same upgrade runs again.
                    db.execute("UPDATE sync_entity_meta SET value='1' WHERE key='agent_organization_fields'")
                    from codex_sync_entities import put as entity_put
                    entity_put(db, "complaint", complaint["id"], {
                        key: field for key, field in value.items() if key != "needsUserResponse"})
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
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
                with runtime.lock, runtime.db() as db:
                    from codex_sync_entities import ensure_tables
                    ensure_tables(db)
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
                    store = context.sync()
                first = store.pull("state:entities:v1", fresh=True, reset_support=True)
                self.assertTrue(first["documents"])
                with runtime.db() as db:
                    self.assertIsNone(db.execute(
                        "SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone())
                blocker = sqlite3.connect(runtime.db_path, timeout=0)
                blocker.execute("BEGIN IMMEDIATE")
                try:
                    for _ in range(5):
                        store.pull("state:entities:v1", fresh=True, reset_support=True)
                finally:
                    blocker.rollback()
                    blocker.close()
                self.assertIsNone(store.runtime)
                with runtime.db() as db:
                    before = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                canvas.runtime = runtime
                self.assertIs(context.sync(), store)
                store.pull("state:entities:v1", fresh=True, reset_support=True)
                self.assertIs(store.runtime, runtime)
                with runtime.db() as db:
                    marker = db.execute("SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone()[0]
                    after = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                    from codex_sync_entities import project
                    runtime_agents = [runtime.agent(key, db) for (key,) in db.execute(
                        "SELECT id FROM runtime_agents")]
                    entity_values = {key: json.loads(payload)["value"] for key, payload in db.execute(
                        "SELECT id,payload FROM sync_entities WHERE collection='agent' AND deleted=0")}
                self.assertEqual(marker, "3")
                self.assertTrue(any(after[key] > before[key] for key in before if key in after))
                with runtime.db() as db:
                    for record in runtime_agents:
                        if record.get("deletedAt"):
                            continue
                        expected = project("agent", runtime.agent_entity_view(db, record))
                        self.assertEqual(entity_values[record["id"]], expected, record["id"])
                stable = {**after}
                store.pull("state:entities:v1", fresh=True, reset_support=True)
                with runtime.db() as db:
                    self.assertEqual(stable, {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")})
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
                canvas = Canvas(root)
                canvas.runtime = runtime
                with runtime.lock, runtime.db() as db:
                    record = runtime.agent(worker["id"], db)
                    record.update(status="completed", lastCompletedTurn="turn-seed",
                                  lastAnswer="Seeded answer", turnId=None, inFlight=False)
                    runtime.put(db, "agents", record)
                    runtime.put(db, "work", {"id": "seed-result", "owner": worker["id"],
                        "rootId": lead["id"], "status": "review", "results": [
                            {"agent": worker["id"], "created": 1, "resultFile": "seed.md"}]})

                    from codex_sync_entities import project, seed, upgrade_agent_organization
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    agent_ids = [row[0] for row in db.execute("SELECT id FROM runtime_agents")]
                    for agent_id in agent_ids:
                        record = runtime.agent(agent_id, db)
                        expected = project("agent", runtime.agent_entity_view(db, record))
                        stored = db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                                            (agent_id,)).fetchone()[0]
                        self.assertEqual(json.loads(stored)["value"], expected)
                    before = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                    self.assertEqual(upgrade_agent_organization(db, runtime, canvas), 0)
                    after = {row[0]: row[1] for row in db.execute(
                        "SELECT id,seq FROM sync_entities WHERE collection='agent'")}
                    self.assertEqual(after, before)
            finally:
                runtime.close()

    def test_populated_first_seed_matches_normal_projection_for_every_collection(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-all-collections-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            canvas = Canvas(root)
            canvas.runtime = runtime
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                worker = runtime.create({"name": "Worker", "cwd": str(root), "prompt": "Task"},
                                        parent=lead["id"], draft=True, defer=True)
                peer_lead = runtime.create({"name": "Peer lead", "cwd": str(root), "prompt": ""},
                                           draft=True, defer=True)
                chat_id = str(uuid.uuid4())
                canvas.create_chat("Planning", [lead["id"], worker["id"]], chat_id)
                with runtime.lock, runtime.db() as db:
                    runtime.projects({"path": str(root)})
                    runtime.put(db, "rooms", {"id": "seed-room", "kind": "private",
                                               "members": [lead["id"], worker["id"]], "updated": 1.0})
                    runtime.put(db, "tasks", {"id": "seed-task", "agent": worker["id"],
                                               "status": "completed", "created": 2.0, "finished": 3.0})
                    runtime.put(db, "monitors", {"id": "seed-monitor", "agent": worker["id"],
                                                  "status": "running", "created": 2.0, "command": "echo"})
                    runtime.put(db, "complaints", {"id": "seed-complaint", "leadId": lead["id"],
                                                    "author": "user", "text": "Review", "status": "open",
                                                    "created": 2.0, "updated": 2.0, "responses": [],
                                                    "recipient": "user", "version": 1})
                    runtime.put(db, "requests", {"id": "seed-request", "agent": worker["id"],
                                                  "method": "agent/asyncQuestion", "status": "pending",
                                                  "created": 2.0, "params": {"questions": []}})
                    runtime.put(db, "rules", {"id": "seed-rule", "agent": worker["id"], "name": "Build"})
                    runtime.put(db, "work", {"id": "seed-work", "rootId": lead["id"], "owner": worker["id"],
                                              "status": "review", "title": "Review"})
                    runtime.put(db, "projects", {"id": str(root), "path": str(root), "name": root.name,
                                                  "accountKey": "default", "accountRevision": 1,
                                                  "created": 1.0, "peerTeamsRevision": 1,
                                                  "peerTeams": [{"id": "seed-team", "name": "Peers",
                                                                 "members": [lead["id"], peer_lead["id"]]}]})
                    db.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) "
                               "VALUES(?,?,?,?,?,?,?)", ("seed-event", worker["id"], "user", "hi",
                                                          "delivered", 4.0, 0))
                    from codex_peer_teams import sync_entities as sync_peer_teams
                    sync_peer_teams(runtime, db)
                    from codex_sync_entities import sync_event_window, sync_monitor_window, sync_task_window, put
                    sync_task_window(db)
                    sync_monitor_window(db)
                    sync_event_window(db)
                    put(db, "workspace", "current", runtime.workspace_entity_view(db))
                    for (agent_id,) in db.execute("SELECT id FROM runtime_agents"):
                        runtime.put(db, "agents", runtime.agent(agent_id, db))
                    runtime_views = [runtime.agent_entity_view(db, runtime.agent(key, db))
                                     for key, _raw in db.execute("SELECT id,record FROM runtime_agents")
                                     if not runtime.agent(key, db).get("deletedAt")]
                    for edge in canvas.edges(canvas.threads(runtime_agents=runtime_views), db=db):
                        put(db, "edge", str(edge["id"]), edge)
                    expected = {
                        (collection, key): json.loads(payload)
                        for collection, key, payload in db.execute(
                            "SELECT collection,id,payload FROM sync_entities WHERE collection NOT LIKE 'transcript:%'")
                    }
                    db.execute("DELETE FROM sync_entities")
                    db.execute("DELETE FROM sync_entity_meta")
                    from codex_sync_entities import ensure_tables
                    ensure_tables(db)
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    actual = {
                        (collection, key): json.loads(payload)
                        for collection, key, payload in db.execute(
                            "SELECT collection,id,payload FROM sync_entities WHERE collection NOT LIKE 'transcript:%'")
                    }
                if actual != expected:
                    for entity_key in sorted(set(actual) | set(expected)):
                        if actual.get(entity_key) != expected.get(entity_key):
                            actual_value = actual.get(entity_key) or {}
                            expected_value = expected.get(entity_key) or {}
                            self.fail(
                                f"first-seed mismatch collection/id={entity_key} changed keys="
                                f"{sorted(key for key in set(actual_value.get('value', {})) | set(expected_value.get('value', {})) if actual_value.get('value', {}).get(key) != expected_value.get('value', {}).get(key))}")
                self.assertEqual({collection for collection, _key in actual}, {
                    "agent", "chat", "room", "task", "monitor", "complaint", "request", "rule",
                    "project", "peerTeam", "work", "event", "edge", "workspace",
                })
            finally:
                runtime.close()

    def test_canvas_chat_is_seeded_only_as_chat_entity(self):
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
                store = SyncStore(canvas.connect, lambda: {}, lambda _key: {}, runtime=runtime, canvas=canvas)
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
            finally:
                runtime.close()

    def test_maintenance_refreshes_wave_and_orchestrator_reference_entities(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-wave-refresh-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                canvas = Canvas(root)
                canvas.runtime = runtime
                wave_path = root / "codex-swarm-status.s8a.json"
                row = {"name": "Wave worker", "threadId": "wave-thread", "turnStatus": "completed",
                       "orchestratorId": "missing-orchestrator", "orchestratorName": "Old orchestrator",
                       "runId": "run-1"}
                wave_path.write_text(json.dumps([row]))
                with runtime.lock, runtime.db() as db:
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    wave_id = next(value[0] for value in db.execute(
                        "SELECT id,payload FROM sync_entities WHERE collection='agent'"
                    ) if json.loads(value[1])["value"].get("source") == "app-server")
                    ref_id = next(value[0] for value in db.execute(
                        "SELECT id,payload FROM sync_entities WHERE collection='agent'"
                    ) if json.loads(value[1])["value"].get("source") == "orchestrator-reference")
                    row.update(turnStatus="failed", orchestratorName="Updated orchestrator")
                    wave_path.write_text(json.dumps([row]))
                    db.execute("UPDATE sync_entity_meta SET value='2' WHERE key='agent_organization_fields'")
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    wave_value = json.loads(db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?", (wave_id,)
                    ).fetchone()[0])["value"]
                    ref_value = json.loads(db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?", (ref_id,)
                    ).fetchone()[0])["value"]
                    self.assertEqual(wave_value["name"], "Wave worker")
                    self.assertEqual(wave_value["status"], "failed")
                    self.assertEqual(ref_value["name"], "Updated orchestrator")
                    seq = db.execute("SELECT MAX(seq) FROM sync_entities").fetchone()[0]
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    self.assertEqual(db.execute("SELECT MAX(seq) FROM sync_entities").fetchone()[0], seq)
            finally:
                runtime.close()

    def test_room_maintenance_tombstones_vanished_rooms_and_reprojects_live_rooms(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-room-refresh-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                worker = runtime.create({"name": "Worker", "cwd": str(root), "prompt": ""},
                                        parent=lead["id"], draft=True, defer=True)
                canvas = Canvas(root)
                canvas.runtime = runtime
                with runtime.lock, runtime.db() as db:
                    live_room = {"id": "room-live", "kind": "private",
                                 "members": [lead["id"], worker["id"]], "updated": 1.0}
                    vanished_room = {"id": "room-vanished", "kind": "private",
                                     "members": [lead["id"], worker["id"]], "updated": 1.0}
                    runtime.put(db, "rooms", live_room)
                    runtime.put(db, "rooms", vanished_room)
                    from codex_sync_entities import put
                    put(db, "room", live_room["id"], {**live_room, "lastMessage": {
                        "created": 1.0, "sender": lead["id"], "seq": 1, "text": "legacy"}})
                    db.execute("DELETE FROM runtime_rooms WHERE id=?", (vanished_room["id"],))
                    db.execute("UPDATE sync_entity_meta SET value='2' WHERE key='agent_organization_fields'")
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    live = db.execute("SELECT payload,deleted FROM sync_entities WHERE collection='room' AND id=?",
                                      (live_room["id"],)).fetchone()
                    vanished = db.execute("SELECT payload,deleted FROM sync_entities WHERE collection='room' AND id=?",
                                          (vanished_room["id"],)).fetchone()
                    live_value = json.loads(live[0])["value"]
                    self.assertIn("lastMessage", live_value)
                    self.assertIsNone(live_value["lastMessage"])
                    self.assertEqual(vanished[1], 1)
                    seq = db.execute("SELECT MAX(seq) FROM sync_entities").fetchone()[0]
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    self.assertEqual(db.execute("SELECT MAX(seq) FROM sync_entities").fetchone()[0], seq)
            finally:
                runtime.close()

    def test_malformed_stored_entity_is_skipped_and_reported_once_on_pull(self):
        with tempfile.TemporaryDirectory(prefix="sync-state-malformed-") as directory:
            canvas = Canvas(Path(directory))
            store = SyncStore(canvas.connect, lambda: {}, lambda _key: {}, canvas=canvas)
            with canvas.connect() as db:
                db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('seeded','1')")
                db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','3')")
                db.execute("INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) "
                           "VALUES('agent','broken',999,'bad','{',0)")
            with self.assertLogs("codex_sync_entities", level="WARNING") as captured:
                result = store.pull("state:entities:v1")
                store.pull("state:entities:v1")
            self.assertEqual(len(captured.records), 1)
            self.assertNotIn("entity:agent:broken", {row["id"] for row in result["documents"]})

    def test_malformed_row_does_not_advance_a_short_filtered_page_to_high_water(self):
        with tempfile.TemporaryDirectory(prefix="sync-state-page-corrupt-") as directory:
            canvas = Canvas(Path(directory))
            store = SyncStore(canvas.connect, lambda: {}, canvas.transcript, canvas=canvas)
            with canvas.connect() as db:
                from codex_sync_entities import put
                db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('seeded','1')")
                db.execute("INSERT INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','3')")
                for index in range(10):
                    put(db, "rule", f"rule-{index}", {"id": f"rule-{index}", "name": f"Rule {index}"})
                db.execute("UPDATE sync_entities SET payload='{' WHERE collection='rule' AND id='rule-1'")
            after = 0
            received: set[str] = set()
            while True:
                page = store.pull("state:entities:v1", after=after, limit=3)
                received.update(row["id"] for row in page["documents"])
                checkpoint = page["checkpoint"]["seq"]
                if checkpoint == after or checkpoint >= page["maxSeq"]:
                    break
                after = checkpoint
            self.assertEqual(received, {f"entity:rule:rule-{index}" for index in range(10) if index != 1})

    def test_fresh_seed_does_not_create_never_visible_tombstones(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-no-phantom-tombstones-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                deleted = dict(lead, id="deleted-agent", rootId="deleted-agent", deletedAt=1.0)
                closed = {"id": "closed-request", "agent": lead["id"], "status": "answered",
                          "created": 1.0, "method": "agent/asyncQuestion", "params": {}}
                canvas = Canvas(root)
                canvas.runtime = runtime
                with runtime.lock, runtime.db() as db:
                    runtime.put(db, "agents", deleted)
                    runtime.put(db, "requests", closed)
                    db.execute("DELETE FROM sync_entities")
                    db.execute("DELETE FROM sync_entity_meta")
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    self.assertIsNone(db.execute(
                        "SELECT 1 FROM sync_entities WHERE collection='agent' AND id='deleted-agent'").fetchone())
                    self.assertIsNone(db.execute(
                        "SELECT 1 FROM sync_entities WHERE collection='request' AND id='closed-request'").fetchone())
            finally:
                runtime.close()

    def test_malformed_runtime_rows_do_not_abort_seed_or_upgrade(self):
        fixture = runtime_fixture()
        with tempfile.TemporaryDirectory(prefix="sync-state-malformed-runtime-") as directory:
            root = Path(directory)
            runtime = Runtime(root, fixture.FakeServer)
            try:
                lead = runtime.create({"name": "Lead", "cwd": str(root), "prompt": ""},
                                      draft=True, defer=True)
                canvas = Canvas(root)
                canvas.runtime = runtime
                with runtime.lock, runtime.db() as db:
                    from codex_sync_entities import ensure_tables, _REPORTED_BAD_ENTITIES
                    ensure_tables(db)
                    db.execute("INSERT INTO runtime_agents(id,record) VALUES(?,?)", ("bad-empty-agent", "{}"))
                    db.execute("INSERT INTO runtime_agents(id,record) VALUES(?,?)", (
                        "bad-lead-agent", json.dumps({"id": "bad-lead-agent", "isLead": True})))
                    db.execute("INSERT INTO runtime_rooms(id,record) VALUES(?,?)", ("bad-array-room", "[]"))
                    db.execute("INSERT INTO runtime_rooms(id,record) VALUES(?,?)", (
                        "bad-members-room", json.dumps({"id": "bad-members-room", "kind": "private",
                            "members": [lead["id"], 7], "updated": 1.0})))
                    db.execute("INSERT INTO runtime_monitors(id,record) VALUES(?,?)", (
                        "bad-id-monitor", json.dumps({"agent": lead["id"], "status": "running", "created": 1.0})))
                    db.execute("INSERT INTO graph_agents(id,record) VALUES(?,?)", ("bad-json-graph", "{"))
                    db.execute("INSERT INTO graph_agents(id,record) VALUES(?,?)", ("bad-object-graph", "{}"))
                    _REPORTED_BAD_ENTITIES.clear()
                    with self.assertLogs("codex_sync_entities", level="WARNING") as captured:
                        seed(db, runtime_owner=runtime, canvas_owner=canvas)
                    self.assertGreaterEqual(len(captured.records), 5)
                    self.assertEqual(db.execute(
                        "SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone()[0],
                        "3")
                    db.execute("DELETE FROM sync_entity_meta WHERE key IN ('seeded','agent_organization_fields')")
                    db.execute("DELETE FROM sync_entities")
                    seed(db, runtime_owner=runtime, canvas_owner=canvas)
                store = SyncStore(canvas.connect, lambda: {}, canvas.transcript,
                                  runtime=runtime, canvas=canvas)
                self.assertIn("documents", store.pull("state:entities:v1", fresh=True))
            finally:
                with runtime.lock, runtime.db() as db:
                    db.execute("DELETE FROM runtime_agents WHERE id IN ('bad-empty-agent','bad-lead-agent')")
                    db.execute("DELETE FROM runtime_rooms WHERE id IN ('bad-array-room','bad-members-room')")
                    db.execute("DELETE FROM runtime_monitors WHERE id='bad-id-monitor'")
                    db.execute("DELETE FROM graph_agents WHERE id IN ('bad-json-graph','bad-object-graph')")
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
