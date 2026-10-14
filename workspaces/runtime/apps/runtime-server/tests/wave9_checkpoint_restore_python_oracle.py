"""Run Python restore against a cloned fixture and emit comparable effects."""
import json
import pathlib
import shutil
import sys

root = pathlib.Path(sys.argv[1])
workspace = pathlib.Path(sys.argv[2])
agent_id, checkpoint_id, expected_tree = sys.argv[3:6]
mode = sys.argv[6]
state_copy = root / "python-oracle-state"
workspace_copy = root / "python-oracle-workspace"
shutil.copytree(
    root / "state",
    state_copy,
    ignore=lambda directory, names: [
        name for name in names if (pathlib.Path(directory) / name).is_socket()
    ],
)
shutil.copytree(workspace, workspace_copy)

source = pathlib.Path(__file__).resolve().parents[5] / "workspaces" / "runtime" / "apps" / "server" / "src"
sys.path.insert(0, str(source))
from codex_runtime import Runtime

runtime = Runtime(state_copy)
requests = []

class Provider:
    def call(self, method, params):
        requests.append({"method": method, "params": params})
        return {
            "thread": {"id": "restored-thread"},
            "sandbox": {"type": "workspaceWrite"},
            "approvalPolicy": "never",
            "activePermissionProfile": None,
        }

try:
    runtime.connect_agent = lambda _agent: Provider()
    with runtime.lock, runtime.db() as db:
        agent = runtime.agent(agent_id, db)
        agent["cwd"] = str(workspace_copy)
        runtime.put(db, "agents", agent, sync_rooms=False)
    result = runtime.restore_checkpoint(agent_id, {
        "checkpoint": checkpoint_id,
        "expectedTree": expected_tree,
    })
    with runtime.db() as db:
        agent = runtime.agent(agent_id, db)
        operation = runtime._workspace_operation(
            db, runtime._workspace_operation_id("restore", agent_id, {
                "checkpoint": checkpoint_id, "expectedTree": expected_tree,
            })
        )
        items = [
            json.loads(row[0])
            for row in db.execute(
                "SELECT record FROM runtime_items WHERE agent=? ORDER BY id", (agent_id,)
            )
        ]
        checkpoints = [
            json.loads(row[0])
            for row in db.execute(
                "SELECT record FROM runtime_checkpoints ORDER BY id"
            )
        ]
    print(json.dumps({
        "result": result,
        "requests": requests,
        "agent": agent,
        "operation": operation,
        "items": items,
        "checkpoints": checkpoints,
        "tree": runtime.snapshot_tree(agent),
        "mode": mode,
    }))
finally:
    runtime.close()
