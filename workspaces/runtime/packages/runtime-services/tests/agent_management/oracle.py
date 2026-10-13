"""Python lifecycle oracle: actual methods with an in-memory storage/effect port.

No Runtime is constructed, no provider is run, and external effects fail loudly.
Archive outputs subsequently become restore inputs, exercising Python-written state.
"""
import copy
import json
from pathlib import Path
import sqlite3
import sys
import threading
from contextlib import contextmanager
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "turn_lifecycle"))
from oracle_guard import install_guard
install_guard(block_processes=True)
ROOT = HERE.parents[5]
sys.path.insert(0, str(ROOT / "workspaces/runtime/apps/server/src"))
from codex_runtime import Runtime
from codex_agent_management import manage_agent
from codex_agent_modes import mode_fields
from studio_api.persisted_models import PersistedAgentRecord

NOW = 1234567.5
TABLES = ("agents", "work", "requests", "monitors", "tasks", "tool_requests")


class MemoryRuntime:
    def __init__(self, records):
        self.connection = sqlite3.connect(":memory:")
        self.connection.row_factory = sqlite3.Row
        self.lock = threading.RLock()
        self.preparations = {}
        self.closed = False
        for table in TABLES:
            self.connection.execute(f"CREATE TABLE runtime_{table}(id TEXT PRIMARY KEY,record TEXT)")
        self.connection.execute("CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,status TEXT,epoch INTEGER)")
        for table, rows in records["tables"].items():
            for record in rows:
                if table == "runtime_events":
                    self.connection.execute("INSERT INTO runtime_events VALUES(?,?,?,?)", tuple(record[k] for k in ("id", "agent", "status", "epoch")))
                else:
                    self.put(self.connection, table.removeprefix("runtime_"), record)
        self.connection.commit()

    @contextmanager
    def db(self):
        with self.connection:
            yield self.connection

    def put(self, db, table, record, **_kwargs):
        if table == "agents":
            PersistedAgentRecord.model_validate(record)
        db.execute(f"INSERT INTO runtime_{table} VALUES(?,?) ON CONFLICT(id) DO UPDATE SET record=excluded.record", (record["id"], json.dumps(record)))

    def agent(self, key, db=None):
        row = (db or self.connection).execute("SELECT record FROM runtime_agents WHERE id=?", (key,)).fetchone()
        if row is None:
            raise ValueError("Unknown agent")
        return json.loads(row[0])

    def records(self, db, table):
        return [json.loads(row[0]) for row in db.execute(f"SELECT record FROM runtime_{table} ORDER BY rowid")]

    descendant_agents = staticmethod(Runtime.descendant_agents)
    release_work_agents = staticmethod(Runtime.release_work_agents)
    _settle_deleted_conversations = Runtime._settle_deleted_conversations
    stop = Runtime.stop

    def capacity_reset(self, db, agent, reason):
        assert not agent.get("capacityRetry")
        agent.pop("capacityRetryCount", None)

    def usage_resume_cancel(self, db, agent, reason):
        assert not agent.get("usageResume")

    def interrupt(self, agent):
        assert not agent.get("threadId") and not agent.get("inFlight")

    def child_stopped_event(self, db, agent, *args, **kwargs):
        assert agent.get("isLead") or agent.get("deletedAt") or agent.get("agentArchive")

    def reconcile_tool_requests(self, db, agent):
        assert not list(db.execute("SELECT 1 FROM runtime_tool_requests"))

    def voice(self):
        return self

    def delete_agent(self, key, db):
        pass

    def affected_agent_room_ids(self, db, before, after):
        return set()

    def sync_agent_rooms(self, db, rooms, fallback):
        assert not rooms and not fallback

    def release_failed_work(self, db, agents, force):
        assert not agents

    def snapshot(self):
        result = {"tables": {"runtime_"+table: self.records(self.connection, table) for table in TABLES}}
        result["tables"]["runtime_events"] = [dict(row) for row in self.connection.execute("SELECT * FROM runtime_events ORDER BY rowid")]
        return result


def seed():
    lead = dict(id="lead", rootId="lead", parentId=None, epoch=3, status="idle", name="Lead", cwd="/fixture", isLead=True, autoWake=True, inFlight=False, created=1.0)
    worker = dict(lead, id="worker", parentId="lead", isLead=False, status="completed", name="Worker", epoch=8)
    mode_fields(lead)
    mode_fields(worker)
    return {"tables": {"runtime_agents": [lead, worker]}}


