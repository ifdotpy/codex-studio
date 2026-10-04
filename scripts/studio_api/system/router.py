"""FastAPI endpoints for desktop status and read-only diagnostics."""
from __future__ import annotations

import json
import os
from pathlib import Path
from typing import TYPE_CHECKING, Callable, cast

from fastapi import APIRouter, Depends, HTTPException, Request, status
from fastapi.responses import Response

from studio_api.models import ErrorResponse, JsonValue
from .models import (
    DesktopQuery,
    DesktopResponse,
    DiagnosticsResponse,
    DirectoriesQuery,
    DirectoriesResponse,
)

if TYPE_CHECKING:
    from studio_api.context import ApiContext


NativeRuntimeStatus = Callable[[object], dict[str, JsonValue] | None]
ProviderVersionStatus = Callable[[object], dict[str, JsonValue]]
BrowserDiagnostics = Callable[[object, str], dict[str, JsonValue] | None]
SupervisorStatus = Callable[[str], dict[str, JsonValue]]
DiagnosticsSnapshot = Callable[[object], dict[str, JsonValue]]


def create_router(context: ApiContext) -> APIRouter:
    router = APIRouter()

    @router.get(
        "/api/desktop",
        response_model=DesktopResponse,
        responses={400: {"model": ErrorResponse}},
    )
    def desktop(request: Request, query: DesktopQuery = Depends()) -> Response:
        from codex_backend_identity import BACKEND_BUILD
        from codex_browser import diagnostics as untyped_browser_diagnostics
        from codex_native_runtime import status as untyped_native_runtime_status
        from codex_provider_versions import status as untyped_provider_version_status

        browser_diagnostics = cast(BrowserDiagnostics, untyped_browser_diagnostics)
        native_runtime_status = cast(NativeRuntimeStatus, untyped_native_runtime_status)
        provider_version_status = cast(ProviderVersionStatus, untyped_provider_version_status)

        canvas = context.canvas
        try:
            saved_recovery = json.loads((Path(canvas.root) / "background-recovery.json").read_text())
        except FileNotFoundError:
            saved_recovery = {}
        supervisor_required = saved_recovery.get("supervisorEnabled") is True
        supervisor_mode = os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") == "1"
        supervisor_fallback = os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") == "1"
        supervisor_error = (
            "Supervisor mode is enabled in background-recovery.json, but this backend did not start in supervisor mode. AppServers are not being started; restart through the supervisor recovery service."
            if supervisor_required and not supervisor_mode and not supervisor_fallback else None
        )
        supervisor_status: object | None = None
        if supervisor_mode or (Path(canvas.root) / "supervisor.sock").exists():
            from codex_process_supervisor import status as untyped_process_supervisor_status

            process_supervisor_status = cast(SupervisorStatus, untyped_process_supervisor_status)

            try:
                supervisor_status = process_supervisor_status(str(canvas.root))
            except (OSError, RuntimeError, ValueError) as error:
                supervisor_status = {"error": str(error)[:300]}

        account_keys = request.query_params.getlist("account_key")
        browser_account = next((value for value in account_keys if value), "default")
        runtime = canvas.runtime
        provider_versions = (
            {"checkedAt": None, "providers": [], "warnings": []}
            if runtime is None else provider_version_status(runtime)
        )
        result = {
            "application": "codex-agents",
            "protocol": 1,
            "mobileProtocol": 1,
            "backendBuild": BACKEND_BUILD,
            "nativeRuntime": native_runtime_status(runtime),
            "providerVersions": provider_versions,
            "browser": browser_diagnostics(runtime, browser_account),
            "liveUpdate": (
                runtime.live_updates.status()
                if getattr(runtime, "live_updates", None) else None
            ),
            "restartEnvironment": {
                key: os.environ[key]
                for key in (
                    "CODEX_HOME", "CODEX_CANVAS_CWD", "CODEX_CANVAS_CONCURRENCY",
                    "CODEX_BIN", "SHELL", "LANG", "LC_ALL",
                    "CODEX_AGENTS_SUPERVISOR_MODE",
                )
                if key in os.environ
            },
            "publicOrigin": context.remote.origin(),
            "pid": os.getpid(),
            "supervisorMode": supervisor_mode,
            "supervisorRequired": supervisor_required,
            "supervisorError": supervisor_error,
            "supervisor": supervisor_status,
            "supervisorFallback": supervisor_fallback,
            "supervisorNotice": (
                "The process supervisor stopped unexpectedly. Studio applied normal recovery to turns, monitors, and terminals with reduced restart protection; accepted or uncertain operations were not resubmitted."
                if supervisor_fallback else None
            ),
            "stateDir": str(Path(canvas.root).resolve()),
        }
        return context.send(request, result)

    @router.get(
        "/api/diagnostics",
        response_model=DiagnosticsResponse,
        responses={400: {"model": ErrorResponse}, 404: {"model": ErrorResponse}},
    )
    def diagnostics(request: Request) -> Response:
        runtime = context.runtime
        if runtime is None:
            raise HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail="Not found")
        from codex_diagnostics import snapshot as untyped_diagnostics_snapshot

        diagnostics_snapshot = cast(DiagnosticsSnapshot, untyped_diagnostics_snapshot)

        result = diagnostics_snapshot(runtime)
        supervisor: dict[str, JsonValue] = {
            "mode": os.environ.get("CODEX_AGENTS_SUPERVISOR_MODE") == "1",
            "fallback": os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") == "1",
            "notice": (
                "The process supervisor stopped unexpectedly. Studio applied normal recovery to turns, monitors, and terminals with reduced restart protection; accepted or uncertain operations were not resubmitted."
                if os.environ.get("CODEX_AGENTS_SUPERVISOR_FALLBACK") == "1" else None
            ),
        }
        if supervisor["mode"] is True:
            try:
                from codex_process_supervisor import status as untyped_process_supervisor_status

                process_supervisor_status = cast(SupervisorStatus, untyped_process_supervisor_status)

                supervisor["health"] = process_supervisor_status(str(context.canvas.root))
            except (OSError, RuntimeError, ValueError) as error:
                supervisor["health"] = {"error": str(error)[:300]}
        result["supervisor"] = supervisor
        return context.send(request, result)

    @router.get(
        "/api/directories",
        response_model=DirectoriesResponse,
        responses={400: {"model": ErrorResponse}},
    )
    def directories(request: Request, query: DirectoriesQuery = Depends()) -> Response:
        paths = request.query_params.getlist("path")
        raw_path = next((value for value in paths if value), os.getcwd())
        directory = Path(raw_path).expanduser().resolve()
        if not directory.is_dir():
            raise HTTPException(status_code=400, detail="This directory is unavailable")
        try:
            folders = sorted(
                (entry for entry in directory.iterdir()
                 if entry.is_dir() and not entry.name.startswith(".")),
                key=lambda entry: entry.name.lower(),
            )
        except (ValueError, RuntimeError, OSError) as error:
            raise HTTPException(status_code=400, detail=str(error)) from error
        result = {
            "path": str(directory),
            "parent": str(directory.parent) if directory != directory.parent else None,
            "directories": [{"name": entry.name, "path": str(entry)} for entry in folders[:500]],
        }
        return context.send(request, result)

    return router
