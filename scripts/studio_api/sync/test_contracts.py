"""Colocated contracts and projection tests for durable sync DTOs."""

from __future__ import annotations

import json
import unittest

from pydantic import ValidationError
from fastapi import Depends, FastAPI
from fastapi.testclient import TestClient

from codex_sync_entities import COLLECTION_FIELDS, project, validate_entity_payload
from studio_api.sync.models import (
    AgentEntityDto,
    DraftPushRequest,
    EntityCollection,
    RuntimeSnapshot,
    SnapshotAgentDto,
    StateSnapshot,
    SyncDocument,
    SyncEntityPayload,
    SyncPullQuery,
)


class SyncEntityContractTests(unittest.TestCase):
    def test_projection_fields_come_from_models(self) -> None:
        self.assertIn("accountTransfer", AgentEntityDto.model_fields)
        self.assertEqual(EntityCollection.PEER_TEAM.value, "peerTeam")
        self.assertIn("id", COLLECTION_FIELDS["task"])
        agent = project("agent", {"id": "a", "status": "running", "private": "excluded"})
        self.assertEqual(agent, {"id": "a", "status": "running"})

    def test_strict_agent_projection_rejects_unknown_status(self) -> None:
        with self.assertRaises(ValidationError):
            AgentEntityDto.model_validate({"id": "a", "status": "new-unreviewed-status"})

    def test_legacy_status_file_public_row_has_named_external_fields(self) -> None:
        row = {
            "id": "wave:run:worker", "name": "Worker", "threadId": "thread-id",
            "runId": "run-id", "wave": "wave", "launcherPid": 123,
            "turnStatus": "running", "status": "running", "goalStatus": "active",
            "agentOwner": "wave:run:worker", "role": "reviewer", "kind": "agent",
            "source": "app-server", "branch": "branch-name", "cwd": "/workspace",
            "requestedModel": "model", "model": "model", "effort": "high",
            "events": 1, "lastEvent": "2026-10-04T00:00:00Z", "tokensUsed": 4,
            "error": None, "tail": "", "capacityRetries": 0, "capacityRetry": None,
            "pendingSubmission": None, "lastTurnStatus": "inProgress",
            "currentMessageId": None, "mailboxError": None, "launcherAlive": True,
            "canSend": True,
        }
        parsed = SnapshotAgentDto.model_validate(row)
        self.assertEqual(parsed.agentOwner, "wave:run:worker")

    def test_sync_document_preserves_wire_alias(self) -> None:
        document = SyncDocument.model_validate(
            {"id": "entity:agent:a", "payload": "{}", "seq": 9, "_deleted": False}
        )
        self.assertEqual(document.model_dump(by_alias=True), {
            "id": "entity:agent:a", "payload": "{}", "seq": 9, "_deleted": False
        })
        with self.assertRaises(ValidationError):
            SyncDocument.model_validate({"id": "x", "payload": "{}", "seq": 0, "_deleted": 0})

    def test_serialized_entity_payload_is_checked_against_collection_dto(self) -> None:
        payload = json.dumps({"collection": "agent", "id": "a", "value": {"id": "a", "status": "running"}})
        envelope = validate_entity_payload(payload)
        self.assertEqual(envelope.collection, EntityCollection.AGENT)
        invalid = json.dumps({"collection": "agent", "id": "a", "value": {"id": "a", "internal": True}})
        with self.assertRaises(ValidationError):
            validate_entity_payload(invalid)

    def test_draft_batch_is_fully_validated_before_service_call(self) -> None:
        row = {
            "newDocumentState": {
                "id": "device:lead",
                "payload": json.dumps({
                    "id": "device:lead", "device": "device", "session": "lead", "text": "draft"
                }),
                "_deleted": False,
            }
        }
        request = DraftPushRequest.model_validate({"rows": [row]})
        self.assertEqual(len(request.rows), 1)
        invalid = {"rows": [row, {"newDocumentState": {"id": "wrong", "payload": "null"}}]}
        with self.assertRaises(ValidationError):
            DraftPushRequest.model_validate(invalid)

    def test_chat_and_full_snapshot_models_share_runtime_contract(self) -> None:
        active_agent = {
            "id": "agent-a", "status": "running", "activity": {
                "phase": "tool", "at": 1.5,
                "tools": [{"id": "tool-a", "type": "commandExecution", "name": "exec"}],
            },
            "nativeRelease": {
                "id": "release-a", "phase": "unsubscribing", "threadId": "thread-a",
                "accountKey": "default", "connectionId": "conn-a", "at": 1.0,
                "submittedAt": 1.2, "resetPending": True,
            },
            "nativeStatus": "notLoaded",
            "startAttempt": {
                "id": "attempt-a", "epoch": 4, "events": ["event-a"],
                "action": "resume", "submitted": True, "activeAtReservation": True,
                "observedTurnId": "turn-a", "nativeOperationId": "native-a",
                "prepareError": "prepare details", "responseError": "response details",
            },
            "workerDefaults": {
                "model": "gpt-6-luna", "effort": "high", "fastMode": False,
                "daybreakEnabled": False, "accountKey": "default",
            },
            "reviewDefaults": {"model": None, "effort": None},
            "pendingSettings": {"model": "gpt-6-luna", "effort": "medium", "fastMode": True},
        }
        runtime = {
            "agents": [active_agent], "projects": [], "projectOrganizationVersion": 1,
            "peerTeamsVersion": 1, "peerTeams": [], "tasks": [], "tasksHistoryLimit": 100,
            "monitors": [], "requests": [], "rooms": [], "complaints": [], "rules": [],
            "rateLimits": {}, "nativeNotices": [], "rateLimitsByAccount": {}, "events": [],
            "connected": True,
        }
        snapshot = StateSnapshot.model_validate({
            "token": "session", "stateDir": "/state", "threads": [], "chats": [],
            "nodes": [], "edges": [], "at": 1.0, "runtime": runtime,
        })
        if snapshot.runtime is None:
            self.fail("snapshot runtime was omitted")
        self.assertIsNone(snapshot.runtime.work)
        active = snapshot.runtime.agents[0]
        if (active.activity is None or active.activity.tools is None or not active.activity.tools
                or active.nativeRelease is None or active.startAttempt is None or active.workerDefaults is None):
            self.fail("active producer nested values were omitted")
        self.assertEqual(active.activity.at, 1.5)
        self.assertEqual(active.activity.tools[0].name, "exec")
        self.assertEqual(active.nativeRelease.connectionId, "conn-a")
        self.assertEqual(active.startAttempt.events, ["event-a"])
        self.assertEqual(active.workerDefaults.model, "gpt-6-luna")
        with self.assertRaises(ValidationError):
            SnapshotAgentDto.model_validate({**active_agent, "activity": {"unknown": True}})
        runtime["work"] = []
        full = RuntimeSnapshot.model_validate(runtime)
        self.assertEqual(full.work, [])

    def test_query_schema_documents_numeric_cursors_and_wire_flags(self) -> None:
        app = FastAPI()

        @app.get("/api/sync/pull")
        def get_pull(query: SyncPullQuery = Depends()) -> dict[str, object]:
            return {
                "after": query.after,
                "limit": query.limit,
                "fresh": query.fresh,
                "reset": query.reset,
                "priorityId": query.priorityId,
            }

        document = TestClient(app).get("/api/sync/pull?after=9&limit=20&fresh=1&reset=1&priorityId=lead")
        self.assertEqual(document.json(), {
            "after": 9, "limit": 20, "fresh": "1", "reset": "1", "priorityId": "lead"
        })
        parameters = app.openapi()["paths"]["/api/sync/pull"]["get"]["parameters"]
        types = {
            parameter["name"]: {
                option["type"] for option in parameter["schema"].get("anyOf", [parameter["schema"]])
                if option.get("type") != "null"
            }
            for parameter in parameters
        }
        self.assertEqual(types["after"], {"integer"})
        self.assertEqual(types["limit"], {"integer"})
        self.assertEqual(types["fresh"], {"string"})


if __name__ == "__main__":
    unittest.main()
