"""Colocated contracts and projection tests for durable sync DTOs."""

from __future__ import annotations

import json
import importlib.util
import sqlite3
import sys
import tempfile
import unittest
from collections.abc import Callable
from contextlib import AbstractContextManager
from pathlib import Path
from typing import Protocol, cast

from pydantic import ValidationError
from codex_account_transfer import AccountTransfers
from codex_sync_entities import COLLECTION_FIELDS, project, validate_entity_payload
from studio_api.models import JsonValue
from studio_api.sync.models import (
    AgentEntityDto,
    AgentNativeStatus,
    DraftPushRow,
    DraftPushRequest,
    EntityCollection,
    NativeProviderError,
    RuntimeSnapshot,
    AccountRateLimitsDto,
    RequestEntityDto,
    RoomRadioSeen,
    SnapshotAgentDto,
    SnapshotChatGroupDto,
    StateSnapshot,
    SyncDocument,
    SyncEntityPayload,
)


class RuntimeFixture(Protocol):
    def create(self, data: dict[str, object], parent: str | None = None, defer: bool = False,
               parent_epoch: int | None = None, draft: bool = False) -> dict[str, JsonValue]: ...
    def close(self) -> None: ...


class ReconcileRuntimeFixture(Protocol):
    def create(self, data: dict[str, JsonValue], parent: str | None = None, defer: bool = False,
               parent_epoch: int | None = None, draft: bool = False) -> dict[str, JsonValue]: ...
    def close(self) -> None: ...
    def db(self) -> AbstractContextManager[sqlite3.Connection]: ...
    def agent(
        self, agent_id: str, db: sqlite3.Connection | None = None
    ) -> dict[str, JsonValue]: ...
    def put(self, db: sqlite3.Connection, table: str, record: dict[str, JsonValue]) -> None: ...
    def enqueue(self, db: sqlite3.Connection, agent: dict[str, JsonValue], kind: str,
                text: str, key: str) -> None: ...


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


class TransferRuntimeFixture:
    def __init__(self) -> None:
        self.lead: dict[str, JsonValue] = {"id": "lead-a"}

    def agent(self, agent_id: str, db: object) -> dict[str, JsonValue]:
        del db
        if agent_id != "lead-a":
            raise ValueError("unexpected transfer lead")
        return self.lead

    def put(self, db: object, collection: str, record: dict[str, JsonValue]) -> None:
        del db
        if collection == "account_transfers":
            return
        if collection != "agents":
            raise ValueError("unexpected transfer collection")
        self.lead = record


