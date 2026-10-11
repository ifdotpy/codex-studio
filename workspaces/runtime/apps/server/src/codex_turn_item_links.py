"""Index terminal turn items after the one-time coverage migration."""

from __future__ import annotations

import sqlite3
import time
import uuid


LINKS = "runtime_turn_item_links_v1"
STATE = "runtime_turn_item_link_state_v1"
INDEX = "runtime_turn_item_link_scope_v1"


def _schema() -> dict[str, str]:
    schema = {
        LINKS: f"CREATE TABLE {LINKS} (id TEXT PRIMARY KEY,agent TEXT NOT NULL,"
            "turn_id,created REAL NOT NULL,epoch TEXT NOT NULL)",
        STATE: f"CREATE TABLE {STATE} (id INTEGER PRIMARY KEY CHECK(id=1),"
            "epoch TEXT NOT NULL,source_rootpage INTEGER NOT NULL,cursor INTEGER,"
            "target INTEGER,complete INTEGER NOT NULL CHECK(complete IN (0,1)),"
            "scanned_rows INTEGER NOT NULL,source_bytes INTEGER NOT NULL,updated REAL NOT NULL)",
        INDEX: f"CREATE INDEX {INDEX} ON {LINKS}(agent,turn_id,epoch,created,id)",
    }
    upsert = (f"INSERT INTO {LINKS}(id,agent,turn_id,created,epoch) VALUES "
              "(NEW.id,NEW.agent,json_extract(NEW.record,'$.turnId'),NEW.created,"
              f"COALESCE((SELECT epoch FROM {STATE} WHERE id=1),'')) "
              "ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,"
              "created=excluded.created,epoch=excluded.epoch "
              f"WHERE {LINKS}.agent IS NOT excluded.agent OR {LINKS}.turn_id IS NOT excluded.turn_id "
              f"OR {LINKS}.created IS NOT excluded.created OR {LINKS}.epoch IS NOT excluded.epoch;")
    for action in ("insert", "update", "delete"):
        name = "runtime_turn_item_link_" + action + "_v1"
        when = (" WHEN OLD.id IS NOT NEW.id OR OLD.agent IS NOT NEW.agent "
                "OR OLD.record IS NOT NEW.record OR OLD.created IS NOT NEW.created "
                "OR OLD.rowid IS NOT NEW.rowid"
                if action == "update" else "")
        statements = ""
        if action in {"update", "delete"}:
            statements += f"DELETE FROM {LINKS} WHERE id=OLD.id"
            if action == "update":
                statements += " AND OLD.id IS NOT NEW.id"
            statements += "; "
        if action != "delete":
            statements += upsert
        schema[name] = (f"CREATE TRIGGER {name} AFTER {action.upper()} ON runtime_items{when} "
                        f"BEGIN {statements} END")
    return schema


def _normalize(sql: str) -> str:
    return " ".join(sql.split())


def _existing(db: sqlite3.Connection) -> dict[str, str]:
    names = tuple(_schema())
    return {row[0]: row[1] for row in db.execute(
        "SELECT name,sql FROM sqlite_master WHERE name IN (" + ",".join("?" * len(names)) + ")", names)}


def _source_rootpage(db: sqlite3.Connection) -> int | None:
    row = db.execute("SELECT rootpage FROM sqlite_master WHERE type='table' AND name='runtime_items'").fetchone()
    return int(row[0]) if row else None


def _complete(db: sqlite3.Connection, existing: dict[str, str]) -> bool:
    schema = _schema()
    if any(name not in existing or _normalize(existing[name]) != _normalize(sql)
           for name, sql in schema.items()):
        return False
    row = db.execute(f"SELECT epoch,cursor,target,complete FROM {STATE} WHERE id=1").fetchone()
    return bool(row and isinstance(row[0], str) and row[0]
                and row[1] == row[2] and row[3] == 1)


def ensure_tables(db: sqlite3.Connection) -> bool:
    """Install indexed writes for a fresh store; refuse uncovered legacy rows."""
    if db.in_transaction:
        raise RuntimeError("Turn item links must initialize outside an existing transaction")
    schema = _schema()
    existing = _existing(db)
    if existing:
        if _complete(db, existing):
            return True
        raise RuntimeError(
            "This state has unfinished turn-item-links v1 data; open it once with Studio 1.0.0 "
            "or earlier to finish the migration, then upgrade."
        )
    rootpage = _source_rootpage(db)
    if rootpage is None:
        return False
    if db.execute("SELECT 1 FROM runtime_items LIMIT 1").fetchone():
        raise RuntimeError(
            "This state has legacy runtime_items without turn-item-links v1 coverage; open it once with "
            "Studio 1.0.0 or earlier to finish the migration, then upgrade."
        )
    epoch = uuid.uuid4().hex
    try:
        db.execute("BEGIN IMMEDIATE")
        for name, sql in schema.items():
            db.execute(sql)
        db.execute(f"INSERT INTO {STATE} VALUES (1,?,?,NULL,NULL,1,0,0,?)",
                   (epoch, rootpage, time.time()))
        db.commit()
    except BaseException:
        db.rollback()
        raise
    return True


def update_turn_status(db: sqlite3.Connection, agent: str, turn_id: str | None,
                       status: str, *, since: float | None = None) -> None:
    """Update statuses through the complete, trigger-maintained turn index."""
    coverage = db.execute(f"SELECT epoch,complete FROM {STATE} WHERE id=1").fetchone()
    if not coverage or coverage[1] != 1:
        raise RuntimeError("Turn item link coverage is incomplete")
    where = (f"id IN (SELECT link.id FROM {LINKS} AS link INDEXED BY {INDEX} "
             "CROSS JOIN runtime_items AS source ON source.id=link.id "
             "WHERE link.agent=? AND link.turn_id=? AND link.epoch=? "
             "AND source.agent=? AND json_extract(source.record,'$.turnId')=?)")
    args: tuple[object, ...] = (status, agent, turn_id, coverage[0], agent, turn_id)
    if since is not None:
        where = where[:-1] + " AND link.created>=? AND source.created>=?)"
        args += (since, since)
    db.execute("UPDATE runtime_items SET record=json_set(record,'$.turnStatus',?) WHERE " + where, args)
