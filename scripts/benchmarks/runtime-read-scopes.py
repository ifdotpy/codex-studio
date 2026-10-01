#!/usr/bin/env python3
"""Compare old global reads with scoped runtime paths on a synthetic SQLite sample."""
import json
import sqlite3
import statistics
import time

from pathlib import Path
import sys
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from codex_runtime import Runtime


class Roster:
    scheduler_agents = Runtime.scheduler_agents
    team_agents = Runtime.team_agents

    def __init__(self):
        self.__dict__ = {}


def record_bytes(rows, column=0):
    return sum(len(row[column].encode()) if isinstance(row[column], str) else 0 for row in rows)


def measure(call, repeats=5):
    durations, result = [], None
    for _ in range(repeats):
        started = time.perf_counter()
        result = call()
        durations.append((time.perf_counter() - started) * 1000)
    return round(statistics.median(durations), 3), result


def add_task_rows(db, agents):
    db.execute("CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
    rows = []
    for number in range(72_902):
        agent = agents[number % len(agents)]
        item = {"id": f"task-{number:06}", "agent": agent, "status": "completed",
                "created": float(number), "finished": float(number + 1), "kind": "command",
                "name": "commandExecution", "command": "sample", "cwd": "/sample",
                "tail": "t" * 380, "arguments": "a" * 320, "error": "e" * 160}
        rows.append((item["id"], json.dumps(item, separators=(",", ":"))))
    db.executemany("INSERT INTO runtime_tasks VALUES (?,?)", rows)
    db.execute("CREATE INDEX runtime_task_workspace_agent_status_created ON runtime_tasks("
               "json_extract(record,'$.agent'),json_extract(record,'$.status'),json_extract(record,'$.created') DESC)")
    db.execute("CREATE INDEX runtime_task_history ON runtime_tasks("
               "json_extract(record,'$.created') DESC,json_extract(record,'$.agent')) "
               "WHERE json_extract(record,'$.status')!='running'")


def old_feed_cursor(db, agents):
    effective = ("CASE WHEN COALESCE(json_extract(t.record,'$.finished'),0)>"
                 "COALESCE(json_extract(t.record,'$.created'),0) THEN json_extract(t.record,'$.finished') "
                 "ELSE json_extract(t.record,'$.created') END")
    placeholders = ",".join("?" for _ in agents)
    return db.execute("SELECT t.record," + effective + " AS updated,t.id FROM runtime_tasks t WHERE "
        "json_extract(t.record,'$.agent') IN (" + placeholders + ") AND (" + effective +
        " > ? OR (" + effective + "=? AND t.id>?)) ORDER BY updated,t.id LIMIT 101",
        (*agents, 100_000, 100_000, "")).fetchall()


def make_agent_rows(db):
    db.execute("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
    db.execute("CREATE INDEX runtime_agent_root ON runtime_agents(json_extract(record,'$.rootId'))")
    db.execute("CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT NOT NULL)")
    db.execute("CREATE INDEX runtime_work_root_status ON runtime_work("
               "json_extract(record,'$.rootId'),json_extract(record,'$.status'))")
    db.execute("CREATE INDEX runtime_work_owner_status ON runtime_work("
               "json_extract(record,'$.owner'),json_extract(record,'$.status'))")
    agent_rows = []
    roots = [f"team-{index}" for index in range(4)]
    for number in range(308):
        deleted = number >= 156
        root = roots[(number % 156) // 39]
        item = {"id": f"agent-{number:03}", "rootId": root, "parentId": root,
                "name": f"Agent {number}", "role": "implementer",
                "status": "completed" if deleted else "queued",
                "autoWake": not deleted, "deletedAt": time.time() if deleted else None,
                "isLead": False, "padding": "p" * 20_000}
        if deleted:
            item["lastContextRepairWait"] = {"status": "superseded"}
        elif number % 13 == 0:
            item.update(status="running", inFlight=True)
        if number < 5:
            item["capacityRetry"] = {"status": "scheduled", "dueAt": time.time() - 1,
                                     "id": f"retry-{number}"}
        elif 10 <= number < 60:
            item["capacityRetry"] = {"status": "scheduled", "dueAt": time.time() + 3600,
                                     "id": f"retry-{number}"}
        agent_rows.append((item["id"], json.dumps(item, separators=(",", ":"))))
    db.executemany("INSERT INTO runtime_agents VALUES (?,?)", agent_rows)
    work_rows = []
    for number in range(1_475):
        root = "team-0" if number < 117 else roots[1 + number % 3]
        padding = "w" * (3_500 if number < 117 else 7_100)
        item = {"id": f"work-{number:04}", "rootId": root,
                "owner": f"agent-{number % 39:03}", "status": "ready",
                "title": "Sample work", "description": padding,
                "dependencies": [], "results": [], "decisions": []}
        work_rows.append((item["id"], json.dumps(item, separators=(",", ":"))))
    db.executemany("INSERT INTO runtime_work VALUES (?,?)", work_rows)
    return roots


def main():
    db = sqlite3.connect(":memory:")
    db.execute("PRAGMA temp_store=MEMORY")
    roots = make_agent_rows(db)
    agents = [row[0] for row in db.execute(
        "SELECT id FROM runtime_agents WHERE json_extract(record,'$.rootId')=? "
        "AND json_extract(record,'$.deletedAt') IS NULL", (roots[0],))]
    agent_all_bytes = db.execute("SELECT sum(length(record)) FROM runtime_agents").fetchone()[0]
    work_all_bytes = db.execute("SELECT sum(length(record)) FROM runtime_work").fetchone()[0]
    work_team_bytes = db.execute("SELECT sum(length(record)) FROM runtime_work "
                                 "WHERE json_extract(record,'$.rootId')=?", (roots[0],)).fetchone()[0]

    add_task_rows(db, agents)
    old_feed_ms, old_empty = measure(lambda: old_feed_cursor(db, agents))
    task_total_bytes = db.execute("SELECT sum(length(record)) FROM runtime_tasks").fetchone()[0]
    db.execute("CREATE INDEX runtime_task_agent_created_id ON runtime_tasks("
               "json_extract(record,'$.agent'),json_extract(record,'$.created') DESC,id DESC)")
    db.execute("CREATE INDEX runtime_task_agent_updated_id ON runtime_tasks("
               "json_extract(record,'$.agent'),CASE WHEN COALESCE(json_extract(record,'$.finished'),0)>"
               "COALESCE(json_extract(record,'$.created'),0) THEN json_extract(record,'$.finished') "
               "ELSE json_extract(record,'$.created') END,id)")
    cursor_plan = [row[3] for row in db.execute("EXPLAIN QUERY PLAN "
        "SELECT id FROM runtime_tasks WHERE json_extract(record,'$.agent')=? "
        "AND CASE WHEN COALESCE(json_extract(record,'$.finished'),0)>COALESCE(json_extract(record,'$.created'),0) "
        "THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END>? "
        "ORDER BY CASE WHEN COALESCE(json_extract(record,'$.finished'),0)>COALESCE(json_extract(record,'$.created'),0) "
        "THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END,id LIMIT 101",
        (agents[0], 100_000))]
    new_feed_ms, new_empty = measure(lambda: Runtime._workspace_task_rows(
        db, agents, order="updated", cursor={"updated": 100_000, "id": ""}, limit=100))
    old_page = db.execute("SELECT record FROM runtime_tasks WHERE json_extract(record,'$.agent')=? "
                          "ORDER BY json_extract(record,'$.created') DESC,id DESC LIMIT 100", (agents[0],)).fetchall()
    new_page = Runtime._workspace_task_rows(db, [agents[0]], order="created", limit=100)
    feed_page_old_bytes = record_bytes(old_page)
    feed_page_new_bytes = record_bytes(new_page[:100], 2)

    roster = Roster()
    def old_roster():
        selected, bytes_read = [], 0
        for (_id, raw) in db.execute("SELECT id,record FROM runtime_agents"):
            row = json.loads(raw)
            if ((row.get("autoWake") and row.get("status") == "queued")
                    or row.get("inFlight") or row.get("status") in {"running", "starting", "approval"}
                    or row.get("lastContextRepairWait") or row.get("contextRepair")
                    or row.get("restartRecovery") or row.get("browserRecovery")):
                selected.append(row)
                bytes_read += len(raw.encode())
        return selected, bytes_read
    old_agents_ms, (old_agents, old_scheduler_bytes) = measure(old_roster)
    new_scheduler_ms, scheduler_agents = measure(lambda: roster.scheduler_agents(db))
    scheduler_ids = [agent["id"] for agent in scheduler_agents]
    scheduler_bytes = record_bytes(db.execute(
        "SELECT record FROM runtime_agents WHERE id IN (" + ",".join("?" for _ in scheduler_ids) + ")",
        scheduler_ids).fetchall()) if scheduler_ids else 0
    team_agents_ms, scoped_agents = measure(lambda: roster.team_agents(db, roots[0]))
    team_agent_rows = db.execute("SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId')=? "
                                 "AND json_extract(record,'$.deletedAt') IS NULL", (roots[0],)).fetchall()

    now = time.time()
    def old_capacity_poll():
        return [json.loads(row[0]) for row in db.execute(
            "SELECT record FROM runtime_agents WHERE json_extract(record,'$.capacityRetry') IS NOT NULL")]
    old_capacity_ms, old_capacity = measure(old_capacity_poll)
    old_capacity_bytes = sum(len(json.dumps(row, separators=(",", ":")).encode())
                             for row in old_capacity)
    db.execute("CREATE INDEX runtime_agent_capacity_retry_state_due_v2 ON runtime_agents("
               "json_extract(record,'$.capacityRetry.status'),json_extract(record,'$.capacityRetry.dueAt'))")
    new_capacity_ms, new_capacity = measure(lambda: [json.loads(row[0]) for row in db.execute(
        "SELECT record FROM runtime_agents WHERE "
        "json_extract(record,'$.capacityRetry.status')='scheduled' "
        "AND json_extract(record,'$.capacityRetry.dueAt')<=? UNION ALL "
        "SELECT record FROM runtime_agents WHERE "
        "json_extract(record,'$.capacityRetry.status')='failed' "
        "AND json_extract(record,'$.capacityRetry.acceptedTurnId') IS NULL "
        "AND json_extract(record,'$.capacityRetry.reason') LIKE 'Context repair waits for %'", (now,))])
    new_capacity_bytes = record_bytes(db.execute(
        "SELECT record FROM runtime_agents WHERE "
        "json_extract(record,'$.capacityRetry.status')='scheduled' "
        "AND json_extract(record,'$.capacityRetry.dueAt')<=?", (now,)).fetchall())
    capacity_plan = [row[3] for row in db.execute("EXPLAIN QUERY PLAN SELECT record FROM runtime_agents WHERE "
        "json_extract(record,'$.capacityRetry.status')='scheduled' "
        "AND json_extract(record,'$.capacityRetry.dueAt')<=? UNION ALL "
        "SELECT record FROM runtime_agents WHERE "
        "json_extract(record,'$.capacityRetry.status')='failed' "
        "AND json_extract(record,'$.capacityRetry.acceptedTurnId') IS NULL "
        "AND json_extract(record,'$.capacityRetry.reason') LIKE 'Context repair waits for %'", (now,))]

    old_work_ms, old_work = measure(lambda: [json.loads(row[0])
        for row in db.execute("SELECT record FROM runtime_work")])
    new_work_ms, team_work = measure(lambda: Runtime.work_records(db, roots[0]))
    get_ms, get_task = measure(lambda: Runtime.work_by_id(db, "work-0001", roots[0]))
    output = {
        "sample": "synthetic SQLite, in-memory, rows sized to review observations",
        "feed": {"rows": 72_902, "oldZeroCursorMsMedian": old_feed_ms,
                 "newZeroCursorMsMedian": new_feed_ms,
                 "oldZeroCursorRows": len(old_empty), "newZeroCursorRows": len(new_empty),
                 "oldCandidateBytes": task_total_bytes, "newZeroCursorBytes": 0,
                 "oldPageBytes": feed_page_old_bytes, "newSummaryPageBytes": feed_page_new_bytes,
                 "newCursorPlan": cursor_plan},
        "scheduler": {"oldAgentRows": len(old_agents), "newSelectedRows": len(scheduler_agents),
                      "oldAgentBytes": old_scheduler_bytes, "newCandidateBytes": scheduler_bytes,
                      "oldRawSelectMsMedian": old_agents_ms, "newRosterMsMedian": new_scheduler_ms,
                      "newRosterDecoded": len(scheduler_agents)},
        "capacityRetry": {"oldRetryRows": len(old_capacity), "newDueRows": len(new_capacity),
                          "oldRetryBytes": old_capacity_bytes, "newDueBytes": new_capacity_bytes,
                          "oldPollMsMedian": old_capacity_ms, "newDuePollMsMedian": new_capacity_ms,
                          "newQueryPlan": capacity_plan},
        "spawn": {"oldGlobalAgentBytes": agent_all_bytes, "newTeamAgentBytes": record_bytes(team_agent_rows),
                  "oldGlobalRows": 308, "newTeamRows": len(team_agent_rows),
                  "newTeamQueryMsMedian": team_agents_ms, "teamAgentsDecoded": len(scoped_agents)},
        "work": {"rows": len(old_work), "oldGlobalBytes": work_all_bytes,
                 "newTeamRows": len(team_work), "newTeamBytes": work_team_bytes,
                 "newGetBytes": db.execute("SELECT length(record) FROM runtime_work WHERE id=?",
                                            ("work-0001",)).fetchone()[0],
                 "oldGlobalMsMedian": old_work_ms,
                 "newTeamMsMedian": new_work_ms, "newGetMsMedian": get_ms},
    }
    print(json.dumps(output, indent=2))
    db.close()


if __name__ == "__main__":
    main()
