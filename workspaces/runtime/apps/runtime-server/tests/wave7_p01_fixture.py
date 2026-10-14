"""Build existing-runtime records without starting its provider or scheduler."""
import json
import pathlib
import sys
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT / "workspaces/runtime/packages/runtime-services/tests/turn_lifecycle"))
from oracle_guard import install_guard

_GUARD = install_guard()
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_runtime import Runtime
import codex_runtime

class QuietRuntime(Runtime):
    def schedule(self):
        pass

    def connect(self, *args, **kwargs):
        raise AssertionError("provider forbidden")

root = pathlib.Path(sys.argv[1])
_connect = codex_runtime.sqlite_connect

def fixture_connect(*args, **kwargs):
    db = _connect(*args, **kwargs)
    db.execute("PRAGMA synchronous=OFF")
    return db

with patch.object(codex_runtime, "sqlite_connect", fixture_connect):
    runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agents = []
recovery = len(sys.argv) > 2 and sys.argv[2] == "recovery"
cases = [
    {"status": "completed", "disconnectRecovery": {}},
    {"status": "completed", "disconnectRecovery": {"stage": "pending"}},
    {"status": "waiting", "restartRecovery": {"stage": "finished"}},
    {"status": "waiting", "restartRecovery": {"stage": "pending"}},
    {"status": "interrupted", "restartRecovery": {"stage": "finished"}},
    {"status": "paused", "restartRecovery": {"stage": "unknown"}},
    {"status": "parked", "restartRecovery": {"stage": "reattached"}},
    {"status": "completed", "restartRecovery": {"stage": "input_restored"}},
    {"status": "completed", "restartRecovery": None},
] if recovery else [{}, {}]
for index, case in enumerate(cases):
    agent = runtime.create({"name": "Registry fixture", "cwd": str(root), "prompt": "temporary"}, defer=True)
    account = f"case-{index}" if recovery else ["default", "second"][index]
    agent.update(autoWake=True, status="queued", accountKey=account,
                 threadId=f"fake-thread-{index}", model="fake-model", worktree=False,
                 workspaceMode="shared", nativeEffort=None, executionSettingsAccountKey=account)
    if recovery:
        agent.update(autoWake=False, **case)
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent)
    agents.append({"id": agent["id"], "epoch": agent["epoch"]})
if recovery:
    with runtime.lock, runtime.db() as db:
        selected = sorted({a["accountKey"] for a in runtime.scheduler_agents(db)})
        stored = {row[0]: json.loads(row[1]) for row in db.execute("SELECT id,record FROM runtime_agents")}
    print(json.dumps({"selected": selected, "stored": stored, "cases": len(cases)}))
else:
    print(json.dumps(agents))
runtime.close()
