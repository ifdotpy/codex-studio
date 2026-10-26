"""Prepare a native Layr folder before a provider can start a turn."""
from __future__ import annotations

import json
import os
from pathlib import Path
import platform
import shutil
import shlex
from typing import Any, Mapping

from codex_native_layr import Client


def client(runtime: Any) -> Client:
    binary = os.environ.get("STUDIO_NATIVE_LAYR_BINARY") or shutil.which("layr")
    if not binary:
        raise RuntimeError("Layr Native requires the Layr CLI on this server")
    state = Path(runtime.root) / "native-layr"
    if platform.system() == "Darwin":
        from codex_native_layr_store import ensure_macos
        store = ensure_macos(state / "storage")
        socket = None
    elif platform.system() == "Linux":
        root = os.environ.get("STUDIO_NATIVE_LAYR_ROOT")
        service = os.environ.get("STUDIO_NATIVE_LAYR_SOCKET")
        if not root or not service:
            raise RuntimeError("Layr Native needs a configured Linux store and service socket")
        store, socket = Path(root), Path(service)
        if not store.is_absolute() or not socket.is_absolute():
            raise ValueError("Native Layr store and socket paths must be absolute")
    else:
        raise RuntimeError("Layr Native supports macOS and Linux")
    return Client(state, store, Path(binary), socket)


def prepare(runtime: Any, agent: Mapping[str, Any]) -> Any:
    native = client(runtime)
    key = agent["id"]
    if agent.get("isLead"):
        source = Path(agent.get("hostProjectPath") or agent["cwd"])
        project_id = str(agent.get("projectId") or source)
        attached = native.ensure_project(project_id, source)
        project, line, path = attached["project"], attached["line"], attached["cwd"]
    else:
        parent = runtime.agent(agent["parentId"])
        if not parent.get("nativeLayrReady"):
            raise ValueError("The parent native Layr workspace is not ready")
        project = parent["nativeLayrProject"]
        # Persist the exact base before the branch command. A later parent
        # commit must not change the meaning of a retried worker request.
        state = agent.get("layrBaseState")
        if not state:
            state = native.command("rev-parse", "HEAD", cwd=Path(parent["cwd"]))
            with runtime.lock, runtime.db() as db:
                current = runtime.agent(key, db)
                if current.get("deletedAt"):
                    raise ValueError("The worker was removed before native Layr preparation")
                current["layrBaseState"] = state
                runtime.put(db, "agents", current)
        worker = native.ensure_worker(project, key, Path(parent["cwd"]), str(state))
        line, path = worker["line"], worker["cwd"]
    info = json.loads(native.command("workspace", "inspect", project, "--line", line))
    if info.get("path") != path:
        raise RuntimeError("The native Layr folder identity changed")
    if agent.get("nativeLayrLineId") and agent["nativeLayrLineId"] != info.get("line_id"):
        raise RuntimeError("The native Layr line was replaced")
    root = Path(path).resolve(strict=True)
    relative = Path(agent.get("layrSubpath") or ".")
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError("Invalid native Layr workspace subpath")
    directory = (root / relative).resolve(strict=True)
    if not directory.is_relative_to(root) or not directory.is_dir():
        raise ValueError("The worker directory must stay inside its native Layr line")
    with runtime.lock, runtime.db() as db:
        current = runtime.agent(key, db)
        if current.get("deletedAt"):
            raise ValueError("The chat was removed during native Layr preparation")
        current.update(cwd=str(directory), environment="host", worktree=False,
                       nativeLayrReady=True, nativeLayrProject=project,
                       nativeLayrLineId=info["line_id"], layrLine=line,
                       nativeLayrRoot=str(native.store),
                       nativeLayrFolder=str(root),
                       nativeLayrSocket=str(native.socket) if native.socket else None,
                       layrStateId=info["state"])
        runtime.put(db, "agents", current)
    from codex_vm_agents import flush_turns
    flush_turns(runtime, runtime.agent(key))
    return runtime.agent(key)


def command_environment(agent: Mapping[str, Any]) -> dict[str, str]:
    if not agent.get("nativeLayrReady"):
        raise ValueError("The native Layr workspace is not ready")
    result = {"LAYR_ROOT": str(agent["nativeLayrRoot"]), "LAYR_SESSION": str(agent["id"]),
              "LAYR_PROJECT": str(agent["nativeLayrProject"]), "LAYR_LINE": str(agent["layrLine"]),
              "LAYR_DIRECT": "0" if agent.get("nativeLayrSocket") else "1"}
    if agent.get("nativeLayrSocket"):
        result["LAYR_SOCKET"] = str(agent["nativeLayrSocket"])
    return result


def spawn_spec(actor: Mapping[str, Any], spec: Mapping[str, Any]) -> dict[str, Any]:
    if spec.get("environment") not in {None, "host"} or spec.get("workspace") not in {None, "layr-native"}:
        raise ValueError("Workers in a native Layr chat use native Layr lines")
    if spec.get("server"):
        raise ValueError("Native Layr workers must run on the chat server")
    root = Path(actor["nativeLayrFolder"]).resolve(strict=True)
    directory = Path(actor["cwd"]) / (spec.get("cwd") or ".")
    directory = directory.resolve(strict=True)
    if not directory.is_relative_to(root) or not directory.is_dir():
        raise ValueError("The worker directory must stay inside its parent native Layr line")
    base = spec.get("base_ref")
    if base is not None and (not isinstance(base, str) or not base.strip() or base.startswith("-")):
        raise ValueError("base_ref must identify a Layr state")
    return {**spec, "cwd": str(directory), "environment": "host", "_worktree": False,
            "_workspaceMode": "layr-native", "_imageWorkspace": False,
            "layrSubpath": str(directory.relative_to(root)),
            **({"layrBaseState": base} if base else {})}


def managed_command(agent: Mapping[str, Any], command: str) -> str:
    """Apply the chat environment after shell startup, for every monitor mode."""
    if agent.get("workspaceMode") != "layr-native":
        return command
    values = command_environment(agent)
    lines = ["unset LAYR_SOCKET"] if "LAYR_SOCKET" not in values else []
    lines.extend("export " + key + "=" + shlex.quote(value) for key, value in values.items())
    return "\n".join([*lines, command])


def validate_submission(runtime: Any, owner: Mapping[str, Any], revision: str) -> dict[str, Any]:
    if not owner.get("nativeLayrReady"):
        raise ValueError("The native Layr worker is not ready")
    native = client(runtime)
    project, line = owner["nativeLayrProject"], owner["layrLine"]
    info = json.loads(native.command("workspace", "inspect", project, "--line", line))
    if info.get("line_id") != owner["nativeLayrLineId"] or info.get("path") != owner["nativeLayrFolder"]:
        raise ValueError("The native worker identity changed")
    if info.get("state") != revision:
        raise ValueError("Submit the exact current Layr state ID")
    return {"layrProjectId": info["project_id"], "layrLine": line, "layrStateId": revision}


def merge_result(runtime: Any, lead: Mapping[str, Any], owner: Mapping[str, Any],
                 result: Mapping[str, Any], identity: str) -> dict[str, str]:
    if (not lead.get("nativeLayrReady") or not owner.get("nativeLayrReady")
            or lead.get("nativeLayrProject") != owner.get("nativeLayrProject")
            or lead.get("nativeLayrRoot") != owner.get("nativeLayrRoot")):
        raise ValueError("The result belongs to another native Layr project")
    native = client(runtime)
    return native.merge_result(lead["nativeLayrProject"], lead["layrLine"],
                               Path(lead["nativeLayrFolder"]), result["revision"],
                               identity, lead["nativeLayrLineId"])
