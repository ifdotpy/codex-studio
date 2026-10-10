"""Shared window selectors for snapshot and entity-sync runtime collections."""
import json
import logging
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import sqlite3
    from codex_records import JsonObject


TASK_ARCHIVE_WINDOW = 100
EVENT_WINDOW = 200
MONITOR_TERMINAL_WINDOW = 100
MONITOR_FINAL_WINDOW = 100
MONITOR_TERMINAL_STATUSES = ("completed", "failed", "cancelled", "lost")
_REPORTED_BAD_ROWS: set[tuple[str, str]] = set()


def _decode_records(collection: str, rows: list[tuple[str]]) -> list["JsonObject"]:
    records: list["JsonObject"] = []
    for (raw,) in rows:
        try:
            record = json.loads(raw)
            if not isinstance(record, dict):
                raise TypeError("record is not an object")
        except (TypeError, ValueError) as error:
            kind = type(error).__name__
            if (collection, kind) not in _REPORTED_BAD_ROWS:
                _REPORTED_BAD_ROWS.add((collection, kind))
                logging.getLogger(__name__).warning(
                    "Skipping malformed runtime row collection=%s error=%s", collection, kind)
            continue
        records.append(record)
    return records
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
    return _decode_records("task", rows)


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
    records = _decode_records("monitor", rows)
    owners = sorted({
        owner for record in records if isinstance(owner := record.get("agent"), str)
    })
    active_agents: set[str] = set()
    for start in range(0, len(owners), 500):
        batch = owners[start:start + 500]
        placeholders = ",".join("?" for _ in batch)
        active_agents.update(row[0] for row in db.execute(
            "SELECT id FROM runtime_agents WHERE id IN (" + placeholders + ") "
            "AND json_extract(record,'$.deletedAt') IS NULL" + active_agent_scope,
            (*batch, *params),
        ))
    return [record for record in records if record.get("agent") in active_agents]


def event_records(db: "sqlite3.Connection") -> list[dict[str, object]]:
    rows = db.execute(
        f"""SELECT id,agent,kind,status,created,error FROM runtime_events
            ORDER BY created DESC,id LIMIT {EVENT_WINDOW}"""
    ).fetchall()
    return [dict(zip(("id", "agent", "kind", "status", "created", "error"), row)) for row in rows]
