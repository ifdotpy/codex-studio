"""Workspace files, checkpoints, conversation forks, and capability discovery."""
from __future__ import annotations

from collections.abc import Iterable, Sequence
from concurrent.futures import ThreadPoolExecutor
from contextlib import AbstractContextManager
import sqlite3
import threading
from typing import TYPE_CHECKING, Any, Protocol

import base64
import codecs
import hashlib
import json
import mimetypes
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

from codex_records import (
    AccountDataRecord, AgentRecord, CheckpointRecord, JsonObject, ProjectRecord, RecordStore,
    WorkspaceOperationRecord,
)
from codex_native_errors import NativeRpcError
from codex_work import _WorkHost
from codex_accounts import AccountStore
from codex_safety_buffering import active as safety_retry_active
from codex_work import text_field
from codex_entity_contracts import (ACTIVE_MONITOR_STATUSES, monitor_records, task_records)

if TYPE_CHECKING:
    from codex_runtime import Runtime

def active_task_records(db: sqlite3.Connection, statuses: Iterable[str] = ("running",), *,
                        agent: str | None = None) -> list[Any]:
    """Use the status index before loading task payloads under the runtime lock."""
    # Task producers share this unvalidated JSON table; callers perform the
    # shape checks appropriate to their task kind.
    if not statuses:
        return []
    values = tuple(statuses)
    scope = " AND json_extract(record,'$.agent')=?" if agent is not None else ""
    rows = db.execute(
        "SELECT record FROM runtime_tasks WHERE json_extract(record,'$.status') IN ("
        + ",".join("?" for _ in values) + ")" + scope,
        (*values, agent) if agent is not None else values,
    )
    return [json.loads(row[0]) for row in rows]


SKILL_CATALOG_TIMEOUT_SECONDS = 5


def active_monitors(db: sqlite3.Connection) -> list[JsonObject]:
    """Monitors that can still run. The status index skips finished history."""
    return [json.loads(r[0]) for r in db.execute(
        "SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') IN (?,?,?) ORDER BY rowid",
        ACTIVE_MONITOR_STATUSES)]

class WorkspaceNativeServer(Protocol):
    def call(self, method: str, params: JsonObject, timeout: float = ...) -> JsonObject: ...


class WorkspaceRuntime(_WorkHost, Protocol):
    root: Path
    lock: AbstractContextManager[object]
    changed: threading.Event
    closed: bool
    pool: ThreadPoolExecutor
    prepare_locks: dict[str, object]
    accounts: AccountStore
    capability_cache: dict[str, JsonObject]
    loaded: set[str]

    def db(self, *, busy_timeout: int | None = None) -> AbstractContextManager[sqlite3.Connection]: ...
    def read_db(self) -> AbstractContextManager[sqlite3.Connection]: ...
    def named_agents(self, db: sqlite3.Connection, agent_ids: Iterable[object]) -> dict[str, AgentRecord]: ...
    def permanent_worker_hold(self, db: sqlite3.Connection, agent: AgentRecord, operation_id: str,
                              transition: str, reason: str) -> None: ...
    def sync_agent_rooms(self, db: sqlite3.Connection, room_ids: Iterable[str]) -> None: ...
    def project_room_ids(self, db: sqlite3.Connection, project_id: str) -> list[str]: ...
    def connect(self, account_key: str = "default", *, for_login: bool = False) -> WorkspaceNativeServer: ...
    def thread_config(self) -> JsonObject: ...
    def new_thread_params(self, agent: AgentRecord) -> JsonObject: ...
    def search_is_indexed(self, db: sqlite3.Connection, key: str) -> bool: ...
    def send(self, key: str, text: str, message_id: str | None = None, manual: bool = True,
             resume: bool = False, delivery: str = "queue", assets: list[JsonObject] | None = None,
             sender: str | None = None, sender_epoch: int | None = None,
             radio_question: JsonObject | None = None) -> JsonObject: ...
    def tool_definitions(self, agent: AgentRecord) -> list[JsonObject]: ...
    def complaint_needs_response(self, complaint: JsonObject) -> bool: ...
    def complaint_recipient(self, complaint: JsonObject) -> str: ...
    def task_detail(self, key: str) -> JsonObject: ...
    def item(self, db: sqlite3.Connection, agent: str, key: str, role: str, text: str,
             title: str | None = None, inputs: list[JsonObject] | None = None, *,
             index_search: bool = True, **metadata: JsonValue) -> None: ...
    def index_item(self, db: sqlite3.Connection, key: str, agent: str, kind: str,
                   body: str | None = None) -> None: ...
    def parent_event(self, db: sqlite3.Connection, agent: AgentRecord, event_id: str, text: str,
                     *, recovery: bool = False) -> None: ...
# WorkspaceMixin methods are included so every self call is structurally checked.
    def setup_workspace(self, db: sqlite3.Connection) -> None: ...
    @staticmethod
    def _workspace_operation_id(kind: str, agent_id: str, data: JsonObject) -> str: ...
    @staticmethod
    def _workspace_operation_signature(body: JsonObject) -> str: ...
    @classmethod
    def _workspace_restore_signature(cls: type["WorkspaceMixin"], agent_id: str, data: JsonObject) -> str: ...
    @staticmethod
    def _workspace_provider_result(response: JsonObject) -> JsonObject: ...
    @staticmethod
    def _workspace_source(a: AgentRecord) -> JsonObject: ...
    def _assert_workspace_source(self, operation: WorkspaceOperationRecord, agent: AgentRecord) -> None: ...
    def _workspace_operation(self, db: sqlite3.Connection, operation_id: str) -> WorkspaceOperationRecord | None: ...
    def _workspace_operations(self, db: sqlite3.Connection, agent_id: str=None) -> list[WorkspaceOperationRecord]: ...
    def _put_workspace_operation(self, db: sqlite3.Connection, operation: WorkspaceOperationRecord) -> None: ...
    def _update_workspace_operation(self, operation_id: str, **changes: JsonValue) -> WorkspaceOperationRecord: ...
    def _workspace_provider_rejected(self, error: BaseException | str | None) -> bool: ...
    def _finish_workspace_operation(self, operation_id: str, agent_id: str, *, result: JsonObject=None, error: BaseException | str | None=None) -> None: ...
    def _require_workspace_recovery(self, operation_id: str, agent_id: str, error: BaseException | str | None) -> None: ...
    def _workspace_operation_busy(self, db: sqlite3.Connection, cwd: str | Path, exclude_operation: str | None=None) -> bool: ...
    @staticmethod
    def project_directory(value: object, require_existing: bool=True) -> str: ...
    def project_account(self, cwd: str | Path, db: sqlite3.Connection=None) -> str: ...
    def project_worker_base(self, cwd: str | Path, db: sqlite3.Connection=None) -> str | None: ...
    def ensure_project(self, path: str | Path, account_key: str, db: sqlite3.Connection) -> ProjectRecord: ...
    def projects(self, data: JsonObject=None, db: sqlite3.Connection=None) -> JsonObject: ...
    def workspace_path(self, agent_id: str, path: str) -> Path: ...
    def upload_asset(self, data: JsonObject) -> JsonObject: ...
    def checked_actor_in_own_db(self, key: str, actor: str | None=None) -> AgentRecord: ...
    @staticmethod
    def asset_view(asset: JsonObject) -> JsonObject: ...
    def asset_record(self, key: str, db: sqlite3.Connection | None = None) -> JsonObject: ...
    def message_inputs(self, agent_id: str, text: str, asset_ids: Sequence[str]) -> JsonObject: ...
    def file_info(self, agent_id: str | None = None, path: str | Path | None = None, asset_id: str | None = None) -> JsonObject: ...
    def file_content(self, agent_id: str | None = None, path: str | Path | None = None,
                     asset_id: str | None = None, limit: int = 20 * 1024 * 1024) -> bytes: ...
    def _image_file_size(self, file: Path, agent: AgentRecord | None = None) -> int: ...
    def _read_image_file(self, file: Path, limit: int, agent: AgentRecord | None = None) -> bytes: ...
    def git(self, a: AgentRecord, args: Sequence[str], env: dict[str, str]=None, input: bytes | None=None) -> subprocess.CompletedProcess[bytes]: ...
    @staticmethod
    def reported_change_files(patch: str) -> list[str]: ...
    def reported_changes(self, agent_id: str) -> JsonObject: ...
    def changes(self, agent_id: str, scope: str | None=None) -> JsonObject: ...
    def snapshot_tree(self, a: AgentRecord) -> str: ...
    def _reserve_checkpoint(self, db: sqlite3.Connection, agent: AgentRecord, kind: str, turn_id: str | None=None) -> str: ...
    def queue_checkpoint_after_turn(self, db: sqlite3.Connection, agent: AgentRecord, turn_id: str | None) -> None: ...
    def _checkpoint_completed_agent(self, db: sqlite3.Connection, request: tuple[str, int, str, str, str | None, str, str | None]) -> AgentRecord | None: ...
    def _checkpoint_after_committed_turn(self, request: tuple[str, int, str, str, str | None, str, str | None]) -> None: ...
    def _settle_checkpoint(self, db: sqlite3.Connection, agent: AgentRecord, operation_id: str, error: BaseException | str | None=None) -> None: ...
    def _capture_reserved_checkpoint(self, key: str, label: str, turn_id: str | None, operation_id: str) -> CheckpointRecord | None: ...
    def checkpoint_capture(self, agent_id: str, label: str='Checkpoint', turn_id: str | None=None, internal: bool=False) -> CheckpointRecord: ...
    def capture_checkpoint(self, agent_id: str, label: str='Checkpoint', turn_id: str | None=None, tree: str | None=None) -> CheckpointRecord: ...
    def _checkpoint_history_ids(self, db: sqlite3.Connection, checkpoint: CheckpointRecord) -> list[str]: ...
    @staticmethod
    def checkpoint_summary(checkpoint: CheckpointRecord) -> JsonObject: ...
    def checkpoint_after_turn(self, key: str, turn_id: str | None, operation_id: str) -> None: ...
    def checkpoint_preview(self, key: str, checkpoint_id: str) -> JsonObject: ...
    def restore_checkpoint(self, key: str, data: JsonObject) -> JsonObject: ...
    def _restore_checkpoint_locked(self, key: str, data: JsonObject) -> JsonObject: ...
    def assert_workspace_idle(self, a: AgentRecord) -> None: ...
    def _workspace_idle_snapshot(self, db: sqlite3.Connection, a: AgentRecord, *, current_state: bool=False) -> JsonObject: ...
    def _resolve_workspace_idle(self, snapshot: JsonObject, a: AgentRecord) -> Path: ...
    def _assert_workspace_idle(self, db: sqlite3.Connection, a: AgentRecord, reservation_id: str | None=None, *, current_state: bool=False, snapshot: JsonObject=None, resolved: Path | None=None) -> None: ...
    def branch_conversation(self, key: str, data: JsonObject) -> JsonObject: ...
    def branch_locked(self, key: str, data: JsonObject) -> JsonObject: ...
    def profiles(self, data: JsonObject | None = None) -> Any: ...  # typed-suspect: same name has an incompatible MRO return shape in EfficiencyMixin
    def skill_catalog(self, key: str) -> JsonObject: ...
    def capabilities(self, key: str) -> JsonObject: ...
    def workspace_snapshot(self, key: str=None, *, view: str='full') -> JsonObject: ...
    def _workspace_records(self, db: sqlite3.Connection, table: str, ids: Iterable[str], field: str) -> list[JsonObject]: ...
    @staticmethod
    def _workspace_requests(db: sqlite3.Connection, ids: Iterable[str]) -> list[JsonObject]: ...
    @staticmethod
    def _workspace_work(db: sqlite3.Connection, root: Path) -> list[JsonObject]: ...
    @staticmethod
    def _workspace_rules(db: sqlite3.Connection, ids: Iterable[str]) -> list[JsonObject]: ...
    def _workspace_complaints(self, db: sqlite3.Connection, lead_ids: Iterable[str] | None) -> list[JsonObject]: ...
    def monitor_log(self, key: str) -> JsonObject: ...
    def native_command_action(self, data: JsonObject) -> JsonObject: ...
    def workspace_blockers(self, db: sqlite3.Connection, a: AgentRecord) -> list[dict[str, object]]: ...
    def assert_workspace_available(self, db: sqlite3.Connection, a: AgentRecord) -> None: ...
    def recent_tasks(self, db: sqlite3.Connection, root: str | None = None) -> list[JsonObject]: ...
    def recent_monitors(self, db: sqlite3.Connection, root: str | None = None) -> list[JsonObject]: ...

