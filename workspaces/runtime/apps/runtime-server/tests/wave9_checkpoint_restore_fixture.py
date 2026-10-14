"""Create a Git-backed checkpoint fixture and print Python's restore inputs."""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_runtime import Runtime

state = pathlib.Path(sys.argv[1])
workspace = pathlib.Path(sys.argv[2])
turn_id = "" if sys.argv[3] == "__empty__" else (sys.argv[3] or None)
agent_id = "wave9-checkpoint-restore-agent"
runtime = Runtime(state)
try:
    agent = {
        "id": agent_id,
        "rootId": agent_id,
        "epoch": 7,
        "status": "paused",
        "name": "Checkpoint restore contract",
        "role": "implementer",
        "cwd": str(workspace),
        "isLead": False,
        "autoWake": False,
        "approvalPolicy": "never",
        "worktreeReady": True,
        "workspaceMode": "worktree",
        "accountKey": "default",
        "executionSettingsAccountKey": "default",
        "threadId": "old-thread",
        "model": "fixture-model",
        "nativeEffort": None,
        "startAttempt": {"settingsFixed": True},
        "sandbox": {"type": "workspaceWrite"},
        "activePermissionProfile": None,
        "lastCompletedTurn": turn_id,
    }
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent, sync_rooms=False)
    checkpoint = runtime.capture_checkpoint(agent_id, "Wave 9 restore fixture")
    params = runtime.new_thread_params(agent)
    (workspace / "tracked.txt").write_text("preview state\n", encoding="utf-8")
    expected_tree = runtime.snapshot_tree(agent)
    print(json.dumps({
        "agent": agent_id,
        "checkpoint": checkpoint["id"],
        "checkpointTree": checkpoint["tree"],
        "expectedTree": expected_tree,
        "startParams": params,
        "threadConfig": runtime.thread_config(),
        "model": agent["model"],
        "threadId": checkpoint.get("threadId"),
        "turnId": checkpoint.get("turnId"),
    }))
finally:
    runtime.close()
