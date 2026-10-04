"""Colocated contracts and projection tests for durable sync DTOs."""

from __future__ import annotations

import json
import importlib.util
import sys
import tempfile
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import Protocol, cast

from pydantic import ValidationError
from fastapi import Depends, FastAPI
from fastapi.testclient import TestClient

from codex_sync_entities import COLLECTION_FIELDS, project, validate_entity_payload
from studio_api.models import JsonValue
from studio_api.sync.models import (
    AgentEntityDto,
    AgentNativeStatus,
    DraftPushRequest,
    EntityCollection,
    RuntimeSnapshot,
    RequestEntityDto,
    RoomRadioSeen,
    SnapshotAgentDto,
    StateSnapshot,
    SyncDocument,
    SyncEntityPayload,
    SyncPullQuery,
)


class RuntimeFixture(Protocol):
    def create(self, data: dict[str, object], parent: str | None = None, defer: bool = False,
               parent_epoch: int | None = None, draft: bool = False) -> dict[str, JsonValue]: ...
    def close(self) -> None: ...


class RadioFixture(Protocol):
    def setUp(self) -> None: ...
    def tearDown(self) -> None: ...
    def doCleanups(self) -> None: ...
    def room(self) -> dict[str, JsonValue]: ...
    def action(self, action: str, **extra: JsonValue) -> JsonValue: ...
    def start(self) -> tuple[str, str]: ...
    def tick(self) -> dict[str, JsonValue]: ...
    def answer(
        self,
        key: str,
        turn: str,
        text: str = "reply",
        item: str = "answer",
        status: str = "completed",
    ) -> JsonValue: ...


