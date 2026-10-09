"""Per-chat VM execution and layr lifecycle. Native chats keep their old path."""
from __future__ import annotations

import hashlib
from pathlib import Path, PurePosixPath
import platform
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
            result = client(runtime).ensure_layr_project(
                project_id(agent), str(agent.get("hostProjectPath") or agent["cwd"]),
                owner=None, request_id="layr-project:" + project_id(agent))
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


def flush_turns(runtime: Any, agent: Mapping[str, Any]) -> None:
    for turn, saved in list(agent.get("layrTurnSaves", {}).items()):
        if saved.get("status") == "saved":
            continue
        result = request(runtime, "line.save", {"agentId": agent["id"], "turnId": turn},
                         "layr-turn:" + agent["id"] + ":" + turn)
        with runtime.lock, runtime.db() as db:
            latest = runtime.agent(agent["id"], db)
            latest.setdefault("layrTurnSaves", {})[turn] = {
                "status": "saved", "stateId": result["stateId"],
            }
            latest["layrStateId"] = result["stateId"]
            latest.pop("layrError", None)
            latest.pop("layrTurnRetryAt", None)
            runtime.put(db, "agents", latest)


def tick(runtime: Any) -> None:
    with runtime.lock:
        if runtime.closed or runtime.__dict__.get("_layr_turn_busy"):
            return
        with runtime.read_db() as db:
            agents = [a for a in runtime.records(db, "agents") if is_vm(a) and a.get("layrTurnRetryAt", 0) <= time.time()
                      and any(s.get("status") != "saved" for s in a.get("layrTurnSaves", {}).values())]
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
    if is_vm(agent):
        path = Path(str(agent.get("layrHome", "/home/studio"))) / ".studio/progress" / agent["id"] / "PROGRESS.md"
        return (f"Your progress file is {path}. Read and edit this UTF-8 Markdown file with ordinary file tools. "
                "Studio displays it above the composer. Keep it at most 128 KiB. Update it when the status changes.")
    from codex_progress import progress_context as native_context
    return str(native_context(runtime.root, agent["id"]))


def read_progress(runtime: Any, agent: Mapping[str, Any]) -> dict[str, Any]:
    if is_vm(agent) and agent.get("layrReady"):
        return request(runtime, "agent.progress", {"agentId": agent["id"]},
                       "layr-progress:" + agent["id"], 20)
    from codex_progress import read_progress as native_read
    return native_read(runtime.root, agent["id"])
