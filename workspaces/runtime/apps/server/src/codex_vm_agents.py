"""Per-chat VM execution and layr lifecycle. Native chats keep their old path."""
from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path, PurePosixPath
import platform
import subprocess
import sys
import time
from typing import Any, Mapping


def workspace_mode(data: Mapping[str, Any], root: Mapping[str, Any] | None = None,
                   *, creation: bool = False) -> str:
    if root is not None:
        if "workspaceMode" in data:
            raise ValueError("Only chat creation selects the workspace mode")
        return str(data.get("_workspaceMode") or root.get("workspaceMode") or "worktree")
    default = "layr" if creation and platform.system() == "Darwin" else "worktree"
    mode = data.get("workspaceMode", default)
    if mode not in {"layr", "image", "worktree"}:
        raise ValueError('workspaceMode must be "layr", "image", or "worktree"')
    if mode != "worktree" and platform.system() != "Darwin":
        raise ValueError("This server supports only worktree chats")
    return str(mode)


def is_vm(agent: Mapping[str, Any]) -> bool:
    return agent.get("executionMode") == "vm"


def project_id(agent: Mapping[str, Any]) -> str:
    if agent.get("layrProjectId"):
        return str(agent["layrProjectId"])
    source = str(agent.get("projectId") or agent.get("hostProjectPath") or agent["cwd"])
    return hashlib.sha256(source.encode()).hexdigest()


def project_layr_id(project: Mapping[str, Any]) -> str:
    """The layr project ID of a new layr chat in this sidebar project. A new chat binds the
    logical project, and quick creation stores it as the chat's projectId: the project's
    own ID, or for a destination registration the home project in projectAliases[0], the
    identity that mergedSidebar groups. An unbound chat uses its folder; a folder project
    has that folder as its ID."""
    aliases = project.get("projectAliases") or [None]
    logical = aliases[0].get("projectId") if isinstance(aliases[0], dict) else None
    return project_id({"projectId": logical or project.get("id"), "cwd": project.get("path")})


def _local_folder(runtime: Any, project: Mapping[str, Any]) -> bool:
    """Only a project with a folder on this server can start a layr chat here."""
    locations = project.get("locations")
    path = str(project.get("path") or "")
    if isinstance(locations, list) and ("_project_server_id" in runtime.__dict__
                                        or "_cross_server_service" in runtime.__dict__):
        from codex_project_locations import local_server
        server = local_server(runtime)
        return any(isinstance(row, dict) and row.get("serverId") == server for row in locations)
    return bool(path) and not path.startswith("project:")


def project_readiness(runtime: Any, project: Mapping[str, Any]) -> dict[str, str] | None:
    """Sidebar readiness for a layr chat: host records only, no VM call. None on a server
    without layr chats (Linux) or for a project without a folder on this server."""
    if platform.system() != "Darwin" or not _local_folder(runtime, project):
        return None
    from codex_linux_vm_share import import_readiness
    from codex_linux_workspaces import client
    try:
        state_dir = getattr(client(runtime), "state_dir", None)
    except (RuntimeError, OSError, ValueError):
        return None  # Without a VM client no layr chat starts; /api/desktop reports the error.
    if not state_dir:
        return None
    layr_id = project_layr_id(project)
    readiness = import_readiness(Path(state_dir), layr_id)
    # A ready project names its layr project: the layr menu acts on it (project secrets).
    return {**readiness, "projectId": layr_id} if readiness["state"] == "ready" else readiness


def project_sync(runtime: Any, project: Mapping[str, Any]) -> dict[str, Any] | None:
    """The sidebar's Mac folder sync report of an imported project; host records only."""
    from codex_linux_workspaces import client
    from codex_vm_mac_sync import sync_status
    try:
        state_dir = getattr(client(runtime), "state_dir", None)
    except (RuntimeError, OSError, ValueError):
        return None
    if not state_dir:
        return None
    layr_id = project_layr_id(project)
    try:
        status = sync_status(Path(state_dir), layr_id)
    except (OSError, ValueError, TypeError) as error:
        return {"projectId": layr_id, "state": "error", "enabled": True, "conflicts": [], "choices": [],
                "withAgent": [], "deletes": 0, "reason": "The sync records are unreadable: " + str(error)[:300]}
    if status is None:
        return None
    # The time of the last round stays out: an unchanged view keeps its sync version.
    return {"projectId": layr_id, **{key: status[key] for key in (
        "state", "enabled", "conflicts", "choices", "withAgent", "deletes", "reason")}}


def project_vault(runtime: Any, project: Mapping[str, Any]) -> dict[str, Any] | None:
    """The sidebar's vault report of an imported project; host records only."""
    from codex_linux_workspaces import client
    from codex_vm_vault import vault_view
    try:
        state_dir = getattr(client(runtime), "state_dir", None)
    except (RuntimeError, OSError, ValueError):
        return None
    return vault_view(Path(state_dir), project_layr_id(project)) if state_dir else None


