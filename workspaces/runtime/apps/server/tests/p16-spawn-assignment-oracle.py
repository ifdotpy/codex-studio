"""Exercise the real Python spawn assignment with disposable Python-written rows."""
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[2] / "packages/runtime-services/tests/turn_lifecycle"))
from oracle_guard import install_guard
install_guard()

sys.path.insert(0, str(HERE.parent / "src"))
sys.path.insert(0, str(HERE))
import copy
import importlib.util
import json
import tempfile
from unittest.mock import patch

from runtime_test_environment import make_runtime

fixture_spec = importlib.util.spec_from_file_location("spawn_task_fixture", HERE / "worker-defaults-contract.py")
fixture = importlib.util.module_from_spec(fixture_spec)
fixture_spec.loader.exec_module(fixture)

CATALOG = {
    "data": [{"model": model, "defaultReasoningEffort": "low",
              "supportedReasoningEfforts": [{"reasoningEffort": "low"}], "serviceTiers": []}
             for model in ("gpt-6-astra", "gpt-5.6-sol", "gpt-6-luna", "worker", "small")]
}

def scenario(name, *, status="ready", owner=None, unknown=False, replay=False):
    with tempfile.TemporaryDirectory(prefix="p16-spawn-oracle-") as temporary:
        root = Path(temporary)
        runtime = make_runtime(fixture.ControlledRuntime, root, fixture.f.FakeServer)
        runtime.catalog = lambda account="default": copy.deepcopy(CATALOG)
        try:
            lead = runtime.new_lead({"cwd": str(root), "workspaceMode": "worktree"})
            work = runtime.work_action(lead["id"], {"action": "create", "title": "Oracle task"})
            if not unknown and (status != "ready" or owner is not None):
                work.update(status=status, owner=owner)
                with runtime.db() as db:
                    runtime.put(db, "work", work)
            with runtime.db() as db:
                before = next(value for value in runtime.records(db, "work") if value["id"] == work["id"])
            task_id = "missing-task" if unknown else work["id"]
            args = {"agents": [{"name": "Worker", "prompt": "Assign", "role": "reviewer", "task_id": task_id}]}
            try:
                with patch("codex_runtime.time.time", return_value=42.5):
                    first = runtime.spawn_agents(lead, args, "p16-" + name)
                    second = runtime.spawn_agents(lead, args, "p16-" + name) if replay else None
                error = None
            except ValueError as caught:
                first = second = None
                error = str(caught)
            with runtime.db() as db:
                row = next((value for value in runtime.records(db, "work") if value["id"] == work["id"]), None)
                count = db.execute("SELECT count(*) FROM runtime_agents").fetchone()[0]
            return {"name": name, "before": before, "after": row, "error": error,
                    "first": first, "second": second, "agentCount": count}
        finally:
            runtime.close()

cases = [scenario("assign"), scenario("unknown", unknown=True),
         scenario("accepted", status="accepted"), scenario("review", status="review"),
         scenario("owned", owner="someone-else"), scenario("same-owner-replay", replay=True)]
print(json.dumps(cases, sort_keys=True, separators=(",", ":")))
