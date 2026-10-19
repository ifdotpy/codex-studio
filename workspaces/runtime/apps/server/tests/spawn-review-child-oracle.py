"""Run the real Python child creation entry points for the Rust corpus."""
import json
import os
import sys
import tempfile
from pathlib import Path

CORPUS_TMPDIR = os.environ.get("TMPDIR")
HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'src'))
sys.path.insert(0, str(HERE.parents[2] / "packages/runtime-services/tests/turn_lifecycle"))
from oracle_guard import install_guard
install_guard()
sys.path.insert(0, str(HERE.parent / "src"))
sys.path.insert(0, str(HERE))

from runtime_test_environment import make_runtime
import importlib.util
spec = importlib.util.spec_from_file_location("spawn_review_fixture", HERE / "worker-defaults-contract.py")
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)

CATALOG = {"data": [{"model": model, "defaultReasoningEffort": "low",
                      "supportedReasoningEfforts": [{"reasoningEffort": "low"}], "serviceTiers": []}
                     for model in ("gpt-6-astra", "gpt-6-luna")]}

with tempfile.TemporaryDirectory(prefix="spawn-review-corpus-", dir=CORPUS_TMPDIR) as temporary:
    root = Path(temporary)
    project = Path(sys.argv[1]).resolve(strict=True)
    review_cwd = sys.argv[2]
    runtime = make_runtime(fixture.ControlledRuntime, root / "state", fixture.f.FakeServer)
    python_home = Path(os.environ["HOME"])
    python_home.mkdir(parents=True, exist_ok=True)
    (python_home / "review-target").symlink_to(project / "workspaces/runtime/apps/server", target_is_directory=True)
    runtime.catalog = lambda account="default": {"data": [row for row in CATALOG["data"]
        if row["model"] != "gpt-6-luna" or account == "alternate"]}
    lead = runtime.new_lead({"cwd": str(project), "workspaceMode": "worktree"})
    with runtime.db() as db:
        from codex_execution import ensure_tables, _new_run, _save_run
        ensure_tables(db)
        run = _new_run(lead, "spawn-review-corpus", {"created": 10.0})
        run["status"] = "pending"
        _save_run(db, run)
        db.execute("INSERT INTO runtime_profiles(id,record) VALUES (?,?)", (
            "profile-reviewer", json.dumps({"id": "profile-reviewer", "name": "Profile",
                "role": "implementer", "model": "gpt-6-luna", "effort": None,
                "instructions": "profile instructions"})))
        project_row = db.execute("SELECT record FROM runtime_projects WHERE id=?", (str(project),)).fetchone()
        project_record = json.loads(project_row[0]) if project_row else {
            "id": str(project), "path": str(project), "name": project.name, "created": 1.0,
            "accountKey": "alternate", "accountRevision": 1}
        project_record["workerBaseRef"] = "HEAD~1"
        db.execute("INSERT INTO runtime_projects(id,record) VALUES (?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
                   (str(project), json.dumps(project_record)))
    account_get = runtime.accounts.get
    account_list = runtime.accounts.list
    def account(key):
        if key == "alternate":
            return {**account_get("default"), "accountKey": key, "provider": "codex",
                    "status": "ready", "disconnected": False, "deleted": False}
        return account_get(key)
    def accounts():
        rows = account_list()
        if not any(row.get("id") == "alternate" for row in rows):
            rows.append({**account("alternate"), "id": "alternate"})
        return rows
    runtime.accounts.get = account
    runtime.accounts.list = accounts
    lead["accountKey"] = "default"
    lead["effort"] = "low"
    with runtime.db() as db:
        runtime.put(db, "agents", lead)
    spawned = runtime.spawn_agents(lead, {"agents": [{"name": "Worker", "prompt": "Run the task",
        "workspace": "worktree", "account_key": "alternate", "profile_id": "profile-reviewer", "effort": None}]},
        "dispatcher-spawn-api")
    spawn_id = spawned["agents"][0]["id"]
    spawn_child = runtime.agent(spawn_id)
    fallback = runtime.spawn_agents(lead, {"agents": [{"name": "Fallback", "prompt": "Use the first available account",
        "model": "gpt-6-luna", "workspace": "shared"}]}, "dispatcher-spawn-fallback")
    fallback_child = runtime.agent(fallback["agents"][0]["id"])
    alternate = runtime.spawn_agents(lead, {"agents": [{"name": "Alternate", "prompt": "Work in the server folder",
        "model": "gpt-6-luna", "workspace": "shared", "cwd": "workspaces/runtime/apps/server"}]},
        "dispatcher-spawn-alternate")
    alternate_child = runtime.agent(alternate["agents"][0]["id"])
    reviewed = __import__("codex_agent_review").request(runtime, lead,
        {"target": {"type": "uncommittedChanges"}, "cwd": review_cwd}, "dispatcher-review-api")
    review_child = runtime.agent(reviewed["agentId"])
    try:
        runtime.spawn_agents(lead, {"agents": [{"name": "Explicit reviewer workspace", "role": "reviewer",
            "prompt": "Review", "workspace": "shared"}]}, "dispatcher-workspace-refusal")
        workspace_refused = False
    except ValueError:
        workspace_refused = True
    with runtime.lock, runtime.db() as db:
        root_record = runtime.agent(lead["rootId"], db)
        root_record["maxAgents"] = 1
        runtime.put(db, "agents", root_record)
        count_before = db.execute("SELECT count(*) FROM runtime_agents").fetchone()[0]
    try:
        __import__("codex_agent_review").request(runtime, lead,
            {"target": {"type": "uncommittedChanges"}, "cwd": str(project / "workspaces/runtime/apps/server")}, "dispatcher-capacity-refusal")
        capacity_error = None
    except ValueError as error:
        capacity_error = str(error)
    with runtime.db() as db:
        count_after = db.execute("SELECT count(*) FROM runtime_agents").fetchone()[0]
    with runtime.db() as db:
        def event(agent_id):
            row = db.execute("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events WHERE id=?",
                             (agent_id + ":initial",)).fetchone()
            return dict(row) if row else None
        def execution_rows(table, agent_id):
            return [json.loads(row[0]) for row in db.execute(
                "SELECT record FROM runtime_execution_" + table + " WHERE " +
                ("reference=?" if table == "effects" else "json_extract(record,'$.agentId')=?"),
                (agent_id,))]
        spawn_event = event(spawn_id)
        spawn_effects = execution_rows("effects", spawn_id)
        spawn_nodes = execution_rows("nodes", spawn_id)
    print(json.dumps({"spawn": {"response": spawned, "child": spawn_child,
        "event": spawn_event, "effects": spawn_effects, "nodes": spawn_nodes},
        "fallback": {"response": fallback, "child": fallback_child},
        "alternate": {"response": alternate, "child": alternate_child},
        "base": spawned["agents"][0],
        "workspaceRefused": workspace_refused,
        "capacity": {"error": capacity_error, "countBefore": count_before, "countAfter": count_after},
        "review": {"response": reviewed, "child": review_child}}, sort_keys=True, separators=(",", ":")))
    runtime.close()