class SyncEntityContractTests(unittest.TestCase):
    def test_projection_fields_come_from_models(self) -> None:
        self.assertIn("accountTransfer", AgentEntityDto.model_fields)
        self.assertEqual(EntityCollection.PEER_TEAM.value, "peerTeam")
        self.assertIn("id", COLLECTION_FIELDS["task"])
        source: dict[str, JsonValue] = {
            "id": "a",
            "status": "running",
            "private": "excluded",
        }
        agent = project("agent", source)
        self.assertEqual(agent, {"id": "a", "status": "running"})

    def test_runtime_draft_create_persists_boolean_worktree_projection(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        test_directory = repository / "tests"
        test_path = test_directory / "runtime-contract.py"
        sys.path.insert(0, str(test_directory))
        try:
            spec = importlib.util.spec_from_file_location("sync_runtime_fixture", test_path)
            if spec is None or spec.loader is None:
                self.fail("runtime fixture could not be loaded")
            fixture = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(fixture)
        finally:
            sys.path.remove(str(test_directory))

        runtime_factory = cast(
            Callable[[Path, Callable[..., object]], RuntimeFixture],
            getattr(fixture, "Runtime"),
        )
        fake_server = cast(Callable[..., object], getattr(fixture, "FakeServer"))
        with tempfile.TemporaryDirectory(prefix="sync-agent-projection-") as temporary:
            runtime = runtime_factory(Path(temporary), fake_server)
            try:
                agent = runtime.create(
                    {"name": "Draft Lead", "prompt": "", "cwd": temporary}, draft=True
                )
                projected = project("agent", agent)
                if not isinstance(projected, dict):
                    self.fail("runtime agent did not project")
                self.assertIs(projected.get("worktree"), False)
            finally:
                runtime.close()

    def test_runtime_radio_open_projects_persisted_seen_cursor(self) -> None:
        repository = Path(__file__).resolve().parents[3]
        test_path = repository / "tests" / "radio-contract.py"
        spec = importlib.util.spec_from_file_location("sync_radio_fixture", test_path)
        if spec is None or spec.loader is None:
            self.fail("radio fixture could not be loaded")
        fixture = importlib.util.module_from_spec(spec)
        test_directory = str(test_path.parent)
        sys.path.insert(0, test_directory)
        try:
            spec.loader.exec_module(fixture)
        finally:
            sys.path.remove(test_directory)
        radio_factory = cast(Callable[[], RadioFixture], getattr(fixture, "Radio"))
        radio = radio_factory()
        radio.setUp()
        try:
            projected = project("room", radio.room())
            if not isinstance(projected, dict):
                self.fail("runtime radio room did not project")
            public_radio = projected.get("radio")
            if not isinstance(public_radio, dict):
                self.fail("runtime radio record did not project")
            self.assertEqual(public_radio.get("seen"), {})

            radio.action("send", text="Record a shared reply")
            agent_id, turn_id = radio.start()
            radio.answer(agent_id, turn_id)
            updated = project("room", radio.room())
            if not isinstance(updated, dict):
                self.fail("updated runtime radio room did not project")
            updated_radio = updated.get("radio")
            if not isinstance(updated_radio, dict):
                self.fail("updated runtime radio record did not project")
            self.assertEqual(
                updated_radio.get("seen"),
                {agent_id: {"identity": ["native-" + agent_id, 1, 0], "seq": 1}},
            )

            radio.action("send", text="Stop while this shared turn is running")
            radio.start()
            radio.action("stop")
            radio.tick()
            stopping = project("room", radio.room())
            if not isinstance(stopping, dict):
                self.fail("stopping shared radio did not project")
            public_radio = stopping.get("radio")
            if not isinstance(public_radio, dict):
                self.fail("stopping shared radio did not project")
            active = public_radio.get("active")
            self.assertIsInstance(active, dict)
            assert isinstance(active, dict)
            self.assertIs(active.get("interruptRequested"), True)
        finally:
            radio.tearDown()
            radio.doCleanups()

    def test_radio_seen_identity_has_exact_runtime_types(self) -> None:
        valid = RoomRadioSeen.model_validate({"identity": ["native-a", 3, 1], "seq": 9})
        self.assertEqual(
            valid.model_dump(mode="json"),
            {"identity": ["native-a", 3, 1], "seq": 9},
        )
        for identity in (["native-a", False, 1], ["native-a", 3, "1"], ["native-a", 3]):
            with self.subTest(identity=identity):
                with self.assertRaises(ValidationError):
                    RoomRadioSeen.model_validate({"identity": identity, "seq": 9})

    def test_strict_agent_projection_rejects_unknown_status(self) -> None:
        with self.assertRaises(ValidationError):
            AgentEntityDto.model_validate({"id": "a", "status": "new-unreviewed-status"})
        model_specific_effort = AgentEntityDto.model_validate({"id": "a", "effort": "minimal"})
        self.assertEqual(model_specific_effort.effort, "minimal")
        resumed = AgentEntityDto.model_validate({
            "id": "a", "nativeRelease": {"phase": "resumed", "resetPending": False},
        })
        if resumed.nativeRelease is None or resumed.nativeRelease.phase is None:
            self.fail("resumed native release phase was omitted")
        self.assertEqual(resumed.nativeRelease.phase.value, "resumed")

    def test_projection_preserves_typed_runtime_and_request_receipts(self) -> None:
        provider_error: dict[str, JsonValue] = {
            "code": -32000, "message": "Native thread is blocked",
            "codexErrorInfo": "misalignmentPolicyViolation",
            "providerDetail": {"phase": "policy"},
        }
        source: dict[str, JsonValue] = {
            "id": "agent-a", "status": "interrupted", "threadId": "thread-a",
            "turnId": "turn-a", "epoch": 4, "accountKey": "default",
            "contextUsage": {"tokens": 120, "window": None, "at": 10.5},
            "nativeThreadBlock": {"threadId": "thread-a", "error": provider_error},
            "nativeTurnError": {"turnId": "turn-a", "error": provider_error},
            "nativeSafetyBuffering": {
                "turnId": "turn-a", "threadId": "thread-a", "showBufferingUi": True,
                "responseStarted": False, "dismissed": False, "at": 10.0,
                "fasterModel": "gpt-6-mini", "accountKey": "default", "connectionId": "c-1",
            },
            "nativeSafetyRetry": {
                "id": "retry-a", "stage": "failed", "model": "gpt-6-mini",
                "turnId": "turn-a", "epoch": 4, "accountKey": "default",
                "error": "No retry was sent", "rpcMethod": "thread/fork",
            },
            "connectionCheck": {
                "epoch": 4, "accountKey": "default", "threadId": "thread-a",
                "turnId": "turn-a", "at": 11.0, "previousError": "Codex disconnected.",
                "nativeState": "idle", "restartTurnStatus": "completed",
            },
            "readState": {"threadId": "thread-a", "turnId": "turn-a", "read": True, "revision": 3},
            "nativeLimitErrorAt": 12.0,
            "activity": {"phase": "tool", "at": 12.5,
                         "tools": [{"id": "tool-a", "type": "commandExecution", "name": "exec"}]},
            "nativeStatus": {"phase": "retrying", "error": provider_error,
                             "turnId": "turn-a", "at": 12.5},
            "capacityRetry": {
                "id": "capacity-a", "threadId": "thread-a", "turnId": "turn-a",
                "accountKey": "default", "epoch": 4, "cause": "serverOverloaded",
                "status": "scheduled", "dueAt": 20.0, "attempt": 1, "maxAttempts": 4,
                "settings": {"model": "gpt-6-mini"}, "taskClaims": ["work-a"],
            },
            "usageResume": {
                "id": "usage-a", "status": "scheduled", "accountKey": "default",
                "threadId": "thread-a", "epoch": 4, "turnId": "turn-a",
                "cause": "usage_limit", "failedAt": 10.0, "dueAt": 30.0,
                "taskClaims": ["work-a"],
            },
            "lastEvent": "2026-10-04T03:00:00Z",
            "privateRuntimeField": "not projected",
        }
        projected = project("agent", source)
        if not isinstance(projected, dict):
            self.fail("runtime agent did not project")
        self.assertNotIn("privateRuntimeField", projected)
        parsed = AgentEntityDto.model_validate(projected)
        self.assertIsNotNone(parsed.contextUsage)
        self.assertIsNotNone(parsed.nativeSafetyRetry)
        self.assertIsNotNone(parsed.nativeStatus)
        assert parsed.contextUsage is not None
        assert parsed.nativeSafetyRetry is not None
        assert parsed.nativeStatus is not None
        self.assertIsNone(parsed.contextUsage.window)
        self.assertEqual(parsed.nativeSafetyRetry.stage, "failed")
        self.assertIsInstance(parsed.nativeStatus, AgentNativeStatus)
        assert isinstance(parsed.nativeStatus, AgentNativeStatus)
        self.assertEqual(parsed.nativeStatus.phase, "retrying")

        snapshot_source = {key: value for key, value in source.items() if key != "privateRuntimeField"}
        snapshot = SnapshotAgentDto.model_validate(snapshot_source)
        parsed = snapshot
        self.assertIsNotNone(parsed.capacityRetry)
        self.assertIsNotNone(parsed.usageResume)
        assert parsed.capacityRetry is not None
        assert parsed.usageResume is not None
        self.assertEqual(parsed.capacityRetry.taskClaims, ["work-a"])
        self.assertEqual(parsed.usageResume.cause, "usage_limit")
        self.assertEqual(parsed.lastEvent, "2026-10-04T03:00:00Z")

        request_source: dict[str, JsonValue] = {
            "id": "request-a", "method": "mcpServer/elicitation/request", "agent": "agent-a",
            "epoch": 4, "status": "pending", "deferred": False,
            "params": {
                "threadId": "thread-a", "mode": "form",
                "requestedSchema": {"type": "object", "required": ["scope"], "properties": {
                    "scope": {"type": "string", "title": "Scope"},
                    "features": {"type": "array", "items": {"type": "string", "enum": ["Tests"]}},
                }},
            },
            "preview": {"command": ["git", "status"], "cwd": "/workspace"},
        }
        request = project("request", request_source)
        self.assertIsInstance(request, dict)
        RequestEntityDto.model_validate(request)

        runtime_snapshot = RuntimeSnapshot.model_validate({
            "agents": [snapshot_source], "projects": [], "projectOrganizationVersion": 1,
            "peerTeamsVersion": 1, "peerTeams": [], "tasks": [], "tasksHistoryLimit": 100,
            "monitors": [], "requests": [request_source], "rooms": [], "complaints": [],
            "rules": [], "rateLimits": {}, "nativeNotices": [],
            "rateLimitsByAccount": {"default": {
                "accountKey": "default", "at": None, "data": None, "error": None,
            }},
            "events": [], "connected": True,
        })
        self.assertEqual(runtime_snapshot.requests[0].epoch, 4)
        state = StateSnapshot.model_validate({
            "token": "session", "stateDir": "/state", "threads": [snapshot_source],
            "chats": [], "nodes": [snapshot_source], "edges": [], "at": 15.0,
            "runtime": runtime_snapshot.model_dump(mode="json"),
        })
        self.assertIsNotNone(state.runtime)

    def test_sync_projection_rejects_invalid_typed_provider_fields(self) -> None:
        with self.assertRaises(ValidationError):
            project("agent", {"id": "agent-a", "nativeSafetyRetry": {"stage": "other"}})
        with self.assertRaises(ValidationError):
            project("agent", {"id": "agent-a", "contextUsage": {"tokens": "many", "window": 1}})

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
            "overview": {"task": "task", "result": "result", "resultFile": "/tmp/result.md"},
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
                or active.nativeRelease is None or active.startAttempt is None or active.workerDefaults is None
                or active.overview is None):
            self.fail("active producer nested values were omitted")
        self.assertEqual(active.activity.at, 1.5)
        self.assertEqual(active.activity.tools[0].name, "exec")
        self.assertEqual(active.nativeRelease.connectionId, "conn-a")
        self.assertEqual(active.startAttempt.events, ["event-a"])
        self.assertEqual(active.workerDefaults.model, "gpt-6-luna")
        self.assertEqual(active.overview.resultFile, "/tmp/result.md")
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
