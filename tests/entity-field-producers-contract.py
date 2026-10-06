#!/usr/bin/env python3
"""Runtime writes keep the fields consumed by the entity-backed renderer."""
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()

import importlib.util
import json
import logging
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
            seed(db, lambda: {"runtime": self.runtime.snapshot(db=db), "stateDir": str(self.runtime.root)})

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
        actual = self.entity("agent", self.lead["id"])[field]
        if field == "capacityRetry":
            value = {key: value[key] for key in ("id", "threadId", "epoch", "accountKey", "status",
                                                  "acceptedTurnId", "updatedAt", "dueAt", "claimedAt", "reason")
                     if key in value}
        elif field == "usageResume":
            value = {key: value[key] for key in ("id", "status", "cause", "reason", "updatedAt", "plannedAt", "dueAt")
                     if key in value}
        elif field == "contextRepairWait":
            value = {key: value[key] for key in ("scope", "error") if key in value}
        self.assertEqual(actual, value)

    def test_agent_epoch(self):
        self.write_agent_field("epoch", 17)

    def test_agent_epoch_tracks_real_stop_transition(self):
        self.runtime.stop(self.lead["id"], descendants=False)
        self.assertEqual(self.entity("agent", self.lead["id"])["epoch"],
                         self.runtime.agent(self.lead["id"])["epoch"])

    def test_agent_last_completed_turn_status(self):
        self.write_agent_field("lastCompletedTurnStatus", "completed")

    def test_agent_capacity_retry(self):
        self.write_agent_field("capacityRetry", capacity_retry())
        value = self.entity("agent", self.lead["id"])["capacityRetry"]
        self.assertEqual(set(value), {"id", "threadId", "epoch", "accountKey", "status",
                                     "updatedAt", "dueAt"})

    def test_agent_usage_resume(self):
        self.write_agent_field("usageResume", usage_resume())
        value = self.entity("agent", self.lead["id"])["usageResume"]
        self.assertEqual(set(value), {"id", "status", "cause", "reason", "updatedAt", "plannedAt", "dueAt"})

    def test_agent_context_repair_wait(self):
        self.write_agent_field("contextRepairWait", context_repair_wait("Unavailable", scope="native"))
        value = self.entity("agent", self.lead["id"])["contextRepairWait"]
        self.assertEqual(value, {"error": "Unavailable", "scope": "native"})

    def test_malformed_promoted_nested_fields_do_not_break_agent_writes(self):
        for field, value in (("capacityRetry", {"status": 7}),
                             ("usageResume", {"status": 7}),
                             ("contextRepairWait", {"scope": 7}),
                             ("lastCompletedTurnStatus", 7)):
            with self.subTest(field=field), self.assertLogs("codex_sync_entities", level="WARNING") as logs:
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(self.lead["id"], db)
                    for nested in ("capacityRetry", "usageResume", "contextRepairWait"):
                        agent.pop(nested, None)
                    agent[field] = value
                    self.runtime.put(db, "agents", agent)
                self.assertNotIn(field, self.entity("agent", self.lead["id"]))
                diagnostic = "\n".join(logs.output)
                self.assertIn("field=" + field, diagnostic)
                self.assertIn("error=ValidationError", diagnostic)

    def test_http_agent_responses_match_entity_projection(self):
        from studio_api.agents.models import AgentResponse
        from studio_api.history.models import BranchResponse
        from codex_sync_entities import project

        for field, value in (("lastCompletedTurnStatus", "inProgress"),
                             ("capacityRetry", {"status": 7})):
            with self.subTest(field=field):
                with self.runtime.lock, self.runtime.db() as db:
                    agent = self.runtime.agent(self.lead["id"], db)
                    for nested in ("capacityRetry", "usageResume", "contextRepairWait"):
                        agent.pop(nested, None)
                    agent[field] = value
                    self.runtime.put(db, "agents", agent)
                    view = self.runtime.agent_entity_view(db, agent)
                    response = project("agent", view)
                    entity = json.loads(db.execute(
                        "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                        (agent["id"],)).fetchone()[0])["value"]
                self.assertEqual(response, entity)
                self.assertEqual(AgentResponse.model_validate(response).model_dump(exclude_unset=True), response)
                self.assertEqual(BranchResponse.model_validate(response).model_dump(exclude_unset=True), response)
                if field == "lastCompletedTurnStatus":
                    self.assertEqual(response[field], "inProgress")
                else:
                    self.assertNotIn(field, response)

    def test_upgrade_reprojects_an_agent_with_malformed_promoted_data(self):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.lead["id"], db)
            agent.update(lastCompletedTurnStatus="aborted", capacityRetry={"status": 9},
                         usageResume={"id": "bad-resume", "status": 7},
                         contextRepairWait={"scope": 7})
            db.execute("UPDATE runtime_agents SET record=? WHERE id=?",
                       (json.dumps(agent), self.lead["id"]))
            db.execute("UPDATE sync_entity_meta SET value='1' WHERE key='agent_organization_fields'")

            class BoundBuilder:
                def __init__(builder, runtime):
                    builder.runtime = runtime

                def build(builder):
                    return {"runtime": builder.runtime.snapshot(db=db)}

            from codex_sync_entities import upgrade_agent_organization
            with self.assertLogs("codex_sync_entities", level="WARNING"):
                upgrade_agent_organization(db, BoundBuilder(self.runtime).build)
            marker = db.execute("SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone()[0]
            self.assertEqual(marker, "2")
            entity = json.loads(db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (self.lead["id"],)).fetchone()[0])["value"]
            self.assertEqual(entity["lastCompletedTurnStatus"], "aborted")
            for field in ("capacityRetry", "usageResume", "contextRepairWait"):
                self.assertNotIn(field, entity)

    def test_upgrade_agent_rows_match_runtime_put_projection(self):
        worker = self.runtime.create({"name": "Migration worker", "cwd": self.path,
                                      "prompt": "Preserve the result"}, parent=self.lead["id"], defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            worker_record = self.runtime.agent(worker["id"], db)
            worker_record.update(status="completed", inFlight=False, turnId=None,
                                 lastCompletedTurn="migration-turn", lastAnswer="Migration answer")
            self.runtime.put(db, "agents", worker_record)
            self.runtime.put(db, "work", {"id": "migration-result", "owner": worker["id"],
                "rootId": self.lead["id"], "status": "review", "results": [
                    {"agent": worker["id"], "created": 1, "resultFile": "migration-result.md"}]})
            agent_ids = [row[0] for row in db.execute("SELECT id FROM runtime_agents")]
            for agent_id in agent_ids:
                db.execute("UPDATE sync_entities SET payload=? WHERE collection='agent' AND id=?",
                           (json.dumps({"value": {"id": agent_id}}), agent_id))
            db.execute("UPDATE sync_entity_meta SET value='1' WHERE key='agent_organization_fields'")

            class Builder:
                def __init__(builder, runtime):
                    builder.runtime = runtime

                def build(builder):
                    return {"runtime": builder.runtime.snapshot(db=db), "stateDir": str(builder.runtime.root)}

            from codex_sync_entities import encoded, seed, project
            for agent_id in agent_ids:
                payload, digest, _ = encoded("agent", agent_id, {"id": agent_id})
                db.execute("UPDATE sync_entities SET payload=?,hash=? WHERE collection='agent' AND id=?",
                           (payload, digest, agent_id))
            seed(db, Builder(self.runtime).build)
            for agent_id in agent_ids:
                record = self.runtime.agent(agent_id, db)
                expected = project("agent", self.runtime.agent_entity_view(db, record))
                payload = db.execute("SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                                     (agent_id,)).fetchone()[0]
                actual = json.loads(payload)["value"]
                self.assertEqual(actual, expected, agent_id)
            worker_entity = json.loads(db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?",
                (worker["id"],)).fetchone()[0])["value"]
            self.assertEqual(worker_entity["lastAnswer"], "Migration answer")
            self.assertEqual(worker_entity["overview"]["result"], "Migration answer")
            self.assertEqual(worker_entity["overview"]["resultFile"], "migration-result.md")

    def test_upgrade_without_runtime_source_keeps_marker_and_workspace_untouched(self):
        with self.runtime.lock, self.runtime.db() as db:
            before = db.execute("SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()
            db.execute("DELETE FROM sync_entity_meta WHERE key='agent_organization_fields'")
            from codex_sync_entities import upgrade_agent_organization
            upgrade_agent_organization(db, {"stateDir": str(self.runtime.root), "runtime": None})
            marker = db.execute("SELECT value FROM sync_entity_meta WHERE key='agent_organization_fields'").fetchone()
            after = db.execute("SELECT seq,payload FROM sync_entities WHERE collection='workspace' AND id='current'").fetchone()
            self.assertIsNone(marker)
            self.assertEqual(tuple(after), tuple(before))

    def test_agent_last_event(self):
        self.write_agent_field("lastEvent", "2026-10-06T00:00:00Z")

    def test_agent_account_transfer_id(self):
        self.write_agent_field("accountTransferId", str(uuid.uuid4()))

    def test_agent_workspace_operation(self):
        with self.runtime.lock, self.runtime.db() as db:
            agent = self.runtime.agent(self.lead["id"], db)
            self.runtime._reserve_checkpoint(db, agent, "checkpoint", "entity-field-turn")
        self.assertEqual(self.entity("agent", self.lead["id"])["workspaceOperation"], "checkpoint")

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
        self.assertFalse(self.entity("complaint", lead_recipient["id"])["needsUserResponse"])
        self.assertNotIn("needsResponse", self.entity("complaint", user_recipient["id"]))

    def test_needs_user_response_matches_renderer_rule_state_table(self):
        states = (("user", "open", [], True), ("user", "in_progress", [], True),
                  ("user", "resolved", [], False), ("user", "declined", [], False),
                  ("user", "open", [{"author": "user"}], True),
                  ("lead", "open", [], False), ("lead", "in_progress", [], False),
                  ("lead", "resolved", [], False), ("lead", "declined", [], False),
                  ("lead", "open", [{"author": self.lead["id"]}], False))
        for recipient, status, responses, expected in states:
            complaint = {"id": str(uuid.uuid4()), "author": "user", "leadId": self.lead["id"],
                         "recipient": recipient, "status": status, "responses": responses,
                         "sourceType": "user_task" if status == "open" else None}
            view = self.runtime.complaint_entity_view(None, complaint, {self.lead["id"]: self.lead})
            self.assertEqual(view["needsUserResponse"], expected, (recipient, status, responses))

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
        self.assertFalse(self.entity("complaint", complaint["id"])["needsUserResponse"])

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

    def test_upgrade_reprojects_promoted_fields_and_preserves_other_room_rows(self):
        private_missing = "private:" + str(uuid.uuid4())
        broadcast_missing = "broadcast:" + str(uuid.uuid4())
        room_with_last_message = "private:" + str(uuid.uuid4())
        with self.runtime.lock, self.runtime.db() as db:
            from codex_sync_entities import put as entity_put, upgrade_agent_organization

            # Model old-but-live rows whose room views are no longer available.
            private_record = {"id": private_missing, "kind": "private",
                              "members": ["deleted-or-missing-agent"], "updated": 1.0,
                              "customName": "Stale private room"}
            broadcast_record = {"id": broadcast_missing, "kind": "broadcast",
                                "rootId": "deleted-or-missing-lead", "updated": 1.0}
            self.runtime.put(db, "rooms", private_record)
            self.runtime.put(db, "rooms", broadcast_record)
            entity_put(db, "room", private_missing, private_record)
            entity_put(db, "room", broadcast_missing, broadcast_record)

            # A richer normal room write includes lastMessage. The migration
            # must leave these non-federated payload bytes and sequence intact.
            room_record = {"id": room_with_last_message, "kind": "private",
                           "members": [self.lead["id"]], "updated": 2.0}
            db.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) "
                       "VALUES(?,?,?,?,?,?)", ("seed-last-message", room_with_last_message,
                       self.lead["id"], "preview", 2.0, "{}"))
            self.runtime.put(db, "rooms", room_record, include_last_message=True)
            self.assertFalse(self.runtime.chat_rooms(db, room_id=private_missing))
            self.assertFalse(self.runtime.chat_rooms(db, room_id=broadcast_missing))
        federated_id, members = self.write_federated_room()
        complaint = self.submit_complaint(key="upgrade-promoted-complaint")
        self.write_project_worker_base()
        worker = self.runtime.create({"name": "Upgrade worker", "cwd": self.path,
            "prompt": "Keep overview"}, parent=self.lead["id"], defer=True)
        with self.runtime.lock, self.runtime.db() as db:
            worker_record = self.runtime.agent(worker["id"], db)
            worker_record.update(status="completed", inFlight=False, turnId=None,
                                 lastCompletedTurn="upgrade-turn", lastAnswer="Upgrade answer")
            self.runtime.put(db, "agents", worker_record)
            self.runtime.put(db, "work", {"id": "upgrade-work", "owner": worker["id"],
                "rootId": self.lead["id"], "status": "review", "results": [
                    {"agent": worker["id"], "created": 1, "resultFile": "upgrade-result.md"}]})
            worker_row = db.execute(
                "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?", (worker["id"],)
            ).fetchone()
            worker_value = json.loads(worker_row[0])["value"]
            worker_value.pop("epoch", None)
            worker_value["overview"].pop("resultFile", None)
            entity_put(db, "agent", worker["id"], worker_value)
            current_row = db.execute(
                "SELECT payload FROM sync_entities WHERE collection='room' AND id=?", (federated_id,)
            ).fetchone()
            current = json.loads(current_row[0])["value"]
            entity_put(db, "room", federated_id,
                       {key: value for key, value in current.items()
                        if key not in {"peerLabel", "localMembers"}})
            complaint_row = db.execute(
                "SELECT payload FROM sync_entities WHERE collection='complaint' AND id=?", (complaint["id"],)
            ).fetchone()
            complaint_value = json.loads(complaint_row[0])["value"]
            entity_put(db, "complaint", complaint["id"],
                       {key: value for key, value in complaint_value.items()
                        if key not in {"needsUserResponse", "title", "authorName", "leadName"}})
            project_row = db.execute(
                "SELECT payload FROM sync_entities WHERE collection='project' AND id=?", (self.path,)
            ).fetchone()
            project_value = json.loads(project_row[0])["value"]
            entity_put(db, "project", self.path,
                       {key: value for key, value in project_value.items()
                        if key not in {"workerBaseRef", "workerBaseRevision"}})
            db.execute("UPDATE sync_entity_meta SET value='1' WHERE key='agent_organization_fields'")
            before = {(row[0], row[1]): (row[2], row[3]) for row in db.execute(
                "SELECT collection,id,seq,payload FROM sync_entities")}
            before_federated = db.execute(
                "SELECT seq FROM sync_entities WHERE collection='room' AND id=?", (federated_id,)).fetchone()[0]
            upgrade_agent_organization(db,
                {"runtime": self.runtime.snapshot(db=db), "stateDir": str(self.runtime.root)}, self.runtime)
            after = {(row[0], row[1]): (row[2], row[3]) for row in db.execute(
                "SELECT collection,id,seq,payload FROM sync_entities")}
            after_federated = db.execute(
                "SELECT seq FROM sync_entities WHERE collection='room' AND id=?", (federated_id,)).fetchone()[0]
        changed_pairs = {key for key in before.keys() & after.keys()
                         if before[key][0] != after[key][0]}
        changed_field_names = {}
        for key in changed_pairs:
            old_value = json.loads(before[key][1]).get("value", {})
            new_value = json.loads(after[key][1]).get("value", {})
            names = {name for name in old_value.keys() | new_value.keys()
                     if old_value.get(name) != new_value.get(name)}
            if "overview" in names:
                old_overview = old_value.get("overview") or {}
                new_overview = new_value.get("overview") or {}
                names.remove("overview")
                names.update("overview." + name for name in old_overview.keys() | new_overview.keys()
                             if old_overview.get(name) != new_overview.get(name))
            changed_field_names[key] = names
        expected_changed = {("room", federated_id), ("agent", worker["id"]),
                            ("agent", self.lead["id"]),
                            ("complaint", complaint["id"]), ("project", self.path)}
        self.assertEqual(changed_pairs, expected_changed, changed_field_names)
        for key, names in changed_field_names.items():
            promoted = {
                "agent": {"epoch", "lastCompletedTurnStatus", "capacityRetry", "usageResume",
                          "contextRepairWait", "lastEvent", "accountTransferId",
                          "workspaceOperation", "overview"},
                "room": {"peerLabel", "localMembers"},
                "complaint": {"needsUserResponse", "title", "authorName", "leadName"},
                "project": {"workerBaseRef", "workerBaseRevision"},
            }[key[0]]
            self.assertTrue(names and all(name.split(".", 1)[0] in promoted for name in names),
                            (key, names))
        for room_id in (private_missing, broadcast_missing, room_with_last_message):
            key = ("room", room_id)
            self.assertEqual(after[key], before[key])
        self.assertGreater(after_federated, before_federated)
        self.assertEqual(self.entity("room", federated_id)["peerLabel"], "Peer")
        self.assertEqual(self.entity("room", federated_id)["localMembers"], members)
        worker_entity = self.entity("agent", worker["id"])
        self.assertEqual(worker_entity["lastAnswer"], "Upgrade answer")
        self.assertEqual(worker_entity["overview"]["result"], "Upgrade answer")
        self.assertEqual(worker_entity["overview"]["resultFile"], "upgrade-result.md")

    def test_project_result_file_tracks_work_write(self):
        worker_id = str(uuid.uuid4())
        next_worker_id = str(uuid.uuid4())
        with self.runtime.lock, self.runtime.db() as db:
            for agent_id in (worker_id, next_worker_id):
                self.runtime.put(db, "agents", {"id": agent_id, "rootId": self.lead["id"],
                    "parentId": self.lead["id"], "isLead": False, "prompt": "Work", "status": "completed",
                    "lastCompletedTurn": "turn", "autoWake": False})
            self.runtime.put(db, "work", {"id": "work-result", "rootId": self.lead["id"],
                "owner": worker_id, "status": "review", "title": "Work", "results": [
                    {"agent": worker_id, "created": 1, "resultFile": "result.md"}]})
        self.assertEqual(self.entity("agent", worker_id)["overview"]["resultFile"], "result.md")
        with self.runtime.lock, self.runtime.db() as db:
            work = self.runtime.records(db, "work")[0]
            work["owner"] = next_worker_id
            work["results"] = [{"agent": next_worker_id, "created": 2, "resultFile": "next-result.md"}]
            self.runtime.put(db, "work", work)
        self.assertIsNone(self.entity("agent", worker_id)["overview"]["resultFile"])
        self.assertEqual(self.entity("agent", next_worker_id)["overview"]["resultFile"], "next-result.md")
        with self.runtime.lock, self.runtime.db() as db:
            work = self.runtime.records(db, "work")[0]
            work["results"] = []
            self.runtime.put(db, "work", work)
        self.assertIsNone(self.entity("agent", next_worker_id)["overview"]["resultFile"])

    def test_workspace_connected_updates_on_desktop_change(self):
        server = Mock()
        server.closed = False
        server.join_callbacks.return_value = True
        self.runtime.servers["default"] = server
        self.runtime.offline_accounts.discard("default")
        self.runtime._publish_desktop_resource()
        self.assertFalse(self.entity("workspace", "current")["connected"])
        self.write_agent_field("lastEvent", "2026-10-06T01:00:00Z")
        self.assertTrue(self.entity("workspace", "current")["connected"])

    def test_workspace_connected_updates_on_disconnect(self):
        server = Mock()
        server.closed = False
        server.join_callbacks.return_value = True
        self.runtime.servers["default"] = server
        self.runtime.offline_accounts.discard("default")
        self.runtime.connection_ids["default"] = "connection"
        self.write_agent_field("lastEvent", "2026-10-06T01:00:00Z")
        self.assertTrue(self.entity("workspace", "current")["connected"])
        self.runtime.disconnected("default", "connection")
        self.assertFalse(self.entity("workspace", "current")["connected"])
        self.write_agent_field("lastEvent", "2026-10-06T01:00:01Z")
        self.assertFalse(self.entity("workspace", "current")["connected"])

    def test_workspace_refresh_is_best_effort_under_write_lock(self):
        blocker = self.runtime.db_path
        conn = __import__("sqlite3").connect(blocker, timeout=0)
        try:
            conn.execute("BEGIN IMMEDIATE")
            with self.runtime.db(busy_timeout=0) as db:
                self.assertFalse(self.runtime.sync_workspace_volatile(db))
            started = __import__("time").monotonic()
            self.assertFalse(self.runtime.refresh_workspace_volatile())
            self.runtime._publish_desktop_resource()
            self.assertLess(__import__("time").monotonic() - started, 0.5)
        finally:
            conn.rollback()
            conn.close()

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

    def test_provider_version_notice_refreshes_workspace_entity(self):
        from codex_provider_versions import ProviderVersionMonitor
        server = Mock()
        server.provider = "codex"
        self.runtime.servers["default"] = server
        self.runtime.connection_ids["default"] = "provider-version-test"
        self.runtime.offline_accounts.discard("default")
        monitor = ProviderVersionMonitor(interval=0,
            read_version=lambda provider, current, account: {"runningVersion": "0.1.0"})
        self.runtime.provider_version_monitor = monitor
        monitor.tick(self.runtime)
        monitor.worker.join(2)
        notices = self.entity("workspace", "current")["nativeNotices"]
        self.assertEqual(len(notices), 1)
        self.assertEqual(notices[0]["version"], "0.1.0")

    def test_peer_team_save_writes_entity(self):
        peer = self.runtime.create({"name": "Peer", "cwd": self.path,
                                     "prompt": "Join the team"}, defer=True)
        team_id = str(uuid.uuid4())
        manage(self.runtime, {"action": "save", "path": self.path, "team_id": team_id,
            "members": [self.lead["id"], peer["id"]], "name": "Peers",
            "expected_revision": 0, "request_id": str(uuid.uuid4())})
        entity = self.entity("peerTeam", team_id)
        self.assertEqual(entity["members"], [self.lead["id"], peer["id"]])

    def test_peer_team_entity_tracks_lead_archive_and_restore(self):
        peer = self.runtime.create({"name": "Peer", "cwd": self.path, "prompt": "Join"}, defer=True)
        team_id = str(uuid.uuid4())
        manage(self.runtime, {"action": "save", "path": self.path, "team_id": team_id,
            "members": [self.lead["id"], peer["id"]], "name": "Peers",
            "expected_revision": 0, "request_id": str(uuid.uuid4())})
        for deleted in (True, False):
            with self.runtime.lock, self.runtime.db() as db:
                member = self.runtime.agent(peer["id"], db)
                member["deletedAt"] = 1.0 if deleted else None
                self.runtime.put(db, "agents", member)
            with self.runtime.db() as db:
                row = db.execute("SELECT deleted FROM sync_entities WHERE collection='peerTeam' AND id=?",
                                 (team_id,)).fetchone()
            self.assertEqual(row[0], int(deleted))

    def test_peer_team_entity_is_tombstoned_when_project_is_removed(self):
        peer = self.runtime.create({"name": "Peer", "cwd": self.path, "prompt": "Join"}, defer=True)
        team_id = str(uuid.uuid4())
        manage(self.runtime, {"action": "save", "path": self.path, "team_id": team_id,
            "members": [self.lead["id"], peer["id"]], "name": "Peers",
            "expected_revision": 0, "request_id": str(uuid.uuid4())})
        self.runtime.projects({"action": "remove", "path": self.path})
        with self.runtime.db() as db:
            row = db.execute("SELECT deleted FROM sync_entities WHERE collection='peerTeam' AND id=?",
                             (team_id,)).fetchone()
        self.assertEqual(row[0], 1)


if __name__ == "__main__":
    unittest.main()
