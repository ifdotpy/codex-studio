"""Focused route and response-contract tests for system endpoints."""
from __future__ import annotations

import os
import json
from pathlib import Path
import queue
import sqlite3
import tempfile
import threading
import unittest
from contextlib import nullcontext
from types import SimpleNamespace
from typing import TYPE_CHECKING, Callable, cast
from unittest.mock import patch

from fastapi import FastAPI
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient
from pydantic import ValidationError

from studio_api.middleware import RequestBoundary
from studio_api.models import ContractModel, JsonValue
from .models import (
    DesktopResponse,
    DiagnosticsResponse,
    DirectoriesResponse,
    ExecutionAttempt,
    ExecutionAction,
    ExecutionRecovery,
    ExecutionEffect,
    MigrationStatus,
    NativeProviderAccount,
    ProcessRecord,
    ProviderVersions,
    SqliteTransaction,
    SupervisorHealth,
)
from .router import create_router

if TYPE_CHECKING:
    from codex_canvas import Canvas
    from studio_api.context import ApiContext, RemoteAccessContract


class FakeContext:
    def __init__(self, root: Path) -> None:
        self.canvas = FakeCanvas(root)
        self.remote = SimpleNamespace(origin=lambda: "https://studio.example")

    @property
    def runtime(self) -> object | None:
        return self.canvas.runtime

    def send(self, _request: object, value: object, **_kwargs: object) -> JSONResponse:
        if not isinstance(value, dict):
            raise TypeError("System API responses must be mappings")
        model: type[ContractModel]
        if "application" in value:
            model = DesktopResponse
        elif "directories" in value:
            model = DirectoriesResponse
        else:
            model = DiagnosticsResponse
        validated = model.model_validate_json(json.dumps(value))
        return JSONResponse(json.loads(validated.model_dump_json(exclude_unset=True)))


class FakeCanvas:
    def __init__(self, root: Path) -> None:
        self.root = root
        self.runtime: object | None = None


class FakeRemote:
    def origin(self) -> str | None:
        return None

    def request_origin(self, _headers: object, _peer: str, _port: int) -> str | None:
        return "http://testserver"


def supervisor_health() -> dict[str, JsonValue]:
    return {
        "protocol": 1, "stateDir": "/fixture", "journalLimitBytes": 1024,
        "outputLimitBytesPerHandle": 1024, "durability": "sqlite-full-sync-per-request",
        "recovery": {"degraded": False, "fallbackReady": False, "blocked": None},
        "handles": [{
            "id": "native:fixture", "pid": 10, "signature": "fixture-signature",
            "startTime": "fixture-start", "sequence": 2, "acknowledged": 1,
            "bufferedBytes": 64, "backpressure": False, "stdoutReaderAlive": True,
            "stdoutReaderError": None, "persistenceErrors": {"stdout": "OSError: fixture"},
        }],
    }


class SystemApiTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="studio-system-api-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.context = FakeContext(self.root)
        app = FastAPI()
        app.include_router(create_router(cast("ApiContext", self.context)))
        self.app = app
        self.client = TestClient(app)

    def test_directories_preserves_wire_shape_hides_hidden_entries_and_sorts(self) -> None:
        (self.root / "zulu").mkdir()
        (self.root / "Alpha").mkdir()
        (self.root / ".private").mkdir()
        (self.root / "plain-file").write_text("fixture", encoding="utf-8")

        response = self.client.get(f"/api/directories?path=&path={self.root}&path=/does/not/exist")

        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json(), {
            "path": str(self.root),
            "parent": str(self.root.parent),
            "directories": [
                {"name": "Alpha", "path": str(self.root / "Alpha")},
                {"name": "zulu", "path": str(self.root / "zulu")},
            ],
        })

    def test_directories_rejects_unavailable_path(self) -> None:
        response = self.client.get("/api/directories", params={"path": str(self.root / "missing")})

        self.assertEqual(response.status_code, 400)

    def test_desktop_preserves_status_and_query_account(self) -> None:
        runtime = SimpleNamespace(
            live_updates=SimpleNamespace(status=lambda: {"status": "idle", "pid": 3}),
        )
        native_status: dict[str, object] = {
            "status": "ready",
            "checkedAt": 10.0,
            "selected": {
                "path": "/bundle/codex",
                "sourcePath": "/installed/codex",
                "version": "1.2.3",
                "sha256": "a" * 64,
                "bundleSha256": "b" * 64,
                "validatedAt": 9.0,
                "approvalRevision": 1,
                "checks": {"schema": {"methods": []}},
                "companions": {"codex-code-mode-host": {"sha256": "c" * 64}},
                "sourceIdentity": {"path": "/installed/codex", "size": 100},
            },
            "candidates": [
                {"path": "/installed/codex", "status": "discovered", "version": "1.2.3",
                 "identity": {"path": "/installed/codex"}, "companionIdentity": {}},
                {"path": "/older/codex", "status": "approved", "version": "1.2.2",
                 "identity": {"path": "/older/codex"}, "companionIdentity": {}},
                {"path": "/broken/codex", "status": "rejected", "error": "invalid"},
            ],
            "accounts": {"fixture-account": {
                "status": "current", "version": "1.2.3", "pid": 9,
                "targetVersion": "1.2.3", "reason": None, "updatedAt": 8.0,
            }},
        }
        with patch("codex_native_runtime.status", return_value=None) as native, \
                patch("codex_provider_versions.status", return_value={
                    "checkedAt": 11.0, "providers": [], "warnings": [],
                }), \
                patch("codex_browser.diagnostics", return_value=None) as browser, \
                patch("codex_process_supervisor.status", return_value=supervisor_health()), \
                patch.dict(os.environ, {"CODEX_AGENTS_SUPERVISOR_MODE": "1"}, clear=False):
            self.context.canvas.runtime = runtime
            native.return_value = native_status
            self.context.remote.origin = lambda: None
            response = self.client.get("/api/desktop?account_key=&account_key=fixture-account")

        self.assertEqual(response.status_code, 200)
        body = response.json()
        self.assertEqual(body["application"], "codex-agents")
        self.assertIsNone(body["publicOrigin"])
        self.assertEqual(body["nativeRuntime"]["selected"]["version"], "1.2.3")
        self.assertEqual(body["liveUpdate"], {"status": "idle", "pid": 3})
        self.assertEqual(body["supervisor"], supervisor_health())
        native.assert_called_once_with(runtime)
        browser.assert_called_once_with(runtime, "fixture-account")

    def test_actual_context_sender_validates_system_route_output(self) -> None:
        from studio_api.context import ApiContext

        context = ApiContext(
            cast("Canvas", FakeCanvas(self.root)),
            token="fixture-token",
            remote=cast("RemoteAccessContract", FakeRemote()),
            schema_only=True,
        )
        app = FastAPI()
        app.add_middleware(RequestBoundary, context=context)
        app.include_router(create_router(context))
        client = TestClient(app)

        startup = client.get("/api/desktop")
        self.assertEqual(startup.status_code, 200)
        self.assertIsNone(startup.json()["nativeRuntime"])
        self.assertIsNone(startup.json()["browser"])
        self.assertEqual(startup.json()["providerVersions"], {
            "checkedAt": None, "providers": [], "warnings": [],
        })

        runtime = SimpleNamespace(
            live_updates=None,
            lock=threading.RLock(),
            servers={},
            accounts={},
            connection_ids={},
            offline_accounts=set(),
            changed=threading.Event(),
        )
        context.canvas.runtime = runtime
        valid_native_status: dict[str, JsonValue] = {
            "status": "checking", "selected": None, "candidates": [], "accounts": {},
        }
        with patch("codex_native_runtime.status", return_value=valid_native_status), \
                patch("codex_browser.diagnostics", return_value=None), \
                patch("codex_process_supervisor.status", return_value=supervisor_health()), \
                patch.dict(os.environ, {"CODEX_AGENTS_SUPERVISOR_MODE": "1"}, clear=False), \
                patch("studio_api.context.logging.Logger.error") as error_log:
            accepted = client.get("/api/desktop")

        self.assertEqual(accepted.status_code, 200)
        self.assertEqual(accepted.json()["nativeRuntime"], valid_native_status)
        self.assertEqual(accepted.json()["supervisor"], supervisor_health())
        error_log.assert_not_called()
        self.assertEqual(accepted.json()["providerVersions"]["providers"], [])
        self.assertEqual(accepted.json()["providerVersions"]["warnings"], [])
        self.assertNotIn("outcome", accepted.json())

        invalid_native_status: dict[str, JsonValue] = {
            "status": "unknown", "selected": None, "candidates": [], "accounts": {},
        }
        with patch("codex_native_runtime.status", return_value=invalid_native_status), \
                patch("codex_browser.diagnostics", return_value=None):
            with self.assertLogs("studio_api.context", level="ERROR"):
                mismatched = client.get("/api/desktop")

        # A read with a contract mismatch fails open, so desktop attach and
        # recovery keep working while the mismatch is logged.
        self.assertEqual(mismatched.status_code, 200)
        self.assertEqual(mismatched.json()["nativeRuntime"], invalid_native_status)
        provider_monitor = runtime.provider_version_monitor
        provider_monitor.worker.join(timeout=2)
        self.assertFalse(provider_monitor.worker.is_alive())

    def test_provider_versions_contract_matches_real_producer(self) -> None:
        from codex_provider_versions import ProviderVersionMonitor as UntypedProviderVersionMonitor
        from codex_provider_versions import status as untyped_status

        monitor_factory = cast(Callable[[], object], UntypedProviderVersionMonitor)
        monitor = monitor_factory()
        provider_rows: list[dict[str, JsonValue]] = [
            {
                "id": "provider-version:checking", "accountKey": "checking",
                "provider": "codex", "status": "checking", "runningVersion": None,
                "installedVersion": None, "configuredVersion": None, "baseline": "0.153.4",
                "error": None, "message": "Checking provider CLI version.", "at": None,
            },
            {
                "id": "provider-version:current", "accountKey": "current",
                "provider": "claude", "status": "current", "runningVersion": "2.1.278",
                "installedVersion": "2.1.278", "configuredVersion": None, "baseline": "2.1.278",
                "error": None, "message": None, "at": 100.0,
            },
            {
                "id": "provider-version:outdated", "accountKey": "outdated",
                "provider": "codex", "status": "outdated", "runningVersion": "0.153.3",
                "installedVersion": "0.153.3", "configuredVersion": None, "baseline": "0.153.4",
                "error": None, "message": "Below the tested baseline.", "at": 101.0,
            },
            {
                "id": "provider-version:unknown", "accountKey": "unknown",
                "provider": "other-provider", "status": "unknown", "runningVersion": None,
                "installedVersion": None, "configuredVersion": None, "baseline": None,
                "error": None, "message": "No evidence-backed version baseline is configured.",
                "at": 102.0,
            },
            {
                "id": "provider-version:error", "accountKey": "error",
                "provider": "claude", "status": "error", "runningVersion": None,
                "installedVersion": None, "configuredVersion": None, "baseline": "2.1.278",
                "error": "Could not read the Claude Code executable version.",
                "message": "Version check failed.", "at": 103.0,
            },
        ]
        setattr(monitor, "checked_at", 104.0)
        setattr(monitor, "providers", provider_rows)
        setattr(monitor, "tick", lambda _runtime: None)
        producer = cast(Callable[[object], dict[str, JsonValue]], untyped_status)
        with patch("codex_provider_versions.monitor", return_value=monitor):
            result = producer(object())

        response = ProviderVersions.model_validate_json(json.dumps(result))
        self.assertEqual([row.status.value for row in response.providers], [
            "checking", "current", "outdated", "unknown", "error",
        ])
        self.assertEqual(len(response.warnings), 1)
        self.assertEqual(response.warnings[0].version, "0.153.3")

    def test_diagnostics_without_runtime_remains_not_found(self) -> None:
        response = self.client.get("/api/diagnostics")
        self.assertEqual(response.status_code, 404)

    def test_diagnostics_validates_real_response_envelope(self) -> None:
        self.context.canvas.runtime = SimpleNamespace()
        fixture: dict[str, object] = {
            "at": "2026-10-04T00:00:00+00:00",
            "processTree": {"rootPid": 10, "processes": [], "kinds": {}, "totalRssMiB": 0.0},
            "hostResources": {"cpuCount": 8, "totalMemoryBytes": None, "availableMemoryBytes": None},
            "resourceAttribution": [],
            "nativeAccounts": {"account1": {
                "provider": "claude", "liveQueries": 1, "activeTurns": 0,
                "backgroundQueries": 0, "idleQueries": 1, "idleLimitSeconds": 900,
                "sessionCache": {"cachedSessions": 1, "retainedBytes": 2,
                                 "pendingWrites": 0, "journalBytes": 4},
            }},
            "studioLoadedThreads": 0,
            "queues": {"recoveryPending": 0, "durableInputPending": 0},
            "runtimeLockSamples": [{"waitMs": 0.1, "holder": None}],
            "executions": {"runs": [], "limit": 25, "relatedLimit": 100},
            "sqliteContention": {
                "activeTransactions": [],
                "activeTransactionScan": {"threads": 0, "frames": 0, "locals": 0, "truncated": False},
                "slowTransactions": {
                    "coverageStartedAt": 1.0,
                    "thresholdMs": 1000.0,
                    "recent": [],
                    "longest": [],
                    "journal": {"status": "notWritten"},
                    "activeTracking": {"tracked": 0, "limit": 256, "untrackedStarts": 0},
                },
            },
            "analyticsCapture": {
                "limit": 64, "byteLimit": 8388608, "queued": 0, "active": 0,
                "bytes": 0, "completed": 0, "failed": 0, "overflow": 0,
                "pendingErrors": 0, "lastError": None,
            },
            "migrations": {"payloads": {}, "analyticsFile": {"status": "idle"}},
            "analyticsFileMigration": {"status": "idle"},
            "searchMigrationError": None,
        }
        with patch("codex_diagnostics.snapshot", return_value=fixture):
            response = self.client.get("/api/diagnostics")

        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["nativeAccounts"]["account1"]["liveQueries"], 1)
        self.assertEqual(response.json()["supervisor"]["mode"], False)

    def test_diagnostics_models_accept_actual_snapshot_producer_shape(self) -> None:
        from codex_claude_input_recovery import capture_input as untyped_capture_input
        from codex_diagnostics import snapshot as untyped_snapshot
        from codex_execution import ensure_tables as untyped_ensure_tables
        from codex_execution import observe_native as untyped_observe_native

        snapshot = cast(
            Callable[[object, int, str], dict[str, JsonValue]],
            untyped_snapshot,
        )
        ensure_tables = cast(Callable[[sqlite3.Connection], None], untyped_ensure_tables)

        db = sqlite3.connect(":memory:")
        db.row_factory = sqlite3.Row
        self.addCleanup(db.close)
        ensure_tables(db)
        db.execute("CREATE TABLE runtime_events(id TEXT,agent TEXT,epoch INTEGER,kind TEXT,text TEXT,created REAL,status TEXT)")
        db.execute("CREATE TABLE runtime_event_meta(id TEXT,record TEXT)")
        db.execute("CREATE TABLE runtime_items(id TEXT,agent TEXT,created REAL,record TEXT)")
        db.execute("INSERT INTO runtime_events VALUES (?,?,?,?,?,?,?)",
                   ("input:fixture", "agent:fixture", 1, "user", "fixture input", 1.0, "pending"))
        db.execute("INSERT INTO runtime_event_meta VALUES (?,?)",
                   ("input:fixture", json.dumps({"assets": ["asset:fixture"]})))
        run: dict[str, object] = {
            "id": "run:fixture", "agent": "agent:fixture", "accountKey": "default",
            "epoch": 1, "threadId": "thread:fixture", "turnId": "turn:fixture", "created": 1.0,
            "status": "pending", "firstAttemptId": "attempt:fixture",
            "rootAttemptId": None, "latestAttemptId": "attempt:fixture",
            "resultItemId": "item:fixture",
            "connectionRecovery": {
                "source": "transport_attach", "at": 2.0, "attemptId": "attempt:fixture",
                "eventIds": ["input:fixture"], "previousError": {"message": "fixture disconnect"},
                "outcome": "input_restored", "automatic": True, "completionDelivered": False,
                "holdOperations": [{"id": "task:fixture", "kind": "runtime_tasks", "label": "fixture task"}],
            },
            "restartRecovery": {"stage": "input_restored", "observedAt": 2.0,
                                "heldAt": 3.0, "supersededAt": 4.0, "eventId": "event:fixture"},
        }
        db.execute(
            "INSERT INTO runtime_execution_runs VALUES (?,?,?,?,?,?,?,?)",
            (run["id"], run["agent"], run["accountKey"], run["epoch"], run["threadId"],
             run["turnId"], run["created"], json.dumps(run)),
        )
        attempt: dict[str, object] = {
            "id": "attempt:fixture", "runId": "run:fixture", "submission": "unsent",
            "epoch": 1, "events": [], "submitted": False,
            "supervisorIdentity": {"stateDir": "/fixture", "handle": "native", "generation": 2},
            "settingsFixed": True,
            "action": "capacity", "capacityRetryId": "capacity:fixture",
            "reviewTarget": {"type": "custom", "instructions": "Review the fixture."},
            "modelSettings": {
                "id": "attempt:fixture", "epoch": 1, "accountKey": "default",
                "threadId": "thread:fixture", "connectionId": "connection:fixture",
                "settings": {"model": "fixture-model", "effort": "high", "nativeEffort": "high",
                             "fastMode": False, "yoloMode": False, "role": "reviewer",
                             "profileInstructions": "", "daybreakEnabled": False,
                             "cyberAccessProgram": "standard"},
                "status": "acknowledged",
            },
        }
        capture_input = cast(Callable[..., dict[str, object]], untyped_capture_input)
        attempt["claudeInputRequest"] = capture_input(
            db,
            {"id": "agent:fixture", "cwd": "/fixture", "rootId": "agent:fixture",
             "role": "lead", "isLead": True, "model": "claude-fixture", "compactions": 0},
            [{"id": "input:fixture", "agent": "agent:fixture", "epoch": 1}],
            {"input": [{"type": "text", "text": "fixture input"}],
             "clientUserMessageId": "input:fixture", "threadId": "thread:fixture",
             "sandboxPolicy": {"type": "workspaceWrite"}},
            "fixture input",
        )
        run["disconnectRecovery"] = {"startAttempt": attempt}
        db.execute("UPDATE runtime_execution_runs SET record=? WHERE id=?",
                   (json.dumps(run), run["id"]))
        db.execute("INSERT INTO runtime_execution_attempts VALUES (?,?,?)",
                   (attempt["id"], run["id"], json.dumps(attempt)))
        node: dict[str, object] = {
            "id": "worker:fixture", "runId": "run:fixture", "kind": "managed_worker",
            "agentId": "agent:child", "status": "waiting",
        }
        db.execute("INSERT INTO runtime_execution_nodes VALUES (?,?,?)",
                   (node["id"], run["id"], json.dumps(node)))
        effect: dict[str, object] = {
            "id": "spawn:agent:child", "runId": "run:fixture", "kind": "spawn",
            "referenceId": "agent:child", "status": "queued",
        }
        db.execute("INSERT INTO runtime_execution_effects VALUES (?,?,?,?,?)",
                   (effect["id"], run["id"], effect["kind"], effect["referenceId"], json.dumps(effect)))
        item = {"id": "item:fixture", "turnId": "turn:fixture", "role": "assistant",
                "text": "fixture final"}
        db.execute("INSERT INTO runtime_items VALUES (?,?,?,?)",
                   (item["id"], "agent:fixture", 2.0, json.dumps(item)))
        observe_native = cast(Callable[..., None], untyped_observe_native)
        native_error = {"message": "fixture native error", "codexErrorInfo": {"type": "other"}}
        actor = {"id": "agent:fixture", "accountKey": "default", "epoch": 1,
                 "threadId": "thread:fixture", "turnId": "turn:fixture"}
        observe_native(db, actor, "turn/completed", {
            "parentTurnId": "turn:fixture",
            "turn": {"id": "child-turn:fixture", "status": "failed", "error": native_error},
        })
        observe_native(db, actor, "turn/completed", {
            "turn": {"id": "turn:fixture", "status": "failed", "error": native_error},
        })
        runtime = SimpleNamespace(
            lock=threading.RLock(),
            servers={},
            accounts={},
            loaded=set(),
            recovery_pool=SimpleNamespace(_work_queue=queue.Queue()),
            db=lambda: nullcontext(db),
            sample_dispatch_lock_holder=lambda _rows, _waited: None,
            analytics_capture_status=lambda: {"available": False},
            analytics_migration_status={"status": "idle"},
        )
        with patch("codex_diagnostics.host_resources", return_value={
            "cpuCount": 4,
            "totalMemoryBytes": None,
            "availableMemoryBytes": None,
        }):
            result = snapshot(runtime, 10, "10 1 1024 0.0 codex-canvas")
        result["supervisor"] = {
            "mode": True, "fallback": False, "notice": None, "health": supervisor_health(),
        }

        validated = DiagnosticsResponse.model_validate(result)
        identity = validated.executions.runs[0].attempts[0].supervisorIdentity
        self.assertIsNotNone(identity)
        assert identity is not None
        self.assertEqual(identity.generation, 2)
        actual_run = validated.executions.runs[0]
        assert actual_run.restartRecovery is not None
        assert actual_run.restartRecovery.stage is not None
        self.assertEqual(actual_run.restartRecovery.stage.value, "input_restored")
        self.assertEqual(actual_run.attempts[0].action, "capacity")
        self.assertEqual(actual_run.attempts[0].capacityRetryId, "capacity:fixture")
        self.assertIsNotNone(actual_run.attempts[0].reviewTarget)
        settings = actual_run.attempts[0].modelSettings
        assert settings is not None
        self.assertEqual(settings.status, "acknowledged")
        self.assertEqual(next(node for node in actual_run.nodes if node.kind.value == "managed_worker").status,
                         "waiting")
        self.assertEqual(actual_run.resultItemId, "item:fixture")
        self.assertEqual(actual_run.error, native_error)
        self.assertEqual(next(node for node in actual_run.nodes if node.kind.value == "native_child").error,
                         native_error)
        recovery = actual_run.connectionRecovery
        disconnected = actual_run.disconnectRecovery
        assert recovery is not None and disconnected is not None
        assert disconnected.startAttempt is not None
        self.assertEqual(recovery.eventIds, ["input:fixture"])
        self.assertEqual(recovery.previousError, {"message": "fixture disconnect"})
        self.assertFalse(recovery.completionDelivered)
        self.assertTrue(disconnected.startAttempt.settingsFixed)
        request = actual_run.attempts[0].claudeInputRequest
        self.assertIsNotNone(request)
        assert request is not None
        self.assertEqual(request.clientUserMessageId, "input:fixture")
        event = request.events[0]
        assert event is not None
        self.assertEqual(event.assets, ["asset:fixture"])
        with patch("codex_diagnostics.snapshot", return_value=result), \
                patch("codex_process_supervisor.status", return_value=supervisor_health()), \
                patch.dict(os.environ, {"CODEX_AGENTS_SUPERVISOR_MODE": "1"}, clear=False):
            self.context.canvas.runtime = runtime
            response = self.client.get("/api/diagnostics")
        self.assertEqual(response.status_code, 200)
        self.assertEqual(response.json()["executions"], result["executions"])

    def test_system_models_keep_new_fields_strict(self) -> None:
        health = supervisor_health()
        health["unexpected"] = True
        with self.assertRaises(ValidationError):
            SupervisorHealth.model_validate(health)
        health.pop("unexpected")
        health["outputLimitBytesPerHandle"] = "1024"
        with self.assertRaises(ValidationError):
            SupervisorHealth.model_validate(health)
        for field, value in (("settingsFixed", "true"), ("claudeRetryOf", 1),
                             ("claudeInputRequest", []), ("capacityRetryId", 1),
                             ("reviewTarget", {"type": "unknown"}), ("modelSettings", {}),
                             ("action", "unknown"), ("unknown", True)):
            with self.subTest(field=field), self.assertRaises(ValidationError):
                ExecutionAttempt.model_validate({"id": "attempt:fixture", field: value})
        with self.assertRaises(ValueError):
            ExecutionAction("capacity")
        self.assertEqual(ExecutionAttempt.model_validate({"id": "attempt:fixture", "action": "safety"}).action,
                         "safety")
        with self.assertRaises(ValueError):
            ExecutionAction("safety")
        with self.assertRaises(ValidationError):
            ExecutionRecovery.model_validate({"stage": "unknown"})
        for status in ("answered", "expired", "lost"):
            effect = ExecutionEffect.model_validate({
                "id": "effect:fixture", "runId": "run:fixture", "kind": "requests",
                "referenceId": "request:fixture", "status": status,
            })
            self.assertEqual(effect.status, status)
        with self.assertRaises(ValidationError):
            ExecutionEffect.model_validate({
                "id": "effect:fixture", "runId": "run:fixture", "kind": "requests",
                "referenceId": "request:fixture", "status": "unknown-status",
            })
        rejected = {
            "id": "attempt:fixture", "epoch": 1, "accountKey": "default",
            "threadId": "thread:fixture", "connectionId": "connection:fixture",
            "nativeOperationId": "operation:fixture", "events": ["input:fixture"],
            "agent": "agent:fixture", "turnId": None, "outcome": "not_applied", "retry": True,
        }
        attempt = ExecutionAttempt.model_validate({
            "id": "attempt:fixture", "executionOutcome": "not_applied",
            "claudeRetryOf": "previous:fixture", "claudeInputRejection": rejected,
        })
        self.assertEqual(attempt.model_dump(exclude_unset=True)["claudeInputRejection"], rejected)
        rejected["retry"] = "true"
        with self.assertRaises(ValidationError):
            ExecutionAttempt.model_validate({"id": "attempt:fixture", "claudeInputRejection": rejected})

    def test_sqlite_transaction_model_accepts_real_sql_completion(self) -> None:
        import codex_sqlite_traces
        from codex_sqlite import connect as untyped_connect

        connect = cast(Callable[..., sqlite3.Connection], untyped_connect)
        db = connect(":memory:", site="system-api-fixture")
        self.addCleanup(db.close)
        history = cast(Callable[[], dict[str, object]], codex_sqlite_traces.history)
        with patch.object(codex_sqlite_traces, "SLOW_MS", 0.0), \
                patch.object(codex_sqlite_traces, "_ACTIVE", {}), \
                patch.object(codex_sqlite_traces, "_RECENT", []), \
                patch.object(codex_sqlite_traces, "_LONGEST", []):
            db.execute("BEGIN")
            db.execute("COMMIT")
            result = cast(list[dict[str, JsonValue]], history()["recent"])[-1]
        validated = SqliteTransaction.model_validate(result)
        self.assertEqual(validated.state.value, "ended")
        result["state"] = "unknown"
        with self.assertRaises(ValidationError):
            SqliteTransaction.model_validate(result)

    def test_openapi_declares_json_success_models_for_owned_paths(self) -> None:
        schemas = self.app.openapi()["components"]["schemas"]
        paths = self.app.openapi()["paths"]
        self.assertIn("DesktopResponse", schemas)
        self.assertIn("DiagnosticsResponse", schemas)
        self.assertIn("DirectoriesResponse", schemas)
        for route in ("/api/desktop", "/api/diagnostics", "/api/directories"):
            self.assertIn("200", paths[route]["get"]["responses"])
        desktop_params = paths["/api/desktop"]["get"]["parameters"]
        directory_params = paths["/api/directories"]["get"]["parameters"]
        self.assertIn("account_key", {item["name"] for item in desktop_params})
        self.assertIn("path", {item["name"] for item in directory_params})
        self.assertIn("providerVersions", schemas["DesktopResponse"]["properties"])
        self.assertIn("ProviderVersions", schemas)

    def test_process_kind_enum_accepts_wire_string_and_rejects_unknown_kind(self) -> None:
        record = '{"pid":1,"parentPid":0,"kind":"codex_app_server","rssMiB":1.0,"cpuPercent":0.0}'
        self.assertEqual(ProcessRecord.model_validate_json(record).kind.value, "codex_app_server")
        invalid = record.replace("codex_app_server", "unexpected_process")
        with self.assertRaises(ValidationError):
            ProcessRecord.model_validate_json(invalid)

    def test_native_provider_model_rejects_malformed_codex_branch(self) -> None:
        from pydantic import TypeAdapter

        adapter: TypeAdapter[NativeProviderAccount] = TypeAdapter(NativeProviderAccount)
        self.assertEqual(
            adapter.validate_json('{"provider":"codex","loadedThreads":3}').provider,
            "codex",
        )
        with self.assertRaises(ValidationError):
            adapter.validate_json('{"provider":"codex","liveQueries":3}')

    def test_migration_models_cover_owned_status_variants(self) -> None:
        value = {
            "search": {
                "phase": "waiting_for_space", "cursor": 0, "updated": 1.0,
                "error": None, "lastBatchBytes": None,
            },
            "payloads": {
                "checkpoints": {"cursor": 1, "complete": False, "status": "running",
                                "updated": 1.0, "error": None},
                "tool_requests": {"cursor": 1, "complete": True, "status": "complete",
                                  "updated": 1.0, "error": None},
                "tool_results": {"cursor": 0, "complete": False, "status": "waitingForSpace",
                                 "updated": None, "error": None},
                "tasks": {"cursor": 0, "complete": False, "status": "pending",
                          "updated": None, "error": None},
            },
            "entityTombstones": {
                "count": 2, "floor": 1,
                "pruning": {"status": "running", "updated": 1.0, "deleted": 1, "remaining": 1},
            },
            "analyticsFile": {"status": "copy", "updated": 1.0, "error": None},
        }
        MigrationStatus.model_validate_json(json.dumps(value))


if __name__ == "__main__":
    unittest.main()