class SyncEntityContractTests(unittest.TestCase):
    def test_worker_model_inheritance_survives_projection_and_snapshot(self) -> None:
        for model in (None, "gpt-6-luna"):
            with self.subTest(model=model):
                record: dict[str, JsonValue] = {
                    "id": "agent-a",
                    "kind": "agent",
                    "workerDefaults": {
                        "model": model, "effort": None, "fastMode": False,
                    },
                }
                projected = project("agent", record)
                agent = AgentEntityDto.model_validate(projected)
                snapshot = SnapshotAgentDto.model_validate(record)
                for parsed in (agent, snapshot):
                    if parsed.workerDefaults is None:
                        self.fail("worker defaults were omitted")
                    self.assertEqual(parsed.workerDefaults.model, model)
                    self.assertIn("model", parsed.workerDefaults.model_dump(exclude_unset=True))
        with self.assertRaises(ValidationError):
            project("agent", {
                "id": "agent-a",
                "workerDefaults": {"model": 42, "effort": None, "fastMode": False},
            })

    def test_projection_fields_come_from_models(self) -> None:
        self.assertIn("accountTransfer", AgentEntityDto.model_fields)
        for field in (
            "imageWorkspace",
            "imageWorkspaceReady",
            "imageWorkspacePhase",
            "imageWorkspaceError",
            "imageWorkspaceRepo",
            "imageWorkspaceBaseRepo",
        ):
            self.assertIn(field, AgentEntityDto.model_fields)
        self.assertEqual(EntityCollection.PEER_TEAM.value, "peerTeam")
        self.assertIn("id", COLLECTION_FIELDS["task"])
        source: dict[str, JsonValue] = {
            "id": "a",
            "status": "running",
            "private": "excluded",
        }
        agent = project("agent", source)
        self.assertEqual(agent, {"id": "a", "status": "running"})
        image_agent = project("agent", {
            "id": "image-worker",
            "imageWorkspace": True,
            "imageWorkspaceReady": True,
            "imageWorkspacePhase": "ready",
            "imageWorkspaceError": None,
            "imageWorkspaceRepo": "/repo",
            "imageWorkspaceBaseRepo": "/repo",
        })
        self.assertEqual(image_agent["imageWorkspace"], True)
        self.assertEqual(image_agent["imageWorkspacePhase"], "ready")

    def test_agent_transfer_summary_matches_runtime_projection(self) -> None:
        runtime = TransferRuntimeFixture()
        transfers = object.__new__(AccountTransfers)
        transfers.rt = runtime
        operation: dict[str, JsonValue] = {
            "id": "transfer-a", "leadId": "lead-a", "targetAccountKey": "account-b",
            "status": "pending", "scope": "subagents", "updated": 10.0,
            "finishHistory": True,
            "members": {
                "agent-a": {"phase": "completed", "lazy": True, "interruptReason": "Interrupted",
                            "name": "Worker"},
                "agent-b": {"phase": "left", "provider": "codex", "name": "Other",
                            "reason": "Unavailable"},
                "agent-c": {"phase": "blocked", "error": "Receipt unknown"},
            },
        }
        save_transfer = cast(Callable[[object, dict[str, JsonValue]], None], transfers.save)
        save_transfer(None, operation)
        projected = project("agent", runtime.lead)
        agent = AgentEntityDto.model_validate(projected)
        self.assertIsNotNone(agent.accountTransfer)
        assert agent.accountTransfer is not None
        assert agent.accountTransfer.leftOnSource is not None
        saved_transfer = runtime.lead.get("accountTransfer")
        assert isinstance(saved_transfer, dict)
        invalid_transfer = saved_transfer.copy()
        invalid_transfer["status"] = "in-progress"
        self.assertEqual(agent.accountTransfer.scope, "subagents")
        self.assertEqual(agent.accountTransfer.moved, 1)
        self.assertEqual(agent.accountTransfer.leftOnSource[0].provider, "codex")
        with self.assertRaises(ValidationError):
            AgentEntityDto.model_validate({
                "id": "lead-a", "accountTransfer": invalid_transfer,
            })

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

    def test_reconcile_start_projects_retired_event_ids(self) -> None:
        from codex_wakeups import reconcile_start as raw_reconcile_start

        repository = Path(__file__).resolve().parents[3]
        test_directory = repository / "tests"
        test_path = test_directory / "runtime-contract.py"
        sys.path.insert(0, str(test_directory))
        try:
            spec = importlib.util.spec_from_file_location("sync_reconcile_runtime_fixture", test_path)
            if spec is None or spec.loader is None:
                self.fail("runtime fixture could not be loaded")
            fixture = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(fixture)
        finally:
            sys.path.remove(str(test_directory))

        runtime_factory = cast(
            Callable[[Path, Callable[..., object]], ReconcileRuntimeFixture],
            getattr(fixture, "Runtime"),
        )
        reconcile_start = cast(
            Callable[[ReconcileRuntimeFixture, sqlite3.Connection, dict[str, JsonValue],
                     list[dict[str, JsonValue]]], bool],
            raw_reconcile_start,
        )
        fake_server = cast(Callable[..., object], getattr(fixture, "FakeServer"))
        with tempfile.TemporaryDirectory(prefix="sync-reconcile-projection-") as temporary:
            runtime = runtime_factory(Path(temporary), fake_server)
            try:
                created = runtime.create(
                    {"name": "Draft Lead", "prompt": "", "cwd": temporary}, draft=True
                )
                agent_id = created.get("id")
                if not isinstance(agent_id, str):
                    self.fail("runtime fixture did not create an agent")
                complaint: dict[str, JsonValue] = {
                    "id": "resolved-complaint", "leadId": agent_id, "recipient": "lead",
                    "status": "resolved", "responses": [],
                }
                event_id = "retired-complaint-event"
                event_text = json.dumps({"complaints": [{"complaint_id": complaint["id"]}]})
                with runtime.db() as db:
                    runtime.put(db, "complaints", complaint)
                    agent = runtime.agent(agent_id, db)
                    runtime.enqueue(db, agent, "complaint", event_text, event_id)
                    db.execute("UPDATE runtime_events SET status='reserved' WHERE id=?", (event_id,))
                    agent["startAttempt"] = {
                        "id": "reconciled-attempt", "epoch": agent["epoch"],
                        "events": [event_id], "submitted": False,
                        "activeAtReservation": False,
                    }
                    runtime.put(db, "agents", agent)
                    rows = cast(list[dict[str, JsonValue]], [dict(db.execute(
                        "SELECT * FROM runtime_events WHERE id=?", (event_id,)
                    ).fetchone())])
                    self.assertTrue(reconcile_start(runtime, db, agent, rows))

                runtime_view = {"kind": "agent", **runtime.agent(agent_id)}
                projected = project("agent", runtime_view)
                if not isinstance(projected, dict):
                    self.fail("reconciled runtime agent did not project")
                snapshot_agent = SnapshotAgentDto.model_validate(projected)
                self.assertIsNotNone(snapshot_agent.startAttempt)
                assert snapshot_agent.startAttempt is not None
                self.assertEqual(snapshot_agent.startAttempt.retiredEvents, [event_id])
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

    def test_runtime_rate_limit_and_provider_error_fields_validate(self) -> None:
        account_limits = AccountRateLimitsDto.model_validate({
            "accountKey": "default", "at": 1791091211.7, "readAt": 1791091211.8,
            "data": None, "error": None,
        })
        self.assertEqual(account_limits.readAt, 1791091211.8)
        agent = SnapshotAgentDto.model_validate({
            "id": "agent-a", "kind": "agent",
            "lastCompletedTurnError": {
                "message": "Usage limit reached: workspace_owner_credits_depleted (rateLimitExceeded).",
                "codexErrorInfo": "rateLimitExceeded",
            },
        })
        assert isinstance(agent.lastCompletedTurnError, NativeProviderError)
        self.assertEqual(agent.lastCompletedTurnError.codexErrorInfo, "rateLimitExceeded")

    def test_projection_preserves_typed_runtime_and_request_receipts(self) -> None:
        provider_error: dict[str, JsonValue] = {
            "code": -32000, "message": "Native thread is blocked",
            "codexErrorInfo": "misalignmentPolicyViolation",
            "providerDetail": {"phase": "policy"},
        }
        source: dict[str, JsonValue] = {
            "id": "agent-a", "kind": "agent", "status": "interrupted", "threadId": "thread-a",
            "turnId": "turn-a", "epoch": 4, "accountKey": "default",
            "startAttempt": {
                "id": "start-a", "epoch": 4, "events": ["event-a"], "created": 9.0,
                "settingsFixed": True, "accountKey": "default", "connectionId": "conn-a",
                "threadId": "thread-a", "modelSettings": {
                    "id": "start-a", "epoch": 4, "accountKey": "default",
                    "threadId": "thread-a", "connectionId": "conn-a",
                    "settings": {"model": "gpt-6-mini", "effort": "medium", "fastMode": False},
                    "status": "acknowledged",
                },
            },
            "nativeToolCatalog": {"threadId": "thread-a", "digest": "sha256-digest"},
            "nativeNameFailure": {
                "identity": {"accountKey": "default", "threadId": "thread-a", "name": "Worker"},
                "error": "Native thread name update failed", "attempts": 2, "retryAt": 15.0,
            },
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
                "settings": {
                    "model": "gpt-6-mini", "effort": "high", "nativeEffort": "medium",
                    "fastMode": True, "yoloMode": False,
                    "profileInstructions": "Use the project profile.", "role": "worker",
                    "daybreakEnabled": True, "cyberAccessProgram": "approved",
                }, "taskClaims": ["work-a"],
            },
            "usageResume": {
                "id": "usage-a", "status": "scheduled", "accountKey": "default",
                "threadId": "thread-a", "epoch": 4, "turnId": "turn-a",
                "cause": "usage_limit", "failedAt": 10.0, "dueAt": 30.0,
                "taskClaims": ["work-a"],
            },
            "lastEvent": "2026-10-04T03:00:00Z",
            "project": "Studio",
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
        assert parsed.capacityRetry.settings is not None
        self.assertEqual(parsed.capacityRetry.taskClaims, ["work-a"])
        self.assertEqual(parsed.capacityRetry.settings.role, "worker")
        self.assertFalse(parsed.capacityRetry.settings.yoloMode)
        self.assertEqual(parsed.usageResume.cause, "usage_limit")
        self.assertEqual(parsed.lastEvent, "2026-10-04T03:00:00Z")
        self.assertIsNotNone(parsed.nativeNameFailure)
        assert parsed.nativeNameFailure is not None
        self.assertEqual(parsed.nativeNameFailure.attempts, 2)
        assert parsed.startAttempt is not None
        self.assertTrue(parsed.startAttempt.settingsFixed)

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
            "peerTeamsVersion": 1, "peerTeams": [], "tasks": [{
                "id": "agent-a:item-a", "agent": "agent-a", "kind": "command",
                "type": "commandExecution", "itemId": "item-a", "status": "completed",
                "created": 13.0, "startedAtMs": 13000, "completedAtMs": 14000,
            }], "tasksHistoryLimit": 100,
            "monitors": [], "requests": [request_source], "rooms": [], "complaints": [],
            "rules": [], "rateLimits": {
                "accountKey": "default", "at": 14.0, "error": None,
                "data": {"accountId": "acct", "rateLimits": {
                    "limitId": "codex", "planType": "plus",
                    "primary": {"usedPercent": 95, "resetsAt": 30, "windowDurationMins": 300},
                }},
            }, "nativeNotices": [{
                "id": "provider-version:default", "accountKey": "default", "provider": "codex",
                "version": "0.153.0", "baseline": "0.153.4", "message": "Update available", "at": 14.0,
            }],
            "rateLimitsByAccount": {"default": {
                "accountKey": "default", "at": None, "data": None, "error": None,
            }},
            "events": [], "connected": True,
        })
        self.assertEqual(runtime_snapshot.requests[0].epoch, 4)
        self.assertEqual(runtime_snapshot.tasks[0].itemId, "item-a")
        self.assertIsNotNone(runtime_snapshot.rateLimits.data)
        assert runtime_snapshot.rateLimits.data is not None
        self.assertIsNotNone(runtime_snapshot.rateLimits.data.rateLimits)
        assert runtime_snapshot.rateLimits.data.rateLimits is not None
        self.assertEqual(runtime_snapshot.rateLimits.data.rateLimits.planType, "plus")
        self.assertEqual(runtime_snapshot.nativeNotices[0].provider, "codex")
        state = StateSnapshot.model_validate({
            "token": "session", "stateDir": "/state", "threads": [snapshot_source],
            "chats": [{
                "id": "chat-a", "name": "Chat", "members": ["agent-a"], "kind": "chat",
                "messageCount": 1, "tail": "Hello", "lastMessageAt": 14.0,
            }], "nodes": [snapshot_source, {
                "id": "chat-a", "name": "Chat", "members": ["agent-a"], "kind": "chat",
                "messageCount": 1, "tail": "Hello", "lastMessageAt": 14.0,
            }], "edges": [], "at": 15.0,
            "runtime": runtime_snapshot.model_dump(mode="json"),
        })
        self.assertIsNotNone(state.runtime)
        self.assertEqual(state.threads[0].project, "Studio")
        self.assertIsInstance(state.nodes[0], SnapshotAgentDto)
        self.assertIsInstance(state.nodes[1], SnapshotChatGroupDto)

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

    def test_worktree_preparation_survives_projection_and_snapshot(self) -> None:
        for phase in ("waiting", "preparing"):
            with self.subTest(phase=phase):
                record: dict[str, JsonValue] = {"id": "worker", "kind": "agent", "status": "starting", "worktreePreparation": phase}
                projected = project("agent", record)
                self.assertEqual(projected, record)
                self.assertEqual(SnapshotAgentDto.model_validate(record).worktreePreparation, phase)
        self.assertEqual(project("agent", {"id": "worker"}), {"id": "worker"})

    def test_snapshot_preserves_unknown_start_identity_and_hold(self) -> None:
        record: dict[str, JsonValue] = {
            "id": "worker", "kind": "agent", "status": "interrupted",
            "startAttempt": {"id": "attempt", "supervisorIdentity": {
                "stateDir": "/fixture", "handle": "native", "generation": 2,
            }},
            "startOutcomeHold": {
                "stage": "held", "at": 1.5, "attemptId": "attempt",
                "threadId": "thread", "connectionId": "connection",
                "evidence": "complete_history_absent_idle_twice_journal_drained",
            },
        }
        self.assertEqual(
            SnapshotAgentDto.model_validate(record).model_dump(mode="json", exclude_unset=True), record,
        )
        projected = project("agent", record)
        assert isinstance(projected, dict)
        self.assertNotIn("startOutcomeHold", projected)
        self.assertEqual(projected["startAttempt"], {})

    def test_snapshot_preserves_recovery_outcomes_without_sync_expansion(self) -> None:
        recoveries: list[dict[str, JsonValue]] = [
            {"at": 1.5, "turnId": None, "outcome": "input_absent",
             "source": "replaced_native_child", "attemptId": "attempt"},
            {"at": 1.5, "turnId": None, "latestTurnId": "last", "outcome": "idle",
             "source": "native_thread_read"},
        ]
        for outcome in ("completed", "failed", "interrupted"):
            recoveries.append({"at": 1.5, "turnId": "turn", "outcome": outcome,
                               "source": "native_thread_read"})
        for recovery in recoveries:
            record: dict[str, JsonValue] = {
                "id": "worker", "kind": "agent", "turnRecovery": recovery,
                "checkpointError": "fixture checkout failure",
            }
            self.assertEqual(
                SnapshotAgentDto.model_validate(record).model_dump(mode="json", exclude_unset=True), record,
            )
            projected = project("agent", record)
            assert isinstance(projected, dict)
            self.assertNotIn("turnRecovery", projected)
            self.assertNotIn("checkpointError", projected)

    def test_snapshot_accepts_budget_accounting_modes(self) -> None:
        for mode in ("provisional", "responseRecords"):
            record: dict[str, JsonValue] = {"id": "worker", "kind": "agent", "tokenUsageAccounting": mode}
            self.assertEqual(SnapshotAgentDto.model_validate(record).tokenUsageAccounting, mode)
            projected = project("agent", record)
            assert isinstance(projected, dict)
            self.assertNotIn("tokenUsageAccounting", projected)

    def test_recovery_receipts_are_snapshot_only_json_objects(self) -> None:
        for field in ("connectionRecovery", "lastContextRepairCheck", "lastContextRepairWait"):
            record: dict[str, JsonValue] = {
                "id": "worker", "kind": "agent", field: {"at": 1.5, "events": ["input"]},
            }
            self.assertEqual(
                SnapshotAgentDto.model_validate(record).model_dump(mode="json", exclude_unset=True), record,
            )
            projected = project("agent", record)
            assert isinstance(projected, dict)
            self.assertNotIn(field, projected)
            for invalid in (1.5, {"at": float("nan")}):
                with self.subTest(field=field, invalid=invalid), self.assertRaises(ValidationError):
                    SnapshotAgentDto.model_validate({"id": "worker", "kind": "agent", field: invalid})

    def test_draft_row_standalone_validation_stays_scoped_to_payloads(self) -> None:
        row = {
            "newDocumentState": {
                "id": "device:tab:lead",
                "payload": json.dumps({
                    "id": "device:tab:lead", "device": "device", "session": "lead", "text": "draft"
                }),
            },
            "assumedMasterState": {"id": "device:tab:lead", "payload": "null"},
        }
        self.assertEqual(DraftPushRow.model_validate(row).newDocumentState.id, "device:tab:lead")
        with self.assertRaises(ValidationError):
            DraftPushRow.model_validate({
                **row, "assumedMasterState": {"id": "device:tab:lead", "payload": "{"},
            })

        mismatched = {
            **row,
            "newDocumentState": {
                **row["newDocumentState"], "id": "device:other",
            },
        }
        self.assertEqual(DraftPushRow.model_validate(mismatched).newDocumentState.id, "device:other")
        with self.assertRaises(ValidationError):
            DraftPushRequest.model_validate({"rows": [mismatched]})

    def test_draft_validation_error_locations_match_public_contract(self) -> None:
        valid_payload = json.dumps({
            "id": "device:lead", "device": "device", "session": "lead", "text": "draft",
        })
        invalid_cases = (
            ({"rows": [{"newDocumentState": {"id": "device:lead", "payload": "{"}}]},
             ("rows", 0, "newDocumentState")),
            ({"rows": [{
                "newDocumentState": {"id": "device:lead", "payload": valid_payload},
                "assumedMasterState": {"id": "device:lead", "payload": "{"},
            }]}, ("rows", 0, "assumedMasterState")),
            ({"rows": [{"newDocumentState": {"id": "wrong", "payload": valid_payload}}]}, ()),
        )
        for body, expected_location in invalid_cases:
            with self.subTest(expected_location=expected_location):
                with self.assertRaises(ValidationError) as raised:
                    DraftPushRequest.model_validate(body)
                self.assertEqual(
                    [error["loc"] for error in raised.exception.errors(include_url=False)],
                    [expected_location],
                )

    def test_chat_and_full_snapshot_models_share_runtime_contract(self) -> None:
        active_agent = {
            "id": "agent-a", "kind": "agent", "status": "running", "activity": {
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
                "action": "compact", "submitted": True, "activeAtReservation": True,
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
            "rateLimits": {
                "accountKey": "default", "at": None, "data": None, "error": None,
            }, "nativeNotices": [], "rateLimitsByAccount": {}, "events": [],
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

if __name__ == "__main__":
    unittest.main()