class WorkspaceMixin:
    WORKSPACE_OPERATION_ACTIVE = {
        "provider_pending",
        "provider_ready",
        "local_mutation",
        "recovery_required",
        "capture_pending",
        "capture_running",
    }

    def setup_workspace(self: "WorkspaceRuntime", db: sqlite3.Connection) -> None:
        db.executescript("""
            CREATE TABLE IF NOT EXISTS runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_sidebar_order (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_workspace_operation_phase ON runtime_workspace_operations(
                json_extract(record,'$.phase'), json_extract(record,'$.agent'));
        """)
        # Capture uses a separate Git index and never restores workspace files or
        # changes the native thread. A dead process cannot retain its reservation.
        for operation in self._workspace_operations(db):
            if operation.get("kind") in {"checkpoint", "capture"}:
                operation.update(phase="failed", updated=time.time(),
                                 error="Server restarted before checkpoint capture finished")
                self.put(db, "workspace_operations", operation)
        active = {
            operation["id"]: operation
            for operation in self._workspace_operations(db)
        }
        # Startup recovery runs once; scanning the roster here also catches legacy
        # reservations that have no workspace-operation row to name their agent.
        for a in self.records(db, "agents"):
            operations = [operation for operation in active.values() if operation.get("agent") == a["id"]]
            if operations:
                operation = operations[0]
                operation["restartHold"] = True
                if operation.get("kind") == "restore" or operation.get("phase") == "provider_pending":
                    operation["phase"] = "recovery_required"
                    operation["error"] = (
                        "Server restarted during a workspace operation. "
                        "Inspect the workspace before retrying."
                    )
                    operation["updated"] = time.time()
                self.put(db, "workspace_operations", operation)
                a.update(
                    workspaceOperation=(
                        "restore_recovery"
                        if operation.get("kind") == "restore"
                        else "branch"
                    ),
                    autoWake=False,
                    status="interrupted",
                    error=operation.get("error")
                    or "Workspace operation interrupted. Inspect files before continuing.",
                )
                self.put(db, "agents", a)
                self.permanent_worker_hold(
                    db, a, operation["id"], "recovery-required",
                    operation.get("error") or a.get("error")
                    or "Workspace operation recovery is required.",
                )
            elif a.get("workspaceOperation") in {"checkpoint", "capture"}:
                a.update(workspaceOperation=None,
                         checkpointError="Server restarted before checkpoint capture finished")
                a.pop("workspaceReservationId", None)
                self.put(db, "agents", a)
            elif a.get("workspaceOperation"):
                a.update(
                    workspaceOperation=None,
                    autoWake=False,
                    status="interrupted",
                    error="Workspace operation interrupted. Inspect files before continuing.",
                )
                self.put(db, "agents", a)
                self.permanent_worker_hold(
                    db, a, "workspace:" + str(a.get("workspaceOperation")),
                    "restart-held", a["error"],
                )
        self.capability_cache: dict[str, JsonObject] = {}

    @staticmethod
    def _workspace_operation_id(kind: str, agent_id: str, data: JsonObject) -> str:
        if kind == "branch" and data.get("id"):
            return "branch:" + str(data["id"])
        value = data.get("checkpoint_id") or data.get("checkpoint")
        if kind == "restore":
            expected = str(data.get("expectedTree"))
            digest = hashlib.sha256(expected.encode()).hexdigest()[:32]
            return "restore:" + str(agent_id) + ":" + str(value) + ":" + digest
        return "branch:" + str(agent_id) + ":" + str(data.get("message_id"))

    @staticmethod
    def _workspace_operation_signature(body: JsonObject) -> str:
        return hashlib.sha256(json.dumps(body, sort_keys=True).encode()).hexdigest()

    @classmethod
    def _workspace_restore_signature(cls: type["WorkspaceMixin"], agent_id: str, data: JsonObject) -> str:
        return cls._workspace_operation_signature(
            {
                "agent": agent_id,
                "checkpoint": data.get("checkpoint_id") or data.get("checkpoint"),
                "expectedTree": data.get("expectedTree"),
            }
        )

    @staticmethod
    def _workspace_provider_result(response: JsonObject) -> JsonObject:
        thread = response.get("thread") if isinstance(response, dict) else None
        thread_id = thread.get("id") if isinstance(thread, dict) else None
        if not isinstance(thread_id, str) or not thread_id:
            raise ValueError("The native workspace response has no thread id")
        result = {"thread": {"id": thread_id}}
        for field in ("sandbox", "approvalPolicy", "activePermissionProfile", "model"):
            if field in response:
                result[field] = response[field]
        return result

    @staticmethod
    def _workspace_source(a: AgentRecord) -> JsonObject:
        return {
            "accountKey": a.get("accountKey", "default"),
            "threadId": a.get("threadId"),
            "cwd": a["cwd"],
        }

    def _assert_workspace_source(self: "WorkspaceRuntime", operation: WorkspaceOperationRecord, agent: AgentRecord) -> None:
        expected = operation.get("source")
        if expected and expected != self._workspace_source(agent):
            raise ValueError(
                "Workspace operation source changed. Inspect the operation before retrying"
            )

    def _workspace_operation(self: "WorkspaceRuntime", db: sqlite3.Connection, operation_id: str) -> WorkspaceOperationRecord | None:
        row = db.execute(
            "SELECT record FROM runtime_workspace_operations WHERE id=?",
            (operation_id,),
        ).fetchone()
        return json.loads(row[0]) if row else None

    def _workspace_operations(self: "WorkspaceRuntime", db: sqlite3.Connection, agent_id: str=None) -> list[WorkspaceOperationRecord]:
        phases = tuple(sorted(self.WORKSPACE_OPERATION_ACTIVE))
        query = ("SELECT record FROM runtime_workspace_operations "
                 "WHERE json_extract(record,'$.phase') IN ("
                 + ",".join("?" for _ in phases) + ")")
        params = phases
        if agent_id is not None:
            query += " AND json_extract(record,'$.agent')=?"
            params += (agent_id,)
        return [json.loads(row[0]) for row in db.execute(query, params)]

    def _put_workspace_operation(self: "WorkspaceRuntime", db: sqlite3.Connection, operation: WorkspaceOperationRecord) -> None:
        self.put(db, "workspace_operations", operation)

    def _update_workspace_operation(self: "WorkspaceRuntime", operation_id: str, **changes: JsonValue) -> WorkspaceOperationRecord:
        with self.lock, self.db() as db:
            operation = self._workspace_operation(db, operation_id)
            if operation is None:
                raise ValueError("Workspace operation record is missing")
            operation.update(changes, updated=time.time())
            self._put_workspace_operation(db, operation)
            return operation

    def _workspace_provider_rejected(self: "WorkspaceRuntime", error: BaseException | str | None) -> bool:
        return isinstance(error, NativeRpcError)

    def _finish_workspace_operation(self: "WorkspaceRuntime", operation_id: str, agent_id: str, *, result: JsonObject=None, error: BaseException | str | None=None) -> None:
        with self.lock, self.db() as db:
            operation = self._workspace_operation(db, operation_id)
            if operation is not None:
                operation.update(
                    phase="completed" if error is None else "failed",
                    result=result,
                    error=str(error) if error is not None else None,
                    updated=time.time(),
                )
                self._put_workspace_operation(db, operation)
            agent = self.agent(agent_id, db)
            if agent.get("workspaceOperation") in {
                "restore",
                "restore_recovery",
                "branch",
                "branch_recovery",
            }:
                agent["workspaceOperation"] = None
                # Native completion and user stop own the current source status.
                # Finishing a fork must not restore an older running state.
                self.put(db, "agents", agent)

    def _require_workspace_recovery(self: "WorkspaceRuntime", operation_id: str, agent_id: str, error: BaseException | str | None) -> None:
        message = "Workspace operation outcome is unknown. Inspect the workspace before retrying."
        try:
            with self.lock, self.db() as db:
                operation = self._workspace_operation(db, operation_id)
                if operation is not None:
                    operation.update(
                        phase="recovery_required",
                        error=str(error),
                        updated=time.time(),
                    )
                    self._put_workspace_operation(db, operation)
                agent = self.agent(agent_id, db)
                agent.update(
                    workspaceOperation=(
                        "restore_recovery"
                        if operation and operation.get("kind") == "restore"
                        else "branch_recovery"
                    ),
                    autoWake=False,
                    status="interrupted",
                    error=message,
                )
                self.put(db, "agents", agent)
                self.permanent_worker_hold(
                    db, agent, operation_id, "recovery-required",
                    str(error) or message,
                )
        except Exception:
            # The original failure remains visible. The prepared operation and
            # agent marker already provide a durable hold if this write fails.
            pass

    def _workspace_operation_busy(self: "WorkspaceRuntime", db: sqlite3.Connection, cwd: str | Path, exclude_operation: str | None=None) -> bool:
        return any(
            Path(agent["cwd"]).resolve() == Path(cwd).resolve()
            for operation in self._workspace_operations(db)
            if operation["id"] != exclude_operation
            for agent in self.named_agents(db, (operation.get("agent"),)).values()
        )

    @staticmethod
    def project_directory(value: object, require_existing: bool=True) -> str:
        if isinstance(value, Path):
            value = str(value)
        text_field(value, "a project path", 4096)
        path = Path(value).expanduser().resolve()
        if require_existing and not path.is_dir():
            raise ValueError("Select an existing project directory")
        return str(path)

    def project_account(self: "WorkspaceRuntime", cwd: str | Path, db: sqlite3.Connection=None) -> str:
        if db is None:
            with self.lock, self.db() as connection:
                return self.project_account(cwd, db=connection)
        directory = Path(self.project_directory(cwd, require_existing=False))
        matches = [p for p in self.records(db, "projects")
                   if p.get("accountKey") and directory.is_relative_to(Path(p["path"]).expanduser().resolve())]
        if matches:
            return max(matches, key=lambda p: len(Path(p["path"]).parts))["accountKey"]
        return self.accounts.default()

    def project_worker_base(self: "WorkspaceRuntime", cwd: str | Path, db: sqlite3.Connection=None) -> str | None:
        if db is None:
            with self.lock, self.db() as connection:
                return self.project_worker_base(cwd, db=connection)
        directory = Path(self.project_directory(cwd, require_existing=False))
        matches = [p for p in self.records(db, "projects")
                   if p.get("workerBaseRef") and directory.is_relative_to(
                       Path(p["path"]).expanduser().resolve())]
        if matches:
            return max(matches, key=lambda p: len(Path(p["path"]).parts))["workerBaseRef"]
        return None

    def ensure_project(self: "WorkspaceRuntime", path: str | Path, account_key: str, db: sqlite3.Connection) -> ProjectRecord:
        """Register a chat project in its transaction; preserve an existing choice."""
        path = self.project_directory(path, require_existing=False)
        existing = db.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
        if existing:
            return json.loads(existing[0])
        self.accounts.get(account_key)
        project = {"id": path, "path": path, "name": Path(path).name or path,
                   "created": time.time(), "accountKey": account_key, "accountRevision": 1}
        self.put(db, "projects", project)
        return project

    def projects(self: "WorkspaceRuntime", data: JsonObject=None, db: sqlite3.Connection=None) -> JsonObject:
        if data is None:
            if db is None:
                with self.lock, self.db() as connection:
                    return self.projects(db=connection)
            return {"items": sorted(self.records(db, "projects"), key=lambda p: (p["created"], p["id"]))}
        action = data.get("action", "register")
        if action == "register" and data.get("server"):
            from codex_project_locations import create as create_remote_project
            return create_remote_project(self, data)
        if action in {"add_location", "remove_location"}:
            from codex_project_locations import request as location_request
            return location_request(self, data)
        if action == "reorder":
            from codex_project_folders import reorder_sidebar
            return reorder_sidebar(self, data)  # type: ignore[arg-type]  # typed-narrowing: workspace host protocol supplies the project-folder runtime interface
        if action in ('rename', 'add_folder', 'rename_folder', 'remove_folder'):
            from codex_project_folders import organize_project
            return organize_project(self, data)  # type: ignore[arg-type]  # typed-narrowing: workspace host protocol supplies the project-folder runtime interface
        if action == "set_accounts":
            from codex_project_accounts import set_project_accounts
            return set_project_accounts(self, data)  # type: ignore[arg-type]  # typed-narrowing: workspace host protocol supplies the project-account runtime interface
        if action == "set_worker_environment":
            from codex_worker_environment import set_project_default
            return set_project_default(self, data)
        if action not in ("register", "remove", "set_account", "set_worker_base"):
            raise ValueError("Unknown project action")
        from codex_project_locations import project_key
        path = project_key(self, data.get("path"), require_existing=action == "register")
        name = text_field(data["name"], "a project name", 255) if "name" in data else Path(path).name or path
        if action == "set_account":
            revision = data.get("expected_revision")
            if type(revision) is not int or revision < 0:
                raise ValueError("Supply the current project account revision")
        if action == "set_worker_base":
            revision = data.get("expected_revision")
            if type(revision) is not int or revision < 0:
                raise ValueError("Supply the current worker base revision")
            worker_base = data.get("base_ref")
            if worker_base is not None and (
                    not isinstance(worker_base, str) or len(worker_base.strip()) > 1024):
                raise ValueError("Worker base ref must be empty or at most 1024 characters")
            worker_base = worker_base.strip() or None if isinstance(worker_base, str) else None
        with self.lock, self.db() as connection:
            connection.execute("BEGIN IMMEDIATE")
            if action == "remove":
                removed = connection.execute("DELETE FROM runtime_projects WHERE id=?", (path,)).rowcount
                if removed:
                    from codex_sync_entities import put as sync_entity_put
                    sync_entity_put(connection, "project", path, {}, deleted=True)
                    self.sync_agent_rooms(connection, self.project_room_ids(connection, path))
                    from codex_peer_teams import sync_entities as sync_peer_team_entities
                    sync_peer_team_entities(self, connection, {path})
                return {"id": path, "removed": bool(removed)}
            existing = connection.execute("SELECT record FROM runtime_projects WHERE id=?", (path,)).fetchone()
            project = json.loads(existing[0]) if existing else None
            if action == "register" and project:
                return project
            if action == "set_account":
                current = project.get("accountRevision", 0) if project else 0
                desired = data["account_key"]
                if project and project.get("accountKey") == desired and revision in (current, current - 1):
                    return project
                if revision != current:
                    raise ValueError("Project account changed. Reload it before saving")
                if self.accounts.get(desired).get("disconnected"):
                    raise ValueError("Reconnect this account before selecting it")
            elif action == "set_worker_base":
                current = project.get("workerBaseRevision", 0) if project else 0
                if project and project.get("workerBaseRef") == worker_base and revision in (current, current - 1):
                    return project
                if revision != current:
                    raise ValueError("Worker base changed. Reload it before saving")
            else:
                current = 0
                desired = data.get("account_key") or self.project_account(path, db=connection)
                self.accounts.get(desired)
            if project is None:
                project = {"id": path, "path": path, "name": name, "created": time.time()}
            if action == "set_worker_base":
                project.update(workerBaseRef=worker_base, workerBaseRevision=current + 1)
            else:
                project.update(accountKey=desired, accountRevision=current + 1)
            if project.get("accountKeys") and desired not in project["accountKeys"]:
                project["accountKeys"] = sorted([*project["accountKeys"], desired])
            self.put(connection, "projects", project)
            return project

    def workspace_path(self: "WorkspaceRuntime", agent_id: str, path: str) -> Path:
        a = self.agent(agent_id)
        root = Path(a["cwd"]).resolve()
        supplied = Path(text_field(path, "a path", 4096)).expanduser()
        resolved = (supplied if supplied.is_absolute() else root / supplied).resolve()
        return resolved

    def upload_asset(self: "WorkspaceRuntime", data: JsonObject) -> JsonObject:
        agent = self.checked_actor_in_own_db(data.get("agent"))
        name = Path(text_field(data.get("name"), "a filename", 255)).name
        if name in {".", ".."}:
            raise ValueError("Invalid filename")
        try:
            content = base64.b64decode(
                data.get("base64", data.get("data", "")), validate=True
            )
        except (ValueError, TypeError):
            raise ValueError("Invalid file data")
        if not content or len(content) > 20 * 1024 * 1024:
            raise ValueError("Files must contain 1 byte to 20 MiB")
        key = str(uuid.UUID(data["id"])) if data.get("id") else str(uuid.uuid4())
        digest = hashlib.sha256(content).hexdigest()
        with self.lock, self.db() as db:
            old = db.execute(
                "SELECT record FROM runtime_assets WHERE id=?", (key,)
            ).fetchone()
            if old:
                asset = json.loads(old[0])
                if (asset["agent"], asset["hash"], asset["name"]) != (
                    agent["id"],
                    digest,
                    name,
                ):
                    raise ValueError("This upload id has different content")
                return self.asset_view(asset)
            directory = self.root / "uploads" / key
            directory.mkdir(parents=True, exist_ok=True)
            path = directory / name
            path.write_bytes(content)
            path.chmod(0o600)
            mime = mimetypes.guess_type(name)[0] or "application/octet-stream"
            image = (
                content.startswith(b"\x89PNG\r\n\x1a\n")
                or content.startswith(b"\xff\xd8\xff")
                or content.startswith((b"GIF87a", b"GIF89a"))
                or (content[:4] == b"RIFF" and content[8:12] == b"WEBP")
            )
            if mime.startswith("image/") and not image:
                mime = "application/octet-stream"
            asset = {
                "id": key,
                "agent": agent["id"],
                "name": name,
                "path": str(path),
                "mime": mime,
                "image": image,
                "size": len(content),
                "hash": digest,
                "created": time.time(),
            }
            self.put(db, "assets", asset)
            return self.asset_view(asset)

    def checked_actor_in_own_db(self: "WorkspaceRuntime", key: str, actor: str | None=None) -> AgentRecord:
        with self.lock, self.db() as db:
            return self.checked_actor(db, key, actor)

    @staticmethod
    def asset_view(asset: JsonObject) -> JsonObject:
        return {k: v for k, v in asset.items() if k != "path"}

    def asset_record(self: "WorkspaceRuntime", key: str, db: sqlite3.Connection | None = None) -> JsonObject:
        if db is None:
            with self.lock, self.db() as own:
                return self.asset_record(key, own)
        row = db.execute(
            "SELECT record FROM runtime_assets WHERE id=?", (key,)
        ).fetchone()
        if not row:
            raise ValueError("Unknown attachment")
        asset = json.loads(row[0])
        self.checked_actor(db, asset["agent"])
        return asset

    def message_inputs(self: "WorkspaceRuntime", agent_id: str, text: str, asset_ids: Sequence[str]) -> JsonObject:
        inputs = [{"type": "text", "text": text}]
        if not isinstance(asset_ids, list) or len(asset_ids) > 8:
            raise ValueError("Attach up to eight files")
        for key in asset_ids:
            asset = self.asset_record(key)
            # Explicit user attachments can be reused by a fork in the same project.
            a = self.agent(agent_id)
            owner = self.agent(asset["agent"])
            if a["rootId"] != owner["rootId"] and a.get("forkedFrom") != owner["id"]:
                raise ValueError("Attachment belongs to another conversation")
            if asset["image"]:
                inputs.append({"type": "localImage", "path": asset["path"]})
            else:
                inputs.append(
                    {
                        "type": "text",
                        "text": f"Attached file: {asset['name']}\nRead the file at {asset['path']} ({asset['size']} bytes).",
                    }
                )
        return inputs

    def file_info(self: "WorkspaceRuntime", agent_id: str | None = None, path: str | Path | None = None, asset_id: str | None = None) -> JsonObject:
        if asset_id:
            asset = self.asset_record(asset_id)
            file = Path(asset["path"]).resolve()
            mime = asset["mime"]
        else:
            self.checked_actor_in_own_db(agent_id)
            file = self.workspace_path(agent_id, path)
            mime = mimetypes.guess_type(file.name)[0] or "application/octet-stream"
        a = self.agent(agent_id) if agent_id and not asset_id else None
        if a and a.get("imageWorkspaceReady"):
            size = self._image_file_size(file, a)
        else:
            if not file.is_file():
                raise ValueError("This file does not exist")
            size = file.stat().st_size
        return {"path": str(file), "name": file.name, "mime": mime, "size": size}

    def file_content(
        self: "WorkspaceRuntime", agent_id: str | None = None, path: str | Path | None = None, asset_id: str | None = None, limit: int=20 * 1024 * 1024
    ) -> bytes:
        if asset_id:
            asset = self.asset_record(asset_id)
            file = Path(asset["path"])
            mime = asset["mime"]
        else:
            self.checked_actor_in_own_db(agent_id)
            file = self.workspace_path(agent_id, path)
            mime = mimetypes.guess_type(file.name)[0] or "application/octet-stream"
        a = self.agent(agent_id) if agent_id and not asset_id else None
        if a and a.get("imageWorkspaceReady"):
            content = self._read_image_file(file, limit, a)
        else:
            if not file.is_file():
                raise ValueError("This file does not exist")
            if file.stat().st_size > limit:
                raise ValueError("This file exceeds the 20 MiB preview limit")
            content = file.read_bytes()
        return content, mime, file.name

    def _image_file_size(self, file: Path, agent: AgentRecord | None = None) -> int:
        from codex_workspace_images import exec_prefix
        script = "import os,sys; p=sys.argv[1]; assert os.path.isfile(p); print(os.stat(p).st_size)"
        result = subprocess.run([*(self.workspace_exec_prefix(agent) if agent else exec_prefix()), "python3", "-c", script, str(file)],
                                capture_output=True, text=True, timeout=10)
        if result.returncode:
            raise ValueError("This file does not exist")
        return int(result.stdout.strip())

    def _read_image_file(self, file: Path, limit: int, agent: AgentRecord | None = None) -> bytes:
        from codex_workspace_images import exec_prefix
        script = ("import os,sys; p=sys.argv[1]; assert os.path.isfile(p); n=os.stat(p).st_size; "
                  "n > int(sys.argv[2]) and sys.exit(23); "
                  "sys.stdout.buffer.write(open(p,'rb').read(int(sys.argv[2])+1))")
        result = subprocess.run([*(self.workspace_exec_prefix(agent) if agent else exec_prefix()), "python3", "-c", script, str(file), str(limit)],
                                capture_output=True, timeout=30)
        if result.returncode == 23:
            raise ValueError("This file exceeds the 20 MiB preview limit")
        if result.returncode:
            raise ValueError("This file does not exist")
        if len(result.stdout) > limit:
            raise ValueError("This file exceeds the 20 MiB preview limit")
        return result.stdout

    def git(self, a, args, env=None, input=None):
        if a.get("imageWorkspace"):
            raise ValueError("Studio Git tools are unavailable for image workspaces")
        prefix = []
        if a.get("imageWorkspaceReady"):
            from codex_workspace_images import exec_prefix
            prefix = exec_prefix()
        result = subprocess.run(
            [*prefix, "git", "-C", a["cwd"], *args],
            input=input,
            capture_output=True,
            env=env,
            timeout=30,
        )
        if result.returncode:
            raise ValueError(
                result.stderr.decode(errors="replace").strip()[:1200]
                or "Git operation failed"
            )
        return result.stdout

    @staticmethod
    def reported_change_files(patch: str) -> list[str]:
        def header_path(value):  # type: (str) -> str | None
            if value.startswith('"'):
                if not re.fullmatch(r'"(?:[^"\\]|\\(?:[abfnrtv\\"]|[0-3][0-7]{2}))*"', value):
                    return None
                try:
                    return codecs.escape_decode(value[1:-1].encode("utf-8"))[0].decode("utf-8")
                except (ValueError, UnicodeError):
                    return None
            return value.split("\t", 1)[0] or None

        files = {}
        previous = None
        old_lines = new_lines = 0
        for line in patch.splitlines():
            hunk = re.match(r"^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@", line)
            if hunk:
                old_lines = int(hunk[1] or 1)
                new_lines = int(hunk[2] or 1)
                previous = None
                continue
            if old_lines or new_lines:
                if line.startswith(("-", " ")):
                    old_lines = max(0, old_lines - 1)
                if line.startswith(("+", " ")):
                    new_lines = max(0, new_lines - 1)
                continue
            if line.startswith("--- "):
                previous = header_path(line[4:])
            elif line.startswith("+++ ") and previous is not None:
                current = header_path(line[4:])
                if current is not None:
                    path = previous if current == "/dev/null" else current
                    prefix = "a/" if current == "/dev/null" else "b/"
                    if path.startswith(prefix):
                        path = path[2:]
                    if path and path != "/dev/null":
                        status = "D" if current == "/dev/null" else "A" if previous == "/dev/null" else "M"
                        files[path] = {"path": path, "status": status}
                previous = None
            else:
                previous = None
        return list(files.values())

    def reported_changes(self: "WorkspaceRuntime", agent_id: str) -> JsonObject:
        with self.lock, self.db() as db:
            self.checked_actor(db, agent_id)
            row = db.execute(
                "SELECT record FROM runtime_items WHERE agent=? AND (id=? OR id LIKE ?) ORDER BY created DESC LIMIT 1",
                (agent_id, agent_id + ":turn/diff/updated", agent_id + ":turn/diff/updated:%"),
            ).fetchone()
            record = json.loads(row[0]) if row else None
            result = {
                "scope": "chat", "git": True, "files": [], "diff": "", "patch": "",
                "truncated": False, "turnId": None, "reportedAt": None,
            }
            if record is None or record.get("afterRestore"):
                return result
            from codex_search_text import search_text
            full = search_text(db, record["id"])
            if record.get("truncated") and not full:
                raise ValueError("The complete reported changes are unavailable")
            try:
                payload = json.loads(full if full else record["text"])
                patch = payload["diff"]
                if not isinstance(patch, str):
                    raise ValueError("Invalid diff")
            except (ValueError, TypeError, KeyError):
                raise ValueError("The recorded changes are invalid") from None
            result.update(
                files=self.reported_change_files(patch), diff=patch[:300000], patch=patch[:300000],
                truncated=len(patch) > 300000, turnId=payload.get("turnId"), reportedAt=record.get("at"),
            )
            return result

    def changes(self: "WorkspaceRuntime", agent_id: str, scope: str | None=None) -> JsonObject:
        if scope == "chat":
            return self.reported_changes(agent_id)
        if scope is not None:
            raise ValueError("Unknown changes scope")
        a = self.checked_actor_in_own_db(agent_id)
        try:
            self.git(a, ["rev-parse", "--show-toplevel"])
            raw = self.git(a, ["status", "--porcelain=v1", "-z"])
            entries = raw.decode(errors="replace").split("\0")
            files = []
            i = 0
            while i < len(entries):
                entry = entries[i]
                i += 1
                if not entry:
                    continue
                state, path = entry[:2], entry[3:]
                if "R" in state or "C" in state:
                    i += 1
                files.append({"path": path, "status": state.strip()})
            patch = self.git(
                a, ["diff", "HEAD", "--no-ext-diff", "--no-color", "--unified=3"]
            ).decode(errors="replace")
            revision = self.git(a, ["rev-parse", "HEAD"]).decode().strip()
            return {
                "files": files,
                "diff": patch[:300000],
                "patch": patch[:300000],
                "truncated": len(patch) > 300000,
                "revision": revision,
                "git": True,
            }
        except ValueError as error:
            return {"files": [], "patch": "", "git": False, "error": str(error)}

    def snapshot_tree(self: "WorkspaceRuntime", a: AgentRecord) -> str:
        # An independent index preserves the user's staging area.
        with tempfile.TemporaryDirectory(
            prefix="checkpoint-", dir=self.root
        ) as directory:
            index = Path(directory) / "index"
            env = {**os.environ, "GIT_INDEX_FILE": str(index)}
            # Start from a copy of the real index: its file stat data lets `add -A`
            # hash only changed files. A fresh read-tree index hashes every file,
            # which takes minutes in a Chromium-size worktree.
            source = Path(a["cwd"]) / self.git(a, ["rev-parse", "--git-path", "index"]).decode().strip()
            if source.is_file():
                shutil.copyfile(source, index)
            else:
                self.git(a, ["read-tree", "HEAD"], env)
            self.git(a, ["add", "-A", "--", "."], env)
            return self.git(a, ["write-tree"], env).decode().strip()

    def _reserve_checkpoint(self, db, agent, kind, turn_id=None):
        operation = {
            "id": kind + ":" + str(uuid.uuid4()),
            "kind": kind, "agent": agent["id"], "cwd": agent["cwd"],
            "epoch": agent["epoch"], "turnId": turn_id,
            "source": self._workspace_source(agent),
            "phase": "capture_pending", "created": time.time(),
        }
        self._put_workspace_operation(db, operation)
        agent.update(workspaceOperation=kind, workspaceReservationId=operation["id"])
        self.put(db, "agents", agent)
        return operation["id"]

    def queue_checkpoint_after_turn(self, db, agent, turn_id):
        if agent.get("imageWorkspace") or not agent.get("worktreeReady"):
            return
        if agent.get("workspaceOperation"):
            agent["checkpointError"] = "Checkpoint skipped: An agent is using this workspace"
            return
        # The worker waits for the notification lock, then proves this turn's
        # committed receipt before resolving paths or reserving the workspace.
        request = (agent["id"], agent["epoch"], agent["cwd"],
                   agent.get("accountKey", "default"), agent.get("threadId"), turn_id,
                   (agent.get("startAttempt") or {}).get("id"))
        try:
            self.pool.submit(self._checkpoint_after_committed_turn, request)
        except Exception as error:
            agent["checkpointError"] = str(error)

    def _checkpoint_completed_agent(self, db, request):
        key, epoch, cwd, account, thread, turn_id, attempt = request
        if self.closed:
            return None
        agent = self.agent(key, db)
        if (agent.get("epoch") != epoch or agent.get("cwd") != cwd
                or agent.get("accountKey", "default") != account
                or agent.get("threadId") != thread or agent.get("lastCompletedTurn") != turn_id
                or agent.get("inFlight") or agent.get("turnId") or agent.get("workspaceOperation")
                or not agent.get("autoWake") or agent.get("status") in {"paused", "starting"}
                or agent.get("deletedAt") or agent.get("agentArchive")
                or (agent.get("startAttempt") or {}).get("id") != attempt):
            return None
        if not db.execute("SELECT 1 FROM runtime_completed_turns WHERE id=?",
                          (key + ":" + str(turn_id),)).fetchone():
            return None
        return agent

    def _checkpoint_after_committed_turn(self, request):
        with self.lock, self.db() as db:
            agent = self._checkpoint_completed_agent(db, request)
            if agent is None:
                return
            snapshot = self._workspace_idle_snapshot(db, agent)
        error = None
        try:
            resolved = self._resolve_workspace_idle(snapshot, agent)
        except Exception as cause:
            error = cause
        with self.lock, self.db() as db:
            agent = self._checkpoint_completed_agent(db, request)
            if agent is None:
                return
            try:
                if error is not None:
                    raise ValueError(str(error)) from error
                if self._workspace_idle_snapshot(db, agent) != snapshot:
                    raise ValueError("Workspace activity changed before checkpoint capture")
                self._assert_workspace_idle(db, agent, snapshot=snapshot, resolved=resolved)
            except ValueError as error:
                agent["checkpointError"] = "Checkpoint skipped: " + str(error)
                self.put(db, "agents", agent)
                return
            operation_id = self._reserve_checkpoint(db, agent, "checkpoint", request[5])
        self.checkpoint_after_turn(request[0], request[5], operation_id)

    def _settle_checkpoint(self: "WorkspaceRuntime", db: sqlite3.Connection, agent: AgentRecord, operation_id: str, error: BaseException | str | None=None) -> None:
        operation = self._workspace_operation(db, operation_id)
        if not operation or operation.get("kind") not in {"checkpoint", "capture"}:
            return
        if operation.get("phase") not in {"capture_pending", "capture_running"}:
            return
        operation.update(phase="failed" if error else "completed", updated=time.time(),
                         error=str(error) if error else None)
        self._put_workspace_operation(db, operation)
        if (agent.get("workspaceReservationId") == operation_id
                and agent.get("workspaceOperation") == operation["kind"]):
            agent.update(workspaceOperation=None, checkpointError=str(error) if error else None)
            agent.pop("workspaceReservationId", None)
            self.put(db, "agents", agent)

    def _capture_reserved_checkpoint(self, key, label, turn_id, operation_id):
        with self.lock, self.db() as db:
            agent = self.agent(key, db)
            operation = self._workspace_operation(db, operation_id)
            if not operation or operation.get("phase") != "capture_pending":
                return None
            if (agent.get("workspaceReservationId") != operation_id
                    or agent.get("workspaceOperation") != operation["kind"]
                    or agent["cwd"] != operation["cwd"]):
                self._settle_checkpoint(db, agent, operation_id,
                                        "Checkpoint reservation changed before capture")
                return None
            snapshot = self._workspace_idle_snapshot(db, agent)
        error = None
        try:
            resolved = self._resolve_workspace_idle(snapshot, agent)
        except Exception as cause:
            error = cause
        with self.lock, self.db() as db:
            agent = self.agent(key, db)
            operation = self._workspace_operation(db, operation_id)
            if not operation or operation.get("phase") != "capture_pending":
                return None
            if (agent.get("workspaceReservationId") != operation_id
                    or agent.get("workspaceOperation") != operation["kind"]
                    or agent["cwd"] != operation["cwd"] or agent["epoch"] != operation["epoch"]):
                self._settle_checkpoint(db, agent, operation_id,
                                        "Checkpoint reservation changed before capture")
                return None
            try:
                if error is not None:
                    raise ValueError(str(error)) from error
                self._assert_workspace_source(operation, agent)
                if self._workspace_idle_snapshot(db, agent) != snapshot:
                    raise ValueError("Workspace activity changed before checkpoint capture")
                self._assert_workspace_idle(db, agent, operation_id, snapshot=snapshot, resolved=resolved)
            except ValueError as error:
                self._settle_checkpoint(db, agent, operation_id, "Checkpoint skipped: " + str(error))
                return None
            operation.update(phase="capture_running", updated=time.time())
            self._put_workspace_operation(db, operation)
        error = None
        try:
            return self.capture_checkpoint(key, label, turn_id)
        except Exception as cause:
            error = cause
            raise
        finally:
            with self.lock, self.db() as db:
                self._settle_checkpoint(db, self.agent(key, db), operation_id, error)
            self.changed.set()

    def checkpoint_capture(
        self, agent_id, label="Checkpoint", turn_id=None, internal=False
    ):
        if self.agent(agent_id).get("imageWorkspace"):
            raise ValueError("Studio Git checkpoints are unavailable for image workspaces")
        if internal:
            return self.capture_checkpoint(agent_id, label, turn_id)
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)
            scope = (a["epoch"], self._workspace_source(a))
            snapshot = self._workspace_idle_snapshot(db, a)
        resolved = self._resolve_workspace_idle(snapshot, a)
        with self.lock, self.db() as db:
            a = self.checked_actor(db, agent_id)
            if (a["epoch"], self._workspace_source(a)) != scope:
                raise ValueError("Workspace source changed before checkpoint capture")
            if self._workspace_idle_snapshot(db, a) != snapshot:
                raise ValueError("Workspace activity changed before checkpoint capture")
            self._assert_workspace_idle(db, a, snapshot=snapshot, resolved=resolved)
            operation_id = self._reserve_checkpoint(db, a, "capture", turn_id)
        return self._capture_reserved_checkpoint(agent_id, label, turn_id, operation_id)

    def capture_checkpoint(self: "WorkspaceRuntime", agent_id: str, label: str="Checkpoint", turn_id: str | None=None, tree: str | None=None) -> CheckpointRecord:
        a = self.checked_actor_in_own_db(agent_id)
        if a.get("imageWorkspace"):
            raise ValueError("Studio Git checkpoints are unavailable for image workspaces")
        tree = tree if tree is not None else self.snapshot_tree(a)
        key = str(uuid.uuid4())
        env = {
            **os.environ,
            "GIT_AUTHOR_NAME": "Codex Agents",
            "GIT_AUTHOR_EMAIL": "local@codex-agents.invalid",
            "GIT_COMMITTER_NAME": "Codex Agents",
            "GIT_COMMITTER_EMAIL": "local@codex-agents.invalid",
        }
        commit = (
            self.git(
                a,
                ["commit-tree", tree, "-p", "HEAD"],
                env,
                b"Codex Agents workspace checkpoint\n",
            )
            .decode()
            .strip()
        )
        ref = f"refs/codex-agents/checkpoints/{agent_id}/{key}"
        self.git(a, ["update-ref", ref, commit])
        with self.lock, self.db() as db:
            current = self.agent(agent_id, db)
            parent_id = current.get("checkpointHistoryHead")
            parent_boundary = None
            if parent_id:
                parent_row = db.execute(
                    "SELECT record FROM runtime_checkpoints WHERE id=?", (parent_id,)
                ).fetchone()
                if parent_row:
                    from codex_payloads import resolve_record
                    parent_boundary = resolve_record(self.root, json.loads(parent_row[0])).get("historyBoundary")
            boundary = db.execute(
                "SELECT rowid,id FROM runtime_items WHERE agent=? "
                "AND json_extract(record,'$.afterRestore') IS NULL "
                "ORDER BY rowid DESC LIMIT 1",
                (agent_id,),
            ).fetchone()
            query = (
                "SELECT id FROM runtime_items WHERE agent=? "
                "AND json_extract(record,'$.afterRestore') IS NULL"
            )
            params = [agent_id]
            if parent_boundary:
                query += " AND rowid>?"
                params.append(parent_boundary)
            record = {
                "historyParent": parent_id,
                "historyDelta": [row[0] for row in db.execute(query, params)],
                "historyBoundary": boundary[0] if boundary else None,
                "id": key,
                "agent": agent_id,
                "rootId": a["rootId"],
                "label": label,
                "tree": tree,
                "commit": commit,
                "ref": ref,
                "threadId": a.get("threadId"),
                "turnId": turn_id or a.get("lastCompletedTurn"),
                "created": time.time(),
                "cwd": a["cwd"],
            }
            self.put(db, "checkpoints", record)
            current["checkpointHistoryHead"] = key
            self.put(db, "agents", current)
            return record

    def _checkpoint_history_ids(self: "WorkspaceRuntime", db: sqlite3.Connection, checkpoint: CheckpointRecord) -> list[str]:
        ids = set()
        seen = set()
        current = checkpoint
        while current:
            checkpoint_id = current.get("id")
            if checkpoint_id in seen:
                raise ValueError("Checkpoint history lineage contains a cycle")
            seen.add(checkpoint_id)
            if "items" in current:
                ids.update(current["items"])
            elif "historyDelta" in current:
                ids.update(current["historyDelta"])
            else:
                ids.update(
                    row[0]
                    for row in db.execute(
                        "SELECT id FROM runtime_items WHERE agent=? AND created<=?",
                        (checkpoint["agent"], checkpoint.get("created", 0)),
                    )
                )
            parent_id = current.get("historyParent")
            if not parent_id:
                break
            row = db.execute(
                "SELECT record FROM runtime_checkpoints WHERE id=?", (parent_id,)
            ).fetchone()
            if not row:
                raise ValueError("Checkpoint history parent is missing")
            from codex_payloads import resolve_record
            current = resolve_record(self.root, json.loads(row[0]))
        return ids

    @staticmethod
    def checkpoint_summary(checkpoint: CheckpointRecord) -> JsonObject:
        return {k: v for k, v in checkpoint.items()
                if k not in {"items", "historyDelta"}}

    def checkpoint_after_turn(self, key, turn_id, operation_id):
        with self.lock, self.db() as db:
            agent = self.agent(key, db)
            if agent.get("imageWorkspace") or not agent.get("worktreeReady"):
                return
        try:
            if self._capture_reserved_checkpoint(key, "After turn", turn_id, operation_id) is None:
                return
        except Exception:
            # The capture helper persists the exact failure and releases only its
            # own reservation. Automatic capture must not fail the completed turn.
            return

    def checkpoint_preview(self: "WorkspaceRuntime", key: str, checkpoint_id: str) -> JsonObject:
        a = self.checked_actor_in_own_db(key)
        recovery_expected = None
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT json_remove(record,'$.items','$.historyDelta','$._payloadBlobs') "
                "FROM runtime_checkpoints WHERE id=?", (checkpoint_id,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown checkpoint")
            checkpoint = json.loads(row[0])
            if checkpoint["agent"] != key:
                raise ValueError("Checkpoint belongs to another agent")
            for operation in self._workspace_operations(db, key):
                if (
                    operation.get("kind") == "restore"
                    and operation.get("checkpoint") == checkpoint_id
                ):
                    recovery_expected = operation.get("expectedTree")
                    break
        tree = self.snapshot_tree(a)
        expected_tree = recovery_expected or tree
        patch = self.git(
            a, ["diff", "--no-ext-diff", "--no-color", expected_tree, checkpoint["tree"]]
        ).decode(errors="replace")
        can_restore = bool(a.get("worktreeReady") and not a.get("inFlight"))
        if recovery_expected is not None:
            can_restore = can_restore and tree in {recovery_expected, checkpoint["tree"]}
        return {
            "checkpoint": self.checkpoint_summary(checkpoint),
            "expectedTree": expected_tree,
            "diff": patch[:300000],
            "patch": patch[:300000],
            "truncated": len(patch) > 300000,
            "canRestore": can_restore,
        }

    def restore_checkpoint(self: "WorkspaceRuntime", key: str, data: JsonObject) -> JsonObject:
        with self.lock:
            guard = self.prepare_locks.setdefault(
                "restore:" + key, __import__("threading").Lock()
            )
        with guard:
            return self._restore_checkpoint_locked(key, data)

    def _restore_checkpoint_locked(self: "WorkspaceRuntime", key: str, data: JsonObject) -> JsonObject:
        operation_id = self._workspace_operation_id("restore", key, data)
        resume_operation = None
        files_already_restored = False
        completed = None
        idle_snapshot = None
        signature = self._workspace_restore_signature(key, data)
        checkpoint_id = data.get("checkpoint_id") or data.get("checkpoint")
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            if not a.get("worktreeReady"):
                raise ValueError("Restore is available only in an isolated worker worktree")
            active = self._workspace_operations(db, key)
            existing = self._workspace_operation(db, operation_id)
            if existing and existing.get("signature") != signature:
                raise ValueError("This restore request has different content")
            if existing and existing.get("phase") == "completed":
                row = db.execute(
                    "SELECT record FROM runtime_checkpoints WHERE id=?",
                    (existing.get("checkpoint"),),
                ).fetchone()
                if not row:
                    raise ValueError("Unknown checkpoint")
                from codex_payloads import resolve_record
                checkpoint = resolve_record(self.root, json.loads(row[0]))
                completed = (existing, checkpoint, a)
            elif active:
                matches = [op for op in active if op.get("id") == operation_id
                           and op.get("kind") == "restore"]
                if not matches:
                    raise ValueError("Workspace recovery is required before retrying this operation")
                resume_operation = matches[0]
                self._assert_workspace_source(resume_operation, a)
                if resume_operation.get("signature") != signature:
                    raise ValueError("This restore request has different content")
                row = db.execute(
                    "SELECT record FROM runtime_checkpoints WHERE id=?",
                    (resume_operation.get("checkpoint"),),
                ).fetchone()
                if not row:
                    raise ValueError("Unknown checkpoint")
                from codex_payloads import resolve_record
                checkpoint = resolve_record(self.root, json.loads(row[0]))
                expected_tree = resume_operation.get("expectedTree")
                if data.get("checkpoint_id") not in {None, checkpoint["id"]}:
                    raise ValueError("This restore request has different content")
                if data.get("expectedTree") != expected_tree:
                    raise ValueError("This restore request has different content")
                operation = resume_operation
            else:
                row = db.execute(
                    "SELECT record FROM runtime_checkpoints WHERE id=?", (checkpoint_id,)
                ).fetchone()
                if not row:
                    raise ValueError("Unknown checkpoint")
                from codex_payloads import resolve_record
                checkpoint = resolve_record(self.root, json.loads(row[0]))
                if checkpoint.get("agent") != key:
                    raise ValueError("Checkpoint belongs to another agent")
                expected_tree = data.get("expectedTree")
                if not isinstance(expected_tree, str) or not expected_tree:
                    raise ValueError("Preview this checkpoint before restore")
                operation = {
                    "id": operation_id, "kind": "restore", "agent": key,
                    "checkpoint": checkpoint["id"], "expectedTree": expected_tree,
                    "signature": signature, "source": self._workspace_source(a),
                    "phase": "provider_pending", "created": time.time(),
                }
                scope = (a["epoch"], self._workspace_source(a))
                idle_snapshot = self._workspace_idle_snapshot(db, a)
            preview = {"checkpoint": checkpoint, "expectedTree": expected_tree if not completed else None}

        if idle_snapshot is not None:
            resolved = self._resolve_workspace_idle(idle_snapshot, a)
            with self.lock, self.db() as db:
                a = self.checked_actor(db, key)
                if (a["epoch"], self._workspace_source(a)) != scope or not a.get("worktreeReady"):
                    raise ValueError("Workspace source changed before restore")
                if (self._workspace_idle_snapshot(db, a) != idle_snapshot
                        or self._workspace_operation(db, operation_id) != existing):
                    raise ValueError("Workspace activity changed before restore")
                self._assert_workspace_idle(db, a, snapshot=idle_snapshot, resolved=resolved)
                self._put_workspace_operation(db, operation)
                a["workspaceOperation"] = "restore"
                self.put(db, "agents", a)

        # Git snapshots and diffs can take seconds. Never hold Runtime.lock here.
        if completed:
            completed_op, checkpoint, a = completed
            if self.snapshot_tree(a) != checkpoint["tree"]:
                raise ValueError("Files changed after the completed restore. Preview again")
            with self.lock, self.db() as db:
                current = self.agent(key, db)
                latest = self._workspace_operation(db, operation_id)
                if (not latest or latest.get("phase") != "completed"
                        or current.get("restoredCheckpoint") != checkpoint["id"]):
                    raise ValueError("Restore state changed. Inspect the operation before retrying")
                return latest.get("result") or {"status": "restored", "checkpoint": checkpoint_id}
        if resume_operation:
            current_tree = self.snapshot_tree(a)
            if current_tree == checkpoint["tree"]:
                files_already_restored = True
            elif current_tree != preview["expectedTree"]:
                raise ValueError(
                    "Workspace recovery is required. Restore files to the saved checkpoint or preview tree before retrying"
                )
        else:
            try:
                if self.snapshot_tree(a) != preview["expectedTree"]:
                    raise ValueError("Files changed after the preview. Preview again")
            except Exception as error:
                self._finish_workspace_operation(operation_id, key, error=error)
                raise
        try:
            # Create the matching conversation before changing files. A provider rejection leaves files intact.
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            if resume_operation:
                response = resume_operation.get("provider")
                if not response:
                    raise ValueError(
                        "Workspace recovery is required because the native result is unknown"
                    )
            else:
                try:
                    if checkpoint.get("turnId"):
                        response = self.connect_agent(a).call(
                            "thread/fork",
                            {
                                "threadId": checkpoint["threadId"],
                                "lastTurnId": checkpoint["turnId"],
                                "cwd": a["cwd"],
                                "config": self.thread_config(),
                                **{k: v for k, v in self.new_thread_params(a).items() if k in {"approvalPolicy", "sandbox"}},
                                "model": a["model"],
                            },
                        )
                    else:
                        response = self.connect_agent(a).call(
                            "thread/start", self.new_thread_params(a)
                        )
                except Exception as error:
                    if self._workspace_provider_rejected(error):
                        self._finish_workspace_operation(operation_id, key, error=error)
                    else:
                        self._require_workspace_recovery(operation_id, key, error)
                    raise
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            response = self._workspace_provider_result(response)
            try:
                if not resume_operation:
                    self._update_workspace_operation(
                        operation_id,
                        phase="provider_ready",
                        provider=response,
                    )
                with self.lock, self.db() as db:
                    self._assert_workspace_source(operation, self.agent(key, db))
                current_tree = self.snapshot_tree(a)
                if current_tree != preview["expectedTree"] and not files_already_restored:
                    raise ValueError("Files changed while preparing restore. Preview again")
                # Mark the local phase before any Git mutation. A restart now must hold the workspace.
                if not files_already_restored:
                    self._update_workspace_operation(operation_id, phase="local_mutation")
                    # Only this isolated, idle worktree is changed; its old state remains a saved checkpoint.
                    if not resume_operation or not resume_operation.get("beforeCheckpoint"):
                        before = self.checkpoint_capture(key, "Before restore", internal=True)
                        self._update_workspace_operation(
                            operation_id,
                            phase="local_mutation",
                            beforeCheckpoint=before["id"],
                        )
                    # Record the preview tree in this isolated index so added files are removed on restore too.
                    self.git(a, ["read-tree", preview["expectedTree"]])
                    self.git(a, ["read-tree", "--reset", "-u", checkpoint["tree"]])
                if self.snapshot_tree(a) != checkpoint["tree"]:
                    raise ValueError(
                        "Workspace files changed before restore metadata was saved"
                    )
                with self.db() as history_db:
                    visible_ids = self._checkpoint_history_ids(history_db, checkpoint)
                with self.lock, self.db() as db:
                    current = self.agent(key, db)
                    self._assert_workspace_source(operation, current)
                    result = {"status": "restored", "checkpoint": checkpoint["id"]}
                    current.update(
                        threadId=response["thread"]["id"],
                        sandbox=response.get("sandbox"),
                        approvalPolicy=response.get("approvalPolicy"),
                        profile=response.get("activePermissionProfile"),
                        turnId=None,
                        inFlight=False,
                        status="paused",
                        autoWake=False,
                        workspaceOperation=None,
                        restoredCheckpoint=checkpoint["id"],
                        lastCompletedTurn=checkpoint.get("turnId"),
                    )
                    self.put(db, "agents", current)
                    operation = self._workspace_operation(db, operation_id)
                    operation.update(phase="completed", result=result, updated=time.time())
                    self._put_workspace_operation(db, operation)
                    self.permanent_worker_hold(
                        db, current, operation_id, "restore-completed-paused",
                        "Workspace restored. The worker is paused and needs a new instruction.",
                    )
                    db.execute(
                        "UPDATE runtime_events SET status='cancelled' WHERE agent=? AND status='pending'",
                        (key,),
                    )
                    current["checkpointHistoryHead"] = checkpoint["id"]
                    self.put(db, "agents", current)
                    for item_row in db.execute(
                        "SELECT id,record,created FROM runtime_items WHERE agent=?", (key,)
                    ).fetchall():
                        record = json.loads(item_row["record"])
                        visible = item_row["id"] in visible_ids
                        was_visible = "afterRestore" not in record
                        if visible == was_visible:
                            continue
                        if visible:
                            record.pop("afterRestore", None)
                            if not self.search_is_indexed(db, record["id"]):
                                self.index_item(
                                    db,
                                    record["id"],
                                    key,
                                    record.get("title", "message"),
                                    record.get("text", ""),
                                )
                        else:
                            record["afterRestore"] = checkpoint["id"]
                        db.execute(
                            "UPDATE runtime_items SET record=? WHERE id=?",
                            (json.dumps(record), item_row["id"]),
                        )
                    self.item(
                        db,
                        key,
                        "restore:" + checkpoint["id"],
                        "system",
                        "Restored workspace and conversation to " + checkpoint["label"],
                        "Checkpoint",
                    )
                self.loaded.discard(current["id"])
                return result
            except Exception as error:
                self._require_workspace_recovery(operation_id, key, error)
                raise
        except Exception as error:
            with self.lock, self.db() as db:
                operation = self._workspace_operation(db, operation_id)
            if not operation or operation.get("phase") not in {"failed", "completed"}:
                self._require_workspace_recovery(operation_id, key, error)
            raise

    def assert_workspace_idle(self: "WorkspaceRuntime", a: AgentRecord) -> None:
        with self.db() as db:
            self._assert_workspace_idle(db, a)

    def _workspace_idle_snapshot(self, db, a, *, current_state=False):
        agents = [tuple(row) for row in db.execute(
            "SELECT id,json_extract(record,'$.cwd'),json_extract(record,'$.inFlight'),"
            "json_extract(record,'$.workspaceOperation'),json_extract(record,'$.workspaceReservationId') "
            "FROM runtime_agents ORDER BY id")]
        if current_state:
            agents = [(a["id"], a["cwd"], a.get("inFlight"), a.get("workspaceOperation"),
                       a.get("workspaceReservationId")) if row[0] == a["id"] else row for row in agents]
        phases = tuple(sorted(self.WORKSPACE_OPERATION_ACTIVE))
        operations = [tuple(row) for row in db.execute(
            "SELECT id,json_extract(record,'$.agent'),json_extract(record,'$.phase') "
            "FROM runtime_workspace_operations WHERE json_extract(record,'$.phase') IN ("
            + ",".join("?" for _ in phases) + ") ORDER BY id", phases)]
        monitors = [tuple(row) for row in db.execute(
            "SELECT id,json_extract(record,'$.cwd'),json_extract(record,'$.status') "
            "FROM runtime_monitors WHERE json_extract(record,'$.status') IN (?,?,?) ORDER BY id",
            ACTIVE_MONITOR_STATUSES)]
        tasks = [tuple(row) for row in db.execute(
            "SELECT id,json_extract(record,'$.agent') FROM runtime_tasks "
            "WHERE json_extract(record,'$.status')='running' ORDER BY id")]
        return agents, operations, monitors, tasks

    def _resolve_workspace_idle(self, snapshot, a):
        agents, operations, monitors, tasks = snapshot
        busy_ids = {row[1] for row in operations} | {row[1] for row in tasks}
        paths = {a["cwd"]} | {row[1] for row in monitors}
        paths.update(row[1] for row in agents if row[2] or row[3] or row[0] in busy_ids)
        return {cwd: Path(cwd).resolve() for cwd in sorted(paths)}

    def _assert_workspace_idle(self, db, a, reservation_id=None, *, current_state=False,
                               snapshot=None, resolved=None):
        snapshot = snapshot if snapshot is not None else self._workspace_idle_snapshot(db, a, current_state=current_state)
        resolved = resolved if resolved is not None else self._resolve_workspace_idle(snapshot, a)
        agents, operations, monitors, tasks = snapshot
        cwd = resolved[a["cwd"]]
        by_id = {row[0]: row for row in agents}
        if any(operation[0] != reservation_id and operation[1] in by_id
               and resolved[by_id[operation[1]][1]] == cwd for operation in operations):
            raise ValueError("Workspace recovery is required before using this workspace")
        for key, path, in_flight, workspace_operation, reservation in agents:
            if not (in_flight or workspace_operation) or resolved[path] != cwd:
                continue
            own_reservation = reservation_id is not None and key == a["id"] and reservation == reservation_id
            if in_flight or (workspace_operation and not own_reservation):
                raise ValueError("An agent is using this workspace")
        if any(resolved[row[1]] == cwd for row in monitors):
            raise ValueError("A monitor is using this workspace")
        if any(row[1] in by_id and resolved[by_id[row[1]][1]] == cwd for row in tasks):
            raise ValueError("A command or tool is still active")

    def branch_conversation(self: "WorkspaceRuntime", key: str, data: JsonObject) -> JsonObject:
        with self.lock:
            guard = self.prepare_locks.setdefault(
                "fork:" + key, __import__("threading").Lock()
            )
        with guard:
            return self.branch_locked(key, data)

    def branch_locked(self: "WorkspaceRuntime", key: str, data: JsonObject) -> JsonObject:
        if "before" in data and type(data["before"]) is not bool:
            raise ValueError("before must be a boolean")
        operation_id = self._workspace_operation_id("branch", key, data)
        body = {"agent": key, **data}
        with self.lock, self.db() as db:
            a = self.checked_actor(db, key)
            signature, prior = self.operation_receipt(db, data.get("id"), body)
            if prior is not None:
                operation = self._workspace_operation(db, operation_id)
                if operation and operation.get("phase") != "completed":
                    operation.update(phase="completed", result=prior, updated=time.time())
                    self._put_workspace_operation(db, operation)
                    if a.get("workspaceOperation") in {"branch", "branch_recovery"}:
                        a["workspaceOperation"] = None
                        self.put(db, "agents", a)
                return prior
            if self.accounts.get(a.get("accountKey", "default")).get("disconnected"):
                raise ValueError("Reconnect this account before creating a branch")
            operation = self._workspace_operation(db, operation_id)
            resume_local = False
            retry_failed = False
            if operation:
                self._assert_workspace_source(operation, a)
                if operation.get("signature") != signature:
                    raise ValueError("This request id has different content")
                if operation.get("phase") == "completed":
                    return operation.get("result")
                if operation.get("phase") == "provider_ready":
                    resume_local = True
                elif operation.get("phase") == "failed":
                    retry_failed = True
                    operation.update(
                        phase="provider_pending",
                        provider=None,
                        result=None,
                        error=None,
                        updated=time.time(),
                    )
                    self._put_workspace_operation(db, operation)
                    a["workspaceOperation"] = "branch"
                    self.put(db, "agents", a)
                elif operation.get("phase") in self.WORKSPACE_OPERATION_ACTIVE:
                    raise ValueError(
                        "Workspace recovery is required before retrying this operation"
                    )
            from codex_transcript_history import resolve_item
            row = resolve_item(db, key, data.get("message_id"))
            item = json.loads(row["record"])
            turn_id = item.get("turnId")
            if not turn_id or item.get("afterRestore"):
                raise ValueError("This message has no active native turn reference")
            if not resume_local and not retry_failed:
                self.assert_workspace_available(db, a)
            if turn_id == a.get("turnId"):
                raise ValueError("Wait for this turn to finish before branching")
            if operation is None:
                operation = {
                    "id": operation_id,
                    "kind": "branch",
                    "agent": key,
                    "message_id": item["id"],
                    "turn_id": turn_id,
                    "signature": signature,
                    "phase": "provider_pending",
                    "leadId": str(uuid.uuid4()),
                    "created": time.time(),
                    "previousWorkspaceOperation": a.get("workspaceOperation"),
                    "previousStatus": a.get("status"),
                    "previousAutoWake": a.get("autoWake"),
                    "source": self._workspace_source(a),
                }
                self._put_workspace_operation(db, operation)
                a["workspaceOperation"] = "branch"
                self.put(db, "agents", a)
            lead_id = operation["leadId"]
        # A branch starts a new team, without the source team's exception.
        try:
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            if resume_local:
                with self.db() as db:
                    operation = self._workspace_operation(db, operation_id)
                response = operation["provider"]
            else:
                provider_dispatched = False
                try:
                    branch_params = self.new_thread_params({**a, "isLead": True,
                        "model": a["model"] if a.get("isLead") else "gpt-5.6-sol", "needsTitle": False})
                    fork_turn = turn_id
                    if data.get("before"):
                        native = self.connect_agent(a).call(
                            "thread/read", {"threadId": a["threadId"], "includeTurns": True})
                        turns = native.get("thread", {}).get("turns", [])
                        index = next((i for i, turn in enumerate(turns) if turn.get("id") == turn_id), None)
                        if index is None:
                            raise ValueError("The selected turn is absent from native history")
                        fork_turn = turns[index - 1]["id"] if index else None
                    self._update_workspace_operation(operation_id, forkTurnId=fork_turn)
                    provider_dispatched = True
                    if fork_turn is None:
                        response = self.connect_agent(a).call(
                            "thread/start", branch_params)
                    else:
                        response = self.connect_agent(a).call(
                            "thread/fork",
                            {
                                "threadId": a["threadId"],
                                "lastTurnId": fork_turn,
                                "cwd": a["cwd"],
                                "config": self.thread_config(),
                                **{k: v for k, v in branch_params.items() if k in {"approvalPolicy", "sandbox"}},
                                "model": a["model"] if a.get("isLead") else "gpt-5.6-sol",
                                "developerInstructions": branch_params[
                                    "developerInstructions"
                                ],
                            },
                        )
                except Exception as error:
                    if not provider_dispatched or self._workspace_provider_rejected(error):
                        self._finish_workspace_operation(operation_id, key, error=error)
                    else:
                        self._require_workspace_recovery(operation_id, key, error)
                    raise
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            try:
                response = self._workspace_provider_result(response)
                if response["thread"]["id"] == a.get("threadId"):
                    raise ValueError("The native branch did not create a new thread")
            except Exception as error:
                self._require_workspace_recovery(operation_id, key, error)
                raise
            if not resume_local:
                try:
                    self._update_workspace_operation(
                        operation_id,
                        phase="provider_ready",
                        provider=response,
                    )
                except Exception as error:
                    self._require_workspace_recovery(operation_id, key, error)
                    raise
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
            self.checked_actor_in_own_db(key)
            lead = self.create(
                {
                    "id": lead_id,
                    "name": a["name"][:90] + " (branch)",
                    "account_key": a.get("accountKey", "default"),
                    "cwd": a["cwd"],
                    "model": a["model"] if a.get("isLead") else "gpt-5.6-sol",
                    "prompt": "",
                },
                defer=True,
                draft=True,
                _validate_only=True,
                _accepted_provider_operation=True,
            )
            with self.lock, self.db() as db:
                self._assert_workspace_source(operation, self.agent(key, db))
                lead.update(
                    yoloMode=a.get("yoloMode"),
                    sandbox=response.get("sandbox", a.get("sandbox")),
                    approvalPolicy=response.get("approvalPolicy", a.get("approvalPolicy")),
                    profile=response.get("activePermissionProfile", a.get("profile")),
                    threadId=response["thread"]["id"],
                    forkedFrom=key,
                    sourceMessage=item["id"],
                    status="idle",
                    autoWake=True,
                    needsTitle=False,
                )
                self.put(db, "agents", lead)
                rows = db.execute(
                    "SELECT record,created FROM runtime_items WHERE agent=? AND json_extract(record,'$.afterRestore') IS NULL ORDER BY created",
                    (key,),
                ).fetchall()
                operation = self._workspace_operation(db, operation_id)
                fork_turn = operation.get("forkTurnId", turn_id)
                cutoff = max((r["created"] for r in rows
                    if json.loads(r["record"]).get("turnId") == fork_turn), default=float("-inf"))
                assets = {}

                def copy_assets(records):  # type: (list[JsonObject]) -> list[JsonObject]
                    copied = []
                    for view in records:
                        old_id = view["id"]
                        if old_id not in assets:
                            row = db.execute(
                                "SELECT record FROM runtime_assets WHERE id=?", (old_id,)
                            ).fetchone()
                            if not row:
                                raise ValueError("A source attachment is unavailable")
                            asset = json.loads(row[0])
                            asset.update(id=str(uuid.uuid4()), agent=lead["id"])
                            self.put(db, "assets", asset)
                            assets[old_id] = self.asset_view(asset)
                        copied.append(assets[old_id])
                    return copied

                for r in rows:
                    if r["created"] > cutoff:
                        break
                    record = json.loads(r["record"])
                    if data.get("before") and record.get("turnId") == turn_id:
                        continue
                    inputs = record.get("inputs")
                    if inputs is not None:
                        inputs = [
                            {**event, "assets": copy_assets(event.get("assets", []))}
                            for event in inputs
                        ]
                    self.item(
                        db,
                        lead["id"],
                        record["id"].split(":", 1)[-1],
                        record["role"],
                        record["text"],
                        record.get("title"),
                        inputs=inputs,
                        turnId=record.get("turnId"),
                        streaming=False,
                        assets=copy_assets(record.get("assets", [])),
                    )
                if data.get("before"):
                    prompt = item
                    if item.get("inputs"):
                        prompt = next((entry for index, entry in enumerate(item["inputs"])
                            if data.get("message_id") == (key + ":" + entry['id'] if entry.get('id') else item['id'] + ':' + str(index))), item["inputs"][0])
                    event = db.execute("SELECT text FROM runtime_events WHERE id=? AND agent=?", (prompt.get("id", "").removeprefix(key + ":"), key)).fetchone()
                    from codex_search_text import search_text
                    full = search_text(db, item["id"]) if not item.get("inputs") else None
                    preceding = []
                    prefix_assets = []
                    for entry in item.get("inputs", []):
                        if entry is prompt:
                            break
                        if entry.get("kind") != "user":
                            continue
                        prior_event = db.execute("SELECT text FROM runtime_events WHERE id=? AND agent=?", (entry.get("id"), key)).fetchone()
                        preceding.append(prior_event[0] if prior_event else entry.get("text", ""))
                        prefix_assets.extend(entry.get("assets", []))
                    lead = {**lead, "draft": {"text": event[0] if event else full if full else prompt.get("text", ""),
                        "prefixText": "\n\n".join(preceding),
                        "assets": copy_assets([*prefix_assets, *prompt.get("assets", [])])}}
                result = self.save_receipt(db, data.get("id"), signature, lead)
            self.loaded.discard(lead["id"])
            self._finish_workspace_operation(operation_id, key, result=result)
            return result
        except Exception as error:
            with self.lock, self.db() as db:
                operation = self._workspace_operation(db, operation_id)
            if operation and operation.get("phase") == "provider_ready":
                # Keep the known provider response. The exact retry may finish
                # the local transaction without creating another native branch.
                try:
                    self._update_workspace_operation(
                        operation_id,
                        phase="provider_ready",
                        localError=str(error),
                    )
                except Exception:
                    self._require_workspace_recovery(operation_id, key, error)
            raise

    def profiles(self: "WorkspaceRuntime", data: JsonObject | None = None) -> Any:  # typed-suspect: shadows EfficiencyMixin.profiles with another response shape
        with self.lock, self.db() as db:
            if data is None:
                return {"profiles": self.records(db, "profiles")}
            key = data.get("id") or str(uuid.uuid4())
            if data.get("action") == "delete":
                db.execute("DELETE FROM runtime_profiles WHERE id=?", (key,))
                return {"deleted": key}
            role = data.get("role", "reviewer")
            if role not in {"reviewer", "implementer"}:
                raise ValueError("Choose reviewer or implementer")
            profile = {
                "id": key,
                "name": text_field(data.get("name"), "a profile name", 100),
                "role": role,
                "model": text_field(data.get("model"), "a model", 100),
                "effort": data.get("effort") or None,
                "instructions": text_field(
                    data.get("instructions", ""), "instructions", 16000, empty=True
                ),
            }
            if profile["effort"] not in {
                None,
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultra",
            }:
                raise ValueError("Unknown reasoning effort")
            self.put(db, "profiles", profile)
            return profile

    def skill_catalog(self: "WorkspaceRuntime", key: str) -> JsonObject:
        """Read only the selected account/project's native skill inventory."""
        a = self.checked_actor_in_own_db(key)
        result: JsonObject = {"skills": [], "errors": []}
        try:
            response = self.connect_agent(a).call(
                "skills/list", {"cwds": [a["cwd"]], "forceReload": False},
                timeout=SKILL_CATALOG_TIMEOUT_SECONDS,
            )
            groups = response.get("data")
            if not isinstance(groups, list):
                raise ValueError("The provider did not return a skill catalog")
            seen = set()
            for group in groups:
                if not isinstance(group, dict) or group.get("cwd") != a["cwd"]:
                    continue
                for error in group.get("errors", []):
                    message = error.get("message") if isinstance(error, dict) else error
                    if isinstance(message, str) and message:
                        result["errors"].append(message)
                for skill in group.get("skills", []):
                    if not isinstance(skill, dict) or skill.get("enabled") is False:
                        continue
                    name, path = skill.get("name"), skill.get("path")
                    if not isinstance(name, str) or not name or not isinstance(path, str) or not path:
                        continue
                    # Preserve native precedence when multiple roots contain a name.
                    if name in seen:
                        continue
                    seen.add(name)
                    description = skill.get("description")
                    result["skills"].append({
                        "name": name, "path": path,
                        "description": description if isinstance(description, str) else "",
                    })
            result["skills"].sort(key=lambda skill: skill["name"].casefold())
        except Exception as error:
            result["errors"].append(str(error))
        return result

    def capabilities(self: "WorkspaceRuntime", key: str) -> JsonObject:
        a = self.checked_actor_in_own_db(key)
        cache = self.capability_cache.get(key)
        if cache and time.time() - cache["at"] < 30:
            return cache
        result = {
            "agent": key,
            "at": time.time(),
            "managed": self.tool_definitions(),
            "observed": [],
            "skills": [],
            "servers": [],
            "errors": [],
            "model": a["model"],
            "effort": a.get("effort"),
            "role": a["role"],
            "nativeInventory": "Codex selects native tools per model and configuration. Observed calls appear below.",
        }
        # The history index contains the agent identity. Filter it before
        # reading names from selected records; never decode full task history.
        name = """CASE WHEN json_type(record,'$.name') IS NOT NULL
                    THEN json_extract(record,'$.name')
                    WHEN json_type(record,'$.type') IS NOT NULL
                    THEN json_extract(record,'$.type') ELSE '' END"""
        with self.db() as db:
            rows = db.execute(
                f"""SELECT {name} FROM runtime_tasks INDEXED BY runtime_task_history
                    WHERE json_extract(record,'$.status')!='running'
                    AND json_extract(record,'$.agent')=?
                    UNION SELECT {name} FROM runtime_tasks INDEXED BY runtime_task_status
                    WHERE (json_extract(record,'$.status')='running'
                        OR json_extract(record,'$.status') IS NULL)
                    AND json_extract(record,'$.agent')=?""", (key, key))
            result["observed"] = sorted(row[0] for row in rows)
        from concurrent.futures import ThreadPoolExecutor
        reads = [
            ("skills", "skills/list", {"cwds": [a["cwd"]], "forceReload": True}),
            ("servers", "mcpServerStatus/list", {"threadId": a.get("threadId"), "limit": 100}),
        ]
        def discover(method, params):
            return self.connect_agent(a).call(method, params, timeout=5)
        # Independent provider reads share a five-second wait instead of two
        # sequential waits that can exceed the client's request deadline.
        with ThreadPoolExecutor(max_workers=2) as executor:
            pending = [(label, executor.submit(discover, method, params)) for label, method, params in reads]
            for label, future in pending:
                try:
                    response = future.result()
                    result[label] = response.get("data", response.get("servers", []))
                    result[label + "Cursor"] = response.get("nextCursor")
                except Exception as error:
                    result["errors"].append(label + ": " + str(error))
        result.update(observedNative=result["observed"], mcp=result["servers"])
        self.capability_cache[key] = result
        return result

    def workspace_snapshot(self: "WorkspaceRuntime", key: str=None, *, view: str="full") -> JsonObject:
        if view not in {"full", "inbox"}:
            raise ValueError("Unknown workspace view")
        with self.read_db() as db:
            root = self.checked_actor(db, key)["rootId"] if key else None
            if root is None:
                agents = [a for a in self.records(db, "agents", shared=True)
                          if not a.get("deletedAt")]
            else:
                agents = [json.loads(row[0]) for row in db.execute(
                    "SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId')=? "
                    "AND json_extract(record,'$.deletedAt') IS NULL", (root,))]
            ids = {a["id"] for a in agents}
            monitors = self.recent_monitors(db, root)
            inbox = []
            for r in self._workspace_requests(db, ids):
                if r["status"] == "pending" and not r.get("deferred"):
                    inbox.append(
                        {
                            "id": r["id"],
                            "kind": "request",
                            "agent": r["agent"],
                            "title": "Answer required",
                            "text": r["method"],
                            "request": r,
                        }
                    )
            for c in self._workspace_complaints(db, ids if root is not None else None):
                if c.get("needsResponse"):
                    inbox.append(
                        {
                            "id": c["id"],
                            "kind": "complaint",
                            "agent": c["leadId"],
                            "title": c["title"],
                            "text": "The lead must read and respond",
                            "complaint": c,
                        }
                    )
            works = self._workspace_work(db, root)
            for w in works:
                if w["status"] == "review":
                    inbox.append(
                        {
                            "id": w["id"],
                            "kind": "work",
                            "agent": w["rootId"],
                            "title": w["title"],
                            "text": "Result awaits acceptance",
                            "work": w,
                        }
                    )
            for a in agents:
                if a["status"] in {"failed", "interrupted"}:
                    inbox.append(
                        {
                            "id": a["id"],
                            "kind": "agent",
                            "agent": a["id"],
                            "title": a["name"],
                            "text": str(a.get("error") or a["status"]),
                        }
                    )
            for m in monitors:
                if (
                    m["agent"] in ids
                    and m["status"] in {"failed", "lost"}
                    and not m.get("ruleId")
                ):
                    inbox.append(
                        {
                            "id": m["id"],
                            "kind": "monitor",
                            "agent": m["agent"],
                            "title": m["command"],
                            "text": m.get("error") or f"Exit {m.get('exitCode')}",
                            "monitor": m,
                        }
                    )
            for r in self._workspace_rules(db, ids):
                if r.get("error"):
                    inbox.append(
                        {
                            "id": r["id"],
                            "kind": "rule",
                            "agent": r["agent"],
                            "title": r["name"],
                            "text": r["error"],
                        }
                    )
            if view == "inbox":
                return {"inbox": inbox}
            return {
                "work": [
                    self.work_view(w, works)
                    for w in works
                    if w["rootId"] in ids and (not root or w["rootId"] == root)
                ],
                "annotations": self._workspace_records(db, "annotations", ids, "agent"),
                "checkpoints": self._workspace_records(db, "checkpoints", {key} if key else ids, "agent"),
                "plans": [v for v in self._workspace_records(db, "plans", ids, "id")
                          if not root or v["rootId"] == root],
                "rules": self._workspace_rules(db, ids),
                "inbox": inbox,
                "tasks": self.recent_tasks(db, root),
                "tasksHistoryLimit": 100,
                "monitors": [m for m in monitors if m["agent"] in ids],
            }

    def _workspace_records(self: "WorkspaceRuntime", db: sqlite3.Connection, table: str, ids: Iterable[str], field: str) -> list[JsonObject]:
        if not ids:
            return []
        placeholders = ",".join("?" for _ in ids)
        if field == "id":
            clause = "id IN (" + placeholders + ")"
        else:
            clause = f"json_extract(record,'$.{field}') IN ({placeholders})"
        field_sql = ("json_remove(record,'$.items','$.historyDelta','$._payloadBlobs')"
                     if table == "checkpoints" else "record")
        return [json.loads(row[0]) for row in db.execute(
            f"SELECT {field_sql} FROM runtime_{table} WHERE {clause}", tuple(ids))]

    @staticmethod
    def _workspace_requests(db: sqlite3.Connection, ids: Iterable[str]) -> list[JsonObject]:
        if not ids:
            return []
        placeholders = ",".join("?" for _ in ids)
        return [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_requests WHERE json_extract(record,'$.agent') IN (" + placeholders + ") "
            "AND json_extract(record,'$.status')='pending' "
            "AND (json_extract(record,'$.deferred') IS NULL OR json_extract(record,'$.deferred')=0 "
            "OR json_extract(record,'$.deferred')='')", tuple(ids))]

    @staticmethod
    def _workspace_work(db: sqlite3.Connection, root: Path) -> list[JsonObject]:
        if root is None:
            return [json.loads(row[0]) for row in db.execute("SELECT record FROM runtime_work")]
        return [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_work WHERE json_extract(record,'$.rootId')=?", (root,))]

    @staticmethod
    def _workspace_rules(db: sqlite3.Connection, ids: Iterable[str]) -> list[JsonObject]:
        if not ids:
            return []
        placeholders = ",".join("?" for _ in ids)
        return [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_rules WHERE json_extract(record,'$.agent') IN (" + placeholders + ")",
            tuple(ids))]

    def _workspace_complaints(self: "WorkspaceRuntime", db: sqlite3.Connection, lead_ids: Iterable[str] | None) -> list[JsonObject]:
        if lead_ids is None:
            complaints = self.records(db, "complaints")
        elif not lead_ids:
            complaints = []
        else:
            placeholders = ",".join("?" for _ in lead_ids)
            complaints = [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_complaints WHERE json_extract(record,'$.leadId') IN (" + placeholders + ")",
                tuple(lead_ids))]
        ids = sorted({key for c in complaints for key in (c["author"], c["leadId"])})
        agents = ({a["id"]: a for a in (json.loads(r[0]) for r in db.execute(
            "SELECT record FROM runtime_agents WHERE id IN (" + ",".join("?" * len(ids)) + ")", ids))}
            if ids else {})
        result = []
        for c in complaints:
            result.append({**{k: c[k] for k in ("id", "leadId", "author", "status", "created", "updated", "readAt")},
                           "title": c["text"][:140], "text": c["text"], "responses": c["responses"],
                           "recipient": self.complaint_recipient(c), "version": c["version"],
                           "needsResponse": self.complaint_needs_response(c),
                           "authorName": agents.get(c["author"], {}).get("name", "You" if c["author"] == "user" else c["author"]),
                           "leadName": agents.get(c["leadId"], {}).get("name", c["leadId"]),
                           "leadStopped": not agents.get(c["leadId"], {}).get("autoWake", False),
                           "leadDeleted": bool(agents.get(c["leadId"], {}).get("deletedAt"))})
        return sorted(result, key=lambda c: (not c["needsResponse"], -c["updated"]))

    def monitor_log(self: "WorkspaceRuntime", key: str) -> JsonObject:
        with self.lock, self.db() as db:
            row = db.execute(
                "SELECT record FROM runtime_monitors WHERE id=?", (key,)
            ).fetchone()
            if not row:
                raise ValueError("Unknown monitor")
            m = json.loads(row[0])
            self.checked_actor(db, m["agent"])
            path = Path(m["log"])
            root = (self.root / "monitor-logs").resolve()
            path = path.resolve()
            if not path.is_relative_to(root):
                raise ValueError("Invalid log path")
            try:
                size = path.stat().st_size
                fallback = None
            except FileNotFoundError:
                size = 0
                fallback = m.get("tail", "").encode()
            return {
                "path": path,
                "fallback": fallback,
                "size": size,
                "name": key + ".log",
                "mime": "text/plain",
                "truncated": m.get("bytes", 0) > (size if fallback is None else len(fallback)),
            }

    def native_command_action(self: "WorkspaceRuntime", data: JsonObject) -> JsonObject:
        task = self.task_detail(data.get("id"))
        if (
            task.get("kind") != "command"
            or task.get("status") != "running"
            or not task.get("processId")
        ):
            raise ValueError("This native command has no active session")
        if data.get("action") not in {"input", "cancel"}:
            raise ValueError("Choose input or cancel")
        a = self.agent(task["agent"])
        if data["action"] == "cancel":
            server = self.connect_agent(a)
            params = {"threadId": a.get("threadId"), "limit": 100}
            # A process number alone can belong to another thread after recovery.
            while True:
                page = server.call("thread/backgroundTerminals/list", params)
                if any(t["itemId"] == task["itemId"] and t["processId"] == str(task["processId"])
                       for t in page["data"]):
                    break
                if not page.get("nextCursor"):
                    raise ValueError("This command no longer has an active native session")
                params["cursor"] = page["nextCursor"]
            result = server.call("thread/backgroundTerminals/terminate", {
                "threadId": params["threadId"], "processId": str(task["processId"])})
            if not result["terminated"]:
                raise ValueError("Codex did not confirm that the command stopped")
            return result
        text = data.get("text")
        text_field(text, "command input", 16000, empty=True)
        instruction = (
            f"Use your native write_stdin tool for session {task['processId']}. "
            f"Send exactly this JSON string as chars: {json.dumps(text)}."
        )
        return self.send(a["id"], instruction)

    def workspace_blockers(self: "WorkspaceRuntime", db: sqlite3.Connection, a: AgentRecord) -> list[dict[str, object]]:
        cwd = Path(a["cwd"]).resolve()
        blockers = []
        # Called under the runtime lock on every start. Decoding every agent
        # record took about a second on a live workspace and stalled callbacks.
        for (raw,) in db.execute("SELECT record FROM runtime_agents "
                                 "WHERE json_extract(record,'$.workspaceOperation') IS NOT NULL"):
            other = json.loads(raw)
            if not other.get("workspaceOperation") or Path(other["cwd"]).resolve() != cwd:
                continue
            blocker = {"agentId": other["id"], "operation": other["workspaceOperation"]}
            operation_id = other.get("workspaceReservationId")
            operation = self._workspace_operation(db, operation_id) if operation_id else None
            if not operation and other["workspaceOperation"] not in {"checkpoint", "capture"}:
                operation = next(iter(self._workspace_operations(db, other["id"])), None)
            if operation:
                blocker.update(operationId=operation["id"], phase=operation["phase"],
                               created=operation.get("created"), turnId=operation.get("turnId"))
            blockers.append(blocker)
        return blockers

    def assert_workspace_available(self: "WorkspaceRuntime", db: sqlite3.Connection, a: AgentRecord) -> None:
        from codex_context_repair import assert_context_available
        assert_context_available(a)
        from codex_native_tools import account_reserved
        if account_reserved(self, a.get("accountKey", "default")):  # type: ignore[arg-type]  # typed-narrowing: workspace host protocol supplies the account reservation interface
            raise ValueError("Wait for the account tool catalog update")
        if safety_retry_active(a):
            raise ValueError('Wait for the model change before another workspace operation')
        if a.get("accountTransferId") and not a.get("inFlight"):
            raise ValueError("Wait for this agent's account transfer to finish")
        blockers = self.workspace_blockers(db, a)
        if blockers:
            from codex_workspace_delivery import WorkspaceBusyError
            raise WorkspaceBusyError(blockers)

    def recent_tasks(self: "WorkspaceRuntime", db: sqlite3.Connection, root: str | None = None) -> list[JsonObject]:
        from codex_sync_entities import project
        return [project("task", record) for record in task_records(db, root)]

    def recent_monitors(self: "WorkspaceRuntime", db: sqlite3.Connection, root: str | None = None) -> list[JsonObject]:
        from codex_sync_entities import project
        return [project("monitor", record) for record in monitor_records(db, root)]
