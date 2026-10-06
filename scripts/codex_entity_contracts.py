"""Shared window selectors for snapshot and entity-sync runtime collections."""
import json
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import sqlite3
    from codex_records import JsonObject


TASK_ARCHIVE_WINDOW = 100
EVENT_WINDOW = 200
MONITOR_TERMINAL_WINDOW = 100
MONITOR_FINAL_WINDOW = 100
MONITOR_TERMINAL_STATUSES = ("completed", "failed", "cancelled", "lost")
ACTIVE_MONITOR_STATUSES = ("running", "starting", "approval")


def task_records(db: "sqlite3.Connection", root: str | None = None) -> list["JsonObject"]:
    scope = "" if root is None else " AND json_extract(a.record,'$.rootId')=?"
    params = () if root is None else (root, root)
    rows = db.execute(
        f"""SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
            ON json_extract(t.record,'$.agent')=a.id
            WHERE json_extract(a.record,'$.deletedAt') IS NULL
            {scope} AND json_extract(t.record,'$.status')='running'
            UNION ALL SELECT record FROM (SELECT t.record FROM runtime_tasks t JOIN runtime_agents a
            ON json_extract(t.record,'$.agent')=a.id WHERE json_extract(a.record,'$.deletedAt') IS NULL
            {scope} AND json_extract(t.record,'$.status')!='running'
            ORDER BY json_extract(t.record,'$.created') DESC LIMIT {TASK_ARCHIVE_WINDOW})""",
        params,
    ).fetchall()
    return [json.loads(row[0]) for row in rows]


def monitor_records(db: "sqlite3.Connection", root: str | None = None) -> list["JsonObject"]:
    active_agent_scope = "" if root is None else " AND json_extract(record,'$.rootId')=?"
    scope = "" if root is None else """ AND json_extract(record,'$.agent') IN (
        SELECT id FROM runtime_agents WHERE json_extract(record,'$.rootId')=?
        AND json_extract(record,'$.deletedAt') IS NULL)"""
    params = () if root is None else (root,)
    terminal = " UNION ALL ".join(
        f"""SELECT * FROM (SELECT record, json_extract(record,'$.created') AS created FROM runtime_monitors
        WHERE json_extract(record,'$.status')='{status}' {scope}
        ORDER BY json_extract(record,'$.created') DESC LIMIT {MONITOR_TERMINAL_WINDOW})"""
        for status in MONITOR_TERMINAL_STATUSES)
    rows = db.execute(
        f"""SELECT record FROM runtime_monitors WHERE json_extract(record,'$.status') IN ('running','starting','approval') {scope}
          UNION ALL SELECT record FROM (SELECT record, created FROM ({terminal}) ORDER BY created DESC LIMIT {MONITOR_FINAL_WINDOW})""",
        params * (1 + len(MONITOR_TERMINAL_STATUSES)),
    ).fetchall()
    records = [json.loads(row[0]) for row in rows]
    active_agents = {row[0] for row in db.execute(
        "SELECT id FROM runtime_agents WHERE json_extract(record,'$.deletedAt') IS NULL" + active_agent_scope,
        params,
    )}
    return [record for record in records if record.get("agent") in active_agents]


def event_records(db: "sqlite3.Connection") -> list[dict[str, object]]:
    rows = db.execute(
        f"""SELECT id,agent,kind,status,created,error FROM runtime_events
            ORDER BY created DESC,id LIMIT {EVENT_WINDOW}"""
    ).fetchall()
    return [dict(zip(("id", "agent", "kind", "status", "created", "error"), row)) for row in rows]