def has_layr_chats(runtime: Any, project: Mapping[str, Any]) -> bool:
    """Whether a chat of this project runs in the VM on this server. layr works for a project
    only through such chats, so without one the sidebar shows no LAYR chip."""
    layr_id = project_layr_id(project)
    # A new layr chat has its workspace mode before its first preparation sets the VM fields.
    with runtime.read_db() as db:
        row = db.execute(
            "SELECT 1 FROM runtime_agents WHERE (json_extract(record,'$.executionMode')='vm' "
            "OR json_extract(record,'$.workspaceMode')='layr') AND json_extract(record,'$.deletedAt') IS NULL "
            "AND (json_extract(record,'$.layrProjectId')=? OR json_extract(record,'$.projectId')=? "
            "OR json_extract(record,'$.hostProjectPath')=? OR json_extract(record,'$.cwd')=? "
            "OR json_extract(record,'$.cwd') LIKE ?) LIMIT 1",
            (layr_id, project.get("id"), project.get("path"), project.get("path"),
             "%/layr/projects/" + layr_id + "/%")).fetchone()
    return row is not None


def project_entity_view(runtime: Any, project: Mapping[str, Any]) -> dict[str, Any]:
    readiness = project_readiness(runtime, project)
    if not readiness or (readiness["state"] == "ready" and not has_layr_chats(runtime, project)):
        return dict(project)
    view = {**project, "layrReadiness": readiness}
    if readiness["state"] == "ready":
        sync = project_sync(runtime, project)
        if sync is not None:
            view["layrSync"] = sync
        vault = project_vault(runtime, project)
        if vault is not None:
            view["vault"] = vault
    return view


def refresh_readiness(runtime: Any) -> None:
    """Re-project the project entities after an import starts or ends, and after a restart.
    An unchanged view keeps its sync version. A failure here never stops a chat."""
    if platform.system() != "Darwin":
        return
    from codex_sync_entities import put as sync_entity_put
    try:
        with runtime.lock, runtime.db() as db:
            for row in db.execute("SELECT record FROM runtime_projects").fetchall():
                record = json.loads(row[0])
                sync_entity_put(db, "project", str(record["id"]), project_entity_view(runtime, record))
    except Exception as error:
        print("layr readiness refresh skipped: " + type(error).__name__ + ": " + str(error)[:300], file=sys.stderr)


def finder_view(runtime: Any, agent: Mapping[str, Any]) -> dict[str, Any]:
    """Where Finder shows a chat's files. A VM chat has the read-only ~/Studio view of its
    layr project. Reads the saved mount records and the mount table; never mounts or calls the VM."""
    if not is_vm(agent):
        return {"kind": "native", "path": agent.get("cwd")}
    view: dict[str, Any] = {"kind": "vm", "state": "unmounted", "path": None, "error": None}
    if not agent.get("layrProjectId"):
        return view
    from codex_linux_vm_share import mount_status
    from codex_linux_workspaces import client
    try:
        status = mount_status(client(runtime).state_dir)
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        return {**view, "state": "failed", "error": "The Finder view status is unavailable: " + str(error)[:300]}
    row = next((row for row in status["projects"] if row.get("projectId") == agent["layrProjectId"]), None)
    if row is None:
        return view
    if row["state"] == "mounted":
        return {**view, "state": "mounted", "path": row["path"]}
    return {**view, "state": row["state"], "error": row.get("error")}


def request(runtime: Any, method: str, params: dict[str, Any], identity: str,
            timeout: int = 130) -> dict[str, Any]:
    from codex_linux_workspaces import client
    result = client(runtime).request(method, params, request_id=identity, timeout=timeout)
    if not isinstance(result, dict):
        raise ValueError("The layr response is invalid")
    return result


