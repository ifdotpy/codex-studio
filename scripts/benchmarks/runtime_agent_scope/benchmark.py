#!/usr/bin/env python3
"""Compare scoped runtime operations using isolated synthetic SQLite state."""
import argparse
import json
import os
from pathlib import Path
import sys
import tempfile
import time


parser = argparse.ArgumentParser()
parser.add_argument("--runtime-source", type=Path, default=Path(__file__).resolve().parents[3])
args = parser.parse_args()
source = args.runtime_source.resolve()
sys.path.insert(0, str(source / "scripts"))
from codex_runtime import Runtime, team_capacity_counts  # noqa: E402


class _Server:
    pass


class _Voice:
    @staticmethod
    def delete_agent(_agent_id, _db):
        return None


def fixture(root):
    runtime = Runtime(root, _Server)
    now = time.time()
    with runtime.db() as db:
        for team in range(30):
            root_id = f"lead-{team:02}"
            for member in range(50):
                agent_id = root_id if member == 0 else f"worker-{team:02}-{member:02}"
                agent = {
                    "id": agent_id, "rootId": root_id,
                    "parentId": None if member == 0 else root_id,
                    "isLead": member == 0, "role": "orchestrator" if member == 0 else "worker",
                    "name": agent_id, "status": "completed", "autoWake": False,
                    "deletedAt": None, "epoch": 0, "accountKey": "bench-alt" if team == 0 else "default",
                    "cwd": str(source), "threadId": None, "turnId": None,
                    "inFlight": False, "maxAgents": 100, "tokenBudget": None,
                    "concurrency": 2 if member == 0 else 0, "created": now,
                }
                db.execute("INSERT INTO runtime_agents(id,record) VALUES (?,?)",
                           (agent_id, json.dumps(agent)))
            room = {"id": "broadcast:" + root_id, "kind": "broadcast", "rootId": root_id,
                    "updated": now}
            db.execute("INSERT INTO runtime_rooms(id,record) VALUES (?,?)",
                       (room["id"], json.dumps(room)))
            private = {"id": f"private:{root_id}:worker-{team:02}-01", "kind": "private",
                       "members": [root_id, f"worker-{team:02}-01"], "updated": now}
            db.execute("INSERT INTO runtime_rooms(id,record) VALUES (?,?)",
                       (private["id"], json.dumps(private)))
    runtime.voice = lambda: _Voice()
    runtime.stop = lambda *_args, **_kwargs: None
    return runtime


def lock_hold_delta(runtime, before):
    getter = getattr(runtime.lock, "runtime_lock_metrics", None)
    if getter is None:
        return None
    old = {row["callSite"]: row["totalHoldMs"] for row in before}
    return round(sum(row["totalHoldMs"] - old.get(row["callSite"], 0)
                     for row in getter()), 3)


def measure(label, operation):
    state = Path(tempfile.mkdtemp(prefix="runtime-agent-scope-"))
    runtime = fixture(state)
    try:
        metrics = getattr(runtime.lock, "runtime_lock_metrics", lambda: [])
        before = metrics()
        started = time.perf_counter()
        value = operation(runtime)
        wall_ms = round((time.perf_counter() - started) * 1000, 3)
        held_ms = lock_hold_delta(runtime, before)
        return {"scenario": label, "wallMs": wall_ms, "lockHoldMs": held_ms,
                "result": value}
    finally:
        runtime.close()


def delete_tree(runtime):
    result = runtime.delete_conversation("lead-00")
    if len(result["deleted"]) != 50:
        raise AssertionError(f"delete returned {len(result['deleted'])} agents, expected 50")
    return {"deleted": len(result["deleted"])}


def disconnect(runtime):
    runtime.disconnected("bench-alt")
    return {"accountAgents": 50}


def capacity(runtime):
    with runtime.lock, runtime.db() as db:
        active, finished = team_capacity_counts(runtime.team_agents(db, "lead-01"), "lead-01")
    return {"active": active, "finished": finished}


def subtree_read(runtime):
    with runtime.lock, runtime.db() as db:
        if hasattr(runtime, "descendant_agents"):
            agents = runtime.descendant_agents(db, "lead-00")
        else:
            agents = runtime.records(db, "agents")
            ids = {"lead-00"}
            while True:
                expanded = ids | {a["id"] for a in agents if a.get("parentId") in ids}
                if expanded == ids:
                    break
                ids = expanded
            agents = [agent for agent in agents if agent["id"] in ids]
        if len(agents) != 50:
            raise AssertionError(f"subtree read returned {len(agents)} agents, expected 50")
        return {"agents": len(agents)}


def settings(runtime):
    result = runtime.configure("lead-01", {"maxAgents": 99, "tokenBudget": None})
    return {"team": result["id"], "maxAgents": result["maxAgents"]}


def main():
    if not os.environ.get("TMPDIR"):
        raise SystemExit("Set TMPDIR to an isolated scratch directory")
    rows = [measure(name, operation) for name, operation in (
        ("delete-50-agent-tree", delete_tree),
        ("disconnect-50-account-agents", disconnect),
        ("read-50-agent-subtree", subtree_read),
        ("team-capacity-50-agents", capacity),
        ("team-settings-50-agents", settings),
    )]
    print(json.dumps({"source": str(source), "agents": 1500, "teams": 30,
                      "agentsPerTeam": 50, "metricsEnabled":
                      os.environ.get("CODEX_RUNTIME_LOCK_METRICS") == "1",
                      "results": rows}, sort_keys=True))


if __name__ == "__main__":
    main()