def run(name, records, action):
    runtime = MemoryRuntime(records)
    before = runtime.snapshot()
    try:
        with patch("time.time", return_value=NOW):
            if action["action"] == "stop":
                result = Runtime.stop(runtime, action["target"], action["descendants"])
            elif action["action"] == "delete":
                result = Runtime.delete_conversation(runtime, action["target"])
            else:
                args = {"action": action["action"], "agent_id": action["target"]}
                args.update({k: v for k, v in action.items() if k in {"reason", "unassign_work"}})
                result = manage_agent(runtime, action["actor"], args, action["epoch"])
        outcome = {"result": result, "records": runtime.snapshot()}
    except ValueError as error:
        outcome = {"error": str(error), "records": runtime.snapshot()}
    finally:
        runtime.connection.close()
    return dict(name=name, records=before, action=action, now=NOW, expected=outcome)


def corpus():
    cases = []
    archive = dict(action="archive", actor="lead", epoch=3, target="worker", reason="Finished work", unassign_work=False)
    restore = dict(action="restore", actor="lead", epoch=3, target="worker")
    for status in ("completed", "failed", "interrupted", "idle", "paused"):
        records = seed()
        records["tables"]["runtime_agents"][1]["status"] = status
        row = run("archive-"+status, records, archive)
        cases.append(row)
        cases.append(run("restore-"+status, row["expected"]["records"], restore))
        cases.append(run("replay-"+status, row["expected"]["records"], archive))
    for key, value in (("epoch", 2), ("isLead", False), ("autoWake", False), ("deletedAt", True)):
        records = seed()
        records["tables"]["runtime_agents"][0][key] = value
        cases.append(run("actor-"+key, records, archive))
    for key, value in (("rootId", "foreign"), ("parentId", None), ("parentId", "worker"), ("isLead", True), ("deletedAt", True)):
        records = seed()
        records["tables"]["runtime_agents"][1][key] = value
        cases.append(run("target-"+key+str(value), records, archive))
    for table, row in (
        ("runtime_events", dict(id="pending", agent="worker", epoch=8, status="pending")),
        ("runtime_requests", dict(id="question", agent="worker", status="pending", method="agent/asyncQuestion")),
        ("runtime_monitors", dict(id="monitor", agent="worker", status="approval")),
        ("runtime_work", dict(id="work", owner="worker", rootId="lead", version=2, status="review", title="Assignment", results=[dict(id="result", text="Keep this result")], decisions=[])),
    ):
        records = seed()
        records["tables"][table] = [row]
        cases.append(run("blocker-"+table, records, archive))
        if table == "runtime_work":
            cases.append(run("unassign", records, dict(archive, unassign_work=True)))
            records["tables"][table][0]["rootId"] = "foreign"
            cases.append(run("unassign-foreign", records, dict(archive, unassign_work=True)))
    archived = cases[0]["expected"]["records"]
    for key, value in (("epoch", 900), ("deletedAt", True)):
        records = copy.deepcopy(archived)
        records["tables"]["runtime_agents"][1][key] = value
        cases.append(run("stale-"+key, records, restore))
    records = copy.deepcopy(archived)
    records["tables"]["runtime_agents"][1]["agentArchive"]["cleanupPending"] = True
    cases.append(run("cleanup-pending", records, restore))
    cases.append(run("not-archived", seed(), restore))
    for descendants in (False, True):
        records = seed()
        records["tables"]["runtime_events"] = [dict(id="pending", agent="lead", epoch=3, status="pending"), dict(id="uncertain", agent="lead", epoch=3, status="uncertain")]
        records["tables"]["runtime_requests"] = [dict(id="question", agent="lead", status="pending", method="agent/asyncQuestion"), dict(id="approval", agent="lead", status="pending", method="monitor/approve")]
        records["tables"]["runtime_agents"][0].update(parkedEvent="release", parkReceipt=dict(event="release",epoch=3,sequence=1),parkAfterTurn=True)
        cases.append(run("stop-"+str(descendants), records, dict(action="stop", target="lead", descendants=descendants)))
    cases.append(run("delete", seed(), dict(action="delete", target="lead")))
    cases.append(run("delete-retry", cases[-1]["expected"]["records"], dict(action="delete", target="lead")))
    return cases


if __name__ == "__main__":
    print(json.dumps(corpus(), ensure_ascii=False))