def prepare(runtime: Any, agent: Mapping[str, Any]) -> Any:
    """Persist the association before the provider can submit a thread or turn."""
    from codex_linux_workspaces import client
    key = agent["id"]
    if not agent.get("layrReady"):
        if agent.get("isLead"):
            try:
                # The sidebar shows "importing" while this attempt holds the project lock.
                result = client(runtime).ensure_layr_project(
                    project_id(agent), str(agent.get("hostProjectPath") or agent["cwd"]),
                    owner=None, request_id="layr-project:" + project_id(agent),
                    started=lambda: refresh_readiness(runtime))
            finally:
                refresh_readiness(runtime)
            result = request(runtime, "line.bind", {
                "projectId": result["projectId"], "agentId": key, "line": "main",
            }, "layr-bind:" + key)
        else:
            parent = runtime.agent(agent["parentId"])
            if not parent.get("layrReady"):
                raise ValueError("The parent layr line is not ready")
            result = request(runtime, "line.branch", {
                "projectId": parent["layrProjectId"], "agentId": key,
                "parentAgentId": parent["id"], "readOnly": agent.get("role") == "reviewer",
                "subpath": agent.get("layrSubpath", "."),
                **({"stateId": agent["layrBaseState"]} if agent.get("layrBaseState") else {}),
            }, "layr-branch:" + key)
        with runtime.lock, runtime.db() as db:
            current = runtime.agent(key, db)
            if current.get("deletedAt"):
                raise ValueError("The chat was removed during layr preparation")
            current.update(cwd=result["cwd"], environment="linux", worktree=False,
                           layrReady=True, layrProjectId=result["projectId"],
                           layrLine=result["line"], layrOwner=result["owner"],
                           layrHome=result["home"], layrStateId=result["stateId"])
            runtime.put(db, "agents", current)
            agent = current
    flush_turns(runtime, agent)
    return runtime.agent(key)


def context(runtime: Any, agent: Mapping[str, Any]) -> dict[str, Any]:
    return request(runtime, "agent.context", {"agentId": agent["id"]},
                   "layr-context:" + agent["id"], 20)


def ensure(runtime: Any, agent: Mapping[str, Any]) -> dict[str, Any]:
    result = context(runtime, agent)
    if result["projectId"] != agent["layrProjectId"] or result["line"] != agent["layrLine"]:
        raise ValueError("The guest layr association changed")
    return result


def queue_turn(runtime: Any, db: Any, agent: Any, turn_id: str | None) -> None:
    if not turn_id or not is_vm(agent) or not agent.get("layrReady"):
        return
    pending = agent.setdefault("layrTurnSaves", {})
    pending.setdefault(turn_id, {"status": "pending"})


def _save_turn(runtime: Any, agent: Mapping[str, Any], turn: str, key: str) -> str:
    """Save the line state after one turn; its state ID."""
    from codex_linux_vm import LinuxVMError
    try:
        return str(request(runtime, "line.save", {"agentId": agent["id"], "turnId": turn}, key)["stateId"])
    except LinuxVMError as error:
        if error.uncertain or error.code != "operation_failed":
            raise
    # A guest from before 2026-10-12 runs `layr layer repair`, which layr renamed to `cache`,
    # so its line.save always fails. The same steps run as the line owner until the VM boots
    # the new guest. A read-only line saves as root, which layr.exec cannot do.
    line = context(runtime, agent)
    if line.get("readOnly"):
        raise ValueError("The guest cannot save this read-only line")
    output = ""
    for step, argv in (("repair", ["layr", "cache", "repair"]),
                       ("save", ["layr", "save", "--force", "-m", "Studio turn " + turn]),
                       ("state", ["layr", "rev-parse", "HEAD@{0}"])):
        result = request(runtime, "layr.exec", {"agentId": agent["id"], "argv": argv, "cwd": line["path"]},
                         key + ":" + step)
        if result.get("exitCode") != 0:
            raise ValueError("The layr " + step + " step of the turn save failed")
        output = base64.b64decode(result.get("stdout", "")).decode().strip()
    if not output:
        raise ValueError("The guest gave no state for the turn save")
    return output


def flush_turns(runtime: Any, agent: Mapping[str, Any]) -> None:
    from codex_linux_vm import LinuxVMError
    for turn, saved in list(agent.get("layrTurnSaves", {}).items()):
        if saved.get("status") == "saved":
            continue
        # The guest keeps the answer of each request ID, failures too. A retry after a lost
        # answer reuses the ID; a retry after a definite failure takes the next attempt, or
        # it would get the same failure from the receipt again.
        attempt = int(saved.get("attempt", 0))
        key = "layr-turn:" + agent["id"] + ":" + turn + (":" + str(attempt) if attempt else "")
        try:
            state = _save_turn(runtime, agent, turn, key)
        except Exception as error:
            if not isinstance(error, ValueError) and not (
                    isinstance(error, LinuxVMError) and not error.uncertain and error.code):
                raise
            with runtime.lock, runtime.db() as db:
                latest = runtime.agent(agent["id"], db)
                row = latest.setdefault("layrTurnSaves", {}).setdefault(turn, {"status": "pending"})
                if row.get("status") != "saved":
                    row["attempt"] = attempt + 1
                    runtime.put(db, "agents", latest)
            raise
        with runtime.lock, runtime.db() as db:
            latest = runtime.agent(agent["id"], db)
            latest.setdefault("layrTurnSaves", {})[turn] = {
                "status": "saved", "stateId": state,
            }
            latest["layrStateId"] = state
            latest.pop("layrError", None)
            latest.pop("layrTurnRetryAt", None)
            runtime.put(db, "agents", latest)


