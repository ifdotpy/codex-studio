#!/usr/bin/env python3
"""Runtime writes keep the fields consumed by the entity-backed renderer."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import uuid
from unittest.mock import Mock

root = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("runtime_fixture", root / "tests/runtime-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)
from codex_peer_teams import manage
from codex_runtime import Runtime
from entity_test_support import capacity_retry, context_repair_wait, usage_resume


class QuietRuntime(Runtime):
    def schedule(self):
        while not self.closed:
            self.changed.wait(.1)
            self.changed.clear()


class EntityFieldProducers(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = str(Path(self.tmp.name).resolve())
        self.runtime = QuietRuntime(Path(self.path), fixture.FakeServer)
        self.addCleanup(self.runtime.close)
        self.lead = self.runtime.create({"name": "Lead", "cwd": self.path,
                                         "prompt": "Keep the project moving"}, defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            lead = self.runtime.agent(self.lead["id"], db)
            lead.update(autoWake=True, status="idle")
            self.runtime.put(db, "agents", lead)
            from codex_sync_entities import seed
            seed(db, self.runtime.snapshot)

    def entity(self, collection, key):
        with self.runtime.db() as db:
            row = db.execute("SELECT payload FROM sync_entities WHERE collection=? AND id=?",
                             (collection, key)).fetchone()
            self.assertIsNotNone(row, (collection, key))
            return json.loads(row[0])["value"]

    def write_agent_field(self, field, value):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.lead["id"], db)
            agent[field] = value
            self.runtime.put(db, "agents", agent)
        self.assertEqual(self.entity("agent", self.lead["id"])[field], value)

    def test_agent_epoch(self):
        self.write_agent_field("epoch", 17)

    def test_agent_last_completed_turn_status(self):
        self.write_agent_field("lastCompletedTurnStatus", "completed")

    def test_agent_capacity_retry(self):
        self.write_agent_field("capacityRetry", capacity_retry())

    def test_agent_usage_resume(self):
        self.write_agent_field("usageResume", usage_resume())

    def test_agent_context_repair_wait(self):
        self.write_agent_field("contextRepairWait", context_repair_wait("Unavailable", scope="native"))

    def test_agent_last_event(self):
        self.write_agent_field("lastEvent", "2026-10-06T00:00:00Z")

    def test_agent_account_transfer_id(self):
        self.write_agent_field("accountTransferId", str(uuid.uuid4()))

    def test_agent_workspace_operation(self):
        self.write_agent_field("workspaceOperation", "branch")

    def submit_complaint(self, actor_id=None, key="entity-field-complaint"):
        return self.runtime.complaint(actor_id or self.lead["id"],
                                      {"action": "submit", "text": "Please review this"}, key)

    def active_worker(self):
        worker = self.runtime.create({"name": "Worker", "cwd": self.path, "prompt": "Review"},
                                     parent=self.lead["id"], defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            record = self.runtime.agent(worker["id"], db)
            record.update(autoWake=True, status="idle")
            self.runtime.put(db, "agents", record)
        return worker

    def test_complaint_title(self):
        complaint = self.submit_complaint()
        self.assertEqual(self.entity("complaint", complaint["id"])["title"], "Please review this")

    def test_complaint_author_name(self):
        complaint = self.submit_complaint()
        self.assertEqual(self.entity("complaint", complaint["id"])["authorName"], "Lead")

    def test_complaint_lead_name(self):
        complaint = self.submit_complaint()
        self.assertEqual(self.entity("complaint", complaint["id"])["leadName"], "Lead")

    def test_complaint_author_name_tracks_agent_rename(self):
        complaint = self.submit_complaint()
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.lead["id"], db)
            agent["name"] = "Renamed lead"
            self.runtime.put(db, "agents", agent)
        self.assertEqual(self.entity("complaint", complaint["id"])["authorName"], "Renamed lead")

    def test_complaint_lead_name_tracks_agent_rename(self):
        complaint = self.submit_complaint()
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.lead["id"], db)
            agent["name"] = "Renamed lead"
            self.runtime.put(db, "agents", agent)
        self.assertEqual(self.entity("complaint", complaint["id"])["leadName"], "Renamed lead")

    def test_complaint_needs_user_response_tracks_recipient(self):
        user_recipient = self.submit_complaint()
        worker = self.active_worker()
        lead_recipient = self.submit_complaint(worker["id"], "entity-field-complaint-worker")
        self.assertEqual(self.entity("complaint", user_recipient["id"])["recipient"], "user")
        self.assertEqual(self.entity("complaint", lead_recipient["id"])["recipient"], "lead")
        self.assertTrue(self.entity("complaint", user_recipient["id"])["needsUserResponse"])
        self.assertTrue(self.entity("complaint", lead_recipient["id"])["needsUserResponse"])
        self.assertNotIn("needsResponse", self.entity("complaint", user_recipient["id"]))

    def test_complaint_needs_user_response_tracks_status(self):
        complaint = self.submit_complaint()
        self.runtime.complaint_response_from_user({"complaint_id": complaint["id"],
            "text": "Resolved", "status": "resolved", "version": complaint["version"]},
            "entity-field-user-resolution")
        self.assertFalse(self.entity("complaint", complaint["id"])["needsUserResponse"])

    def test_complaint_needs_user_response_tracks_responses(self):
        complaint = self.submit_complaint()
        self.runtime.complaint_response_from_user({"complaint_id": complaint["id"],
            "text": "Still in progress", "status": "in_progress", "version": complaint["version"]},
            "entity-field-user-progress")
        self.assertFalse(self.entity("complaint", complaint["id"])["needsUserResponse"])

    def test_complaint_needs_user_response_tracks_lead_response(self):
        worker = self.active_worker()
        complaint = self.submit_complaint(worker["id"], "entity-field-lead-response")
        self.runtime.complaint(self.lead["id"], {"action": "respond", "complaint_id": complaint["id"],
            "text": "I have an update", "status": "in_progress"}, "entity-field-lead-answer")
        self.assertTrue(self.entity("complaint", complaint["id"])["needsUserResponse"])

    def write_project_worker_base(self):
        self.runtime.projects({"action": "set_worker_base", "path": self.path,
                              "expected_revision": 0, "base_ref": "main"})
        return self.entity("project", self.path)

    def test_project_worker_base_ref(self):
        self.assertEqual(self.write_project_worker_base()["workerBaseRef"], "main")

    def test_project_worker_base_revision(self):
        self.assertEqual(self.write_project_worker_base()["workerBaseRevision"], 1)

    def write_federated_room(self):
        room_id = "federated:" + str(uuid.uuid4())
        members = [self.lead["id"]]
        with self.runtime.lock, self.runtime.db() as db:
            room = {"id": room_id, "kind": "federated", "members": members,
                    "updated": 1.0, "name": "Peer · Shared room", "federation": True}
            self.runtime.put(db, "rooms", room)
            db.execute("INSERT INTO runtime_federation_rooms(id,record) VALUES(?,?)", (room_id, json.dumps({
                "id": room_id, "kind": "federated", "peerId": "peer", "peerLabel": "Peer",
                "name": "Peer · Shared room", "status": "approved", "localApproved": True,
                "remoteApproved": True, "localMembers": members, "remoteMembers": [],
                "localParticipants": [],
            })))
            self.runtime.put(db, "rooms", room)
        return room_id, members

    def test_room_peer_label(self):
        room_id, _ = self.write_federated_room()
        self.assertEqual(self.entity("room", room_id)["peerLabel"], "Peer")

    def test_room_local_members(self):
        room_id, members = self.write_federated_room()
        self.assertEqual(self.entity("room", room_id)["localMembers"], members)

    def test_project_result_file_tracks_work_write(self):
        worker_id = str(uuid.uuid4())
        with self.runtime.lock, self.runtime.db() as db:
            self.runtime.put(db, "agents", {"id": worker_id, "rootId": self.lead["id"],
                "parentId": self.lead["id"], "isLead": False, "prompt": "Work", "status": "completed",
                "lastCompletedTurn": "turn", "autoWake": False})
            self.runtime.put(db, "work", {"id": "work-result", "rootId": self.lead["id"],
                "owner": worker_id, "status": "review", "title": "Work", "results": [
                    {"agent": worker_id, "created": 1, "resultFile": "result.md"}]})
        self.assertEqual(self.entity("agent", worker_id)["overview"]["resultFile"], "result.md")

    def test_workspace_connected_updates_on_desktop_change(self):
        server = Mock()
        server.closed = False
        server.join_callbacks.return_value = True
        self.runtime.servers["default"] = server
        self.runtime.offline_accounts.discard("default")
        self.runtime._publish_desktop_resource()
        self.assertTrue(self.entity("workspace", "current")["connected"])

    def test_workspace_connected_updates_on_disconnect(self):
        server = Mock()
        server.closed = False
        server.join_callbacks.return_value = True
        self.runtime.servers["default"] = server
        self.runtime.offline_accounts.discard("default")
        self.runtime.connection_ids["default"] = "connection"
        self.runtime.disconnected("default", "connection")
        self.assertFalse(self.entity("workspace", "current")["connected"])

    def test_workspace_native_notices_update_on_desktop_change(self):
        connection_id = "native-notice-connection"
        self.runtime.connection_ids["default"] = connection_id
        self.runtime.offline_accounts.discard("default")
        self.runtime.notification({"method": "mcpServer/oauthLogin/completed", "params": {
            "name": "Fixture tool", "success": False, "error": "Sign-in expired"}},
            "default", connection_id)
        notices = self.entity("workspace", "current")["nativeNotices"]
        self.assertEqual(len(notices), 1)
        self.assertEqual(notices[0]["message"], "Fixture tool: Sign-in expired")
        self.runtime.notification({"method": "mcpServer/oauthLogin/completed", "params": {
            "name": "Fixture tool", "success": True}}, "default", connection_id)
        self.assertEqual(self.entity("workspace", "current")["nativeNotices"], [])

    def test_peer_team_save_writes_entity(self):
        peer = self.runtime.create({"name": "Peer", "cwd": self.path,
                                     "prompt": "Join the team"}, defer=True)
        team_id = str(uuid.uuid4())
        manage(self.runtime, {"action": "save", "path": self.path, "team_id": team_id,
            "members": [self.lead["id"], peer["id"]], "name": "Peers",
            "expected_revision": 0, "request_id": str(uuid.uuid4())})
        entity = self.entity("peerTeam", team_id)
        self.assertEqual(entity["members"], [self.lead["id"], peer["id"]])


if __name__ == "__main__":
    unittest.main()
