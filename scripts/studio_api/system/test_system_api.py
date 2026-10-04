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
from typing import cast
from unittest.mock import patch

from fastapi import FastAPI
from fastapi.responses import JSONResponse
from fastapi.testclient import TestClient
from pydantic import ValidationError

from studio_api.context import ApiContext
from studio_api.models import ContractModel
from .models import DesktopResponse, DiagnosticsResponse, DirectoriesResponse, ProcessRecord
from .router import create_router


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


class SystemApiTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory(prefix="studio-system-api-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.context = FakeContext(self.root)
        app = FastAPI()
        app.include_router(create_router(cast(ApiContext, self.context)))
        self.app = app
        self.client = TestClient(app)

    def test_directories_preserves_wire_shape_hides_hidden_entries_and_sorts(self) -> None:
        (self.root / "zulu").mkdir()
        (self.root / "Alpha").mkdir()
        (self.root / ".private").mkdir()
        (self.root / "plain-file").write_text("fixture", encoding="utf-8")

        response = self.client.get(f"/api/directories?path={self.root}&path=/does/not/exist")

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
        with patch("codex_native_runtime.status", return_value=None) as native, \
                patch("codex_browser.diagnostics", return_value=None) as browser, \
                patch.dict(os.environ, {"CODEX_AGENTS_SUPERVISOR_MODE": "0"}, clear=False):
            self.context.canvas.runtime = runtime
            response = self.client.get("/api/desktop?account_key=fixture-account")

        self.assertEqual(response.status_code, 200)
        body = response.json()
        self.assertEqual(body["application"], "codex-agents")
        self.assertEqual(body["publicOrigin"], "https://studio.example")
        self.assertEqual(body["liveUpdate"], {"status": "idle", "pid": 3})
        native.assert_called_once_with(runtime)
        browser.assert_called_once_with(runtime, "fixture-account")

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
            "nativeAccounts": {"account1": {"provider": "claude", "liveQueries": 1}},
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
            "analyticsCapture": {"available": False},
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
        from codex_diagnostics import snapshot
        from codex_execution import ensure_tables

        db = sqlite3.connect(":memory:")
        self.addCleanup(db.close)
        ensure_tables(db)
        db.execute("CREATE TABLE runtime_events(status TEXT)")
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
            result = snapshot(runtime, root_pid=10, ps_output="10 1 1024 0.0 codex-canvas")
        result["supervisor"] = {"mode": False, "fallback": False, "notice": None}

        DiagnosticsResponse.model_validate_json(json.dumps(result))

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

    def test_process_kind_enum_accepts_wire_string_and_rejects_unknown_kind(self) -> None:
        record = '{"pid":1,"parentPid":0,"kind":"codex_app_server","rssMiB":1.0,"cpuPercent":0.0}'
        self.assertEqual(ProcessRecord.model_validate_json(record).kind.value, "codex_app_server")
        invalid = record.replace("codex_app_server", "unexpected_process")
        with self.assertRaises(ValidationError):
            ProcessRecord.model_validate_json(invalid)


if __name__ == "__main__":
    unittest.main()