def tick(runtime: Any) -> None:
    with runtime.lock:
        if runtime.closed or runtime.__dict__.get("_layr_turn_busy"):
            return
        # Filter in SQLite: native chats must not add an agent decode to each scheduler pass.
        from codex_agent_modes import mode_fields
        with runtime.read_db() as db:
            agents = [mode_fields(json.loads(row[0])) for row in db.execute(
                "SELECT record FROM runtime_agents WHERE json_extract(record, '$.executionMode') = 'vm' "
                "AND COALESCE(json_extract(record, '$.layrTurnRetryAt'), 0) <= ? "
                "AND EXISTS (SELECT 1 FROM json_each(record, '$.layrTurnSaves') "
                "WHERE COALESCE(json_extract(value, '$.status'), '') != 'saved')", (time.time(),))]
        if not agents:
            return
        runtime._layr_turn_busy = True
    def save() -> None:
        try:
            for agent in agents:
                try:
                    flush_turns(runtime, agent)
                except Exception as error:
                    with runtime.lock, runtime.db() as db:
                        latest = runtime.agent(agent["id"], db)
                        latest["layrError"] = "Turn state unavailable: " + type(error).__name__
                        latest["layrTurnRetryAt"] = time.time() + 30
                        runtime.put(db, "agents", latest)
        finally:
            with runtime.lock:
                runtime._layr_turn_busy = False
    runtime.pool.submit(save)


def spawn_spec(actor: Mapping[str, Any], spec: Mapping[str, Any]) -> dict[str, Any]:
    if spec.get("environment") not in {None, "linux"} or spec.get("workspace") not in {None, "layr"}:
        raise ValueError("All agents in a VM chat use layr in the VM")
    if spec.get("server"):
        raise ValueError("VM chat workers must run on the chat server")
    directory = PurePosixPath(spec.get("cwd") or actor["cwd"])
    root = PurePosixPath(actor["cwd"])
    if not directory.is_absolute():
        directory = root / directory
    if ".." in directory.parts or not directory.is_relative_to(root):
        raise ValueError("A worker directory must stay in its parent layr line")
    base = spec.get("base_ref")
    if base is not None and (not isinstance(base, str) or not base.strip()):
        raise ValueError("base_ref must identify a layr state")
    return {**spec, "cwd": str(directory), "environment": "linux", "_worktree": False,
            "_workspaceMode": "layr", "_imageWorkspace": False,
            "layrSubpath": str(directory.relative_to(root)),
            **({"layrBaseState": base} if base else {})}


def validate_submission(runtime: Any, owner: Mapping[str, Any], revision: str) -> dict[str, Any]:
    result = request(runtime, "line.evidence", {"agentId": owner["id"], "stateId": revision},
                     "layr-evidence:" + owner["id"] + ":" + revision, 20)
    if result.get("stateId") != revision:
        raise ValueError("Submit the exact layr state ID")
    return {"layrProjectId": result["projectId"], "layrLine": result["line"], "layrStateId": revision}


def merge_result(runtime: Any, lead: Mapping[str, Any], owner: Mapping[str, Any],
                 result: Mapping[str, Any], identity: str) -> dict[str, Any]:
    if lead["layrProjectId"] != owner["layrProjectId"]:
        raise ValueError("The result belongs to another layr project")
    return request(runtime, "line.merge", {
        "agentId": lead["id"], "sourceAgentId": owner["id"],
        "expectedStateId": result["revision"],
    }, "layr-accept:" + identity)


def dispose(runtime: Any, agent: Mapping[str, Any]) -> dict[str, Any]:
    flush_turns(runtime, agent)
    return request(runtime, "agent.release" if agent.get("isLead") else "line.remove", {"agentId": agent["id"]},
                   "layr-remove:" + agent["id"] + ":" + str(agent["epoch"]))


def progress_context(runtime: Any, agent: Mapping[str, Any]) -> str:
    from codex_shared_values import progress_file_limit_label

    if is_vm(agent):
        path = Path(str(agent.get("layrHome", "/home/studio"))) / ".studio/progress" / agent["id"] / "PROGRESS.md"
        return (f"Your progress file is {path}. Read and edit this UTF-8 Markdown file with ordinary file tools. "
                f"Studio displays it above the composer. Keep it at most {progress_file_limit_label()}. "
                "Update it when the status changes.")
    from codex_progress import progress_context as native_context
    return str(native_context(runtime.root, agent["id"]))


def read_progress(runtime: Any, agent: Mapping[str, Any]) -> dict[str, Any]:
    if is_vm(agent) and agent.get("layrReady"):
        return request(runtime, "agent.progress", {"agentId": agent["id"]},
                       "layr-progress:" + agent["id"], 20)
    from codex_progress import read_progress as native_read
    return native_read(runtime.root, agent["id"])
