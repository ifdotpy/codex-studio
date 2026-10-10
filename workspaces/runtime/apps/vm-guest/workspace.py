"""Run the existing Linux workspace engine behind a process deadline."""
from __future__ import annotations

import json
from pathlib import Path
import sys
import threading

from common import runtime_source_dir
sys.path.insert(0, str(runtime_source_dir()))
import codex_workspace_images as images
import codex_workspace_linux as linux


def run(method, params):
    if method == "workspace.startBase":
        done = threading.Event()
        result = {}

        def complete(value):
            result.update(value)
            done.set()

        images.start_base_build(params["root"], complete, retry_failed=True)
        if not done.wait(params.get("timeoutSeconds", 1800)):
            raise TimeoutError("The base build exceeded its deadline")
        return result
    if method == "workspace.create":
        return images.create_workspace(params["root"], params["agentId"])
    if method == "workspace.archive":
        return images.archive_workspace(params["agentId"])
    if method == "workspace.remove":
        return images.remove_workspace(params["agentId"])
    if method == "workspace.status":
        rows = images.list_workspaces()
        if params.get("agentId"):
            rows = [row for row in rows if row.get("agentId") == params["agentId"]]
        result = {"workspaces": rows}
        if params.get("root"):
            root = Path(params["root"]).resolve()
            state = images._read_json(images._base_state_path(images._repo_key(root)), {}) or {}
            if state.get("state") == "building" and linux._proc_start_time(state.get("builderPid")) is None:
                state = {"state": "failed", "error": "The base builder stopped before completion"}
            result["base"] = {key: state.get(key) for key in ("state", "version", "error")}
            result["base"]["state"] = state.get("state", "missing")
        return result
    if method == "execPrefix":
        if params.get("agentId"):
            images.ensure_mounted(params["agentId"])
        return images.exec_prefix()
    raise ValueError("Unknown workspace operation")


if __name__ == "__main__":
    request = json.load(sys.stdin)
    try:
        result = run(request["method"], request["params"])
        print("\nGUEST_RESULT:" + json.dumps({"result": result}), flush=True)
    except Exception as exc:
        print("\nGUEST_RESULT:" + json.dumps({"error": str(exc)[:1000]}), flush=True)
        sys.exit(1)
