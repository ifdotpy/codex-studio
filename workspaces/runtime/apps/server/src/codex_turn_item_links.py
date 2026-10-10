"""Index terminal turn items without a blocking scan of runtime history.

Persistent triggers cover writes from every connection. Legacy rows are read
outside a writer transaction, then linked in small resumable commits. Until the
initial range is complete, terminal callbacks retain their original query.
"""

from __future__ import annotations

from dataclasses import dataclass
import sqlite3
import time
import uuid


LINKS = "runtime_turn_item_links_v1"
STATE = "runtime_turn_item_link_state_v1"
INDEX = "runtime_turn_item_link_scope_v1"
ROW_LIMIT = 512
BYTE_LIMIT = 512 * 1024
SECONDS_LIMIT = .02


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
        schema[name] = f"CREATE TRIGGER {name} AFTER {action.upper()} ON runtime_items{when} BEGIN {statements} END"
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


@dataclass(frozen=True)
class Coverage:
    epoch: str
    source_rootpage: int
    cursor: int | None
    target: int | None
    complete: bool
    scanned_rows: int
    source_bytes: int


def _coverage(db: sqlite3.Connection) -> Coverage | None:
    schema = _schema()
    existing = _existing(db)
    if any(name not in existing or _normalize(existing[name]) != _normalize(sql)
           for name, sql in schema.items()):
        return None
    row = db.execute(f"SELECT epoch,source_rootpage,cursor,target,complete,scanned_rows,source_bytes FROM {STATE} WHERE id=1").fetchone()
    if (not row or not isinstance(row[0], str) or not row[0]
            or type(row[1]) is not int or row[1] != _source_rootpage(db)
            or any(value is not None and type(value) is not int for value in row[2:4])
            or row[4] not in (0, 1) or (row[4] and row[2] != row[3])
            or any(type(value) is not int or value < 0 for value in row[5:7])):
        return None
    return Coverage(row[0], row[1], row[2], row[3], bool(row[4]), row[5], row[6])


def ensure_tables(db: sqlite3.Connection) -> bool:
    """Install metadata and triggers only. Never scan or rebuild an old index."""
    if _coverage(db) is not None:
        return True
    if db.in_transaction:
        # The initial range must share the commit that installs its triggers.
        raise RuntimeError("Turn item links must install outside an existing transaction")
    schema = _schema()
    previous_timeout = int(db.execute("PRAGMA busy_timeout").fetchone()[0])
    db.execute("PRAGMA busy_timeout=40")
    begun = False
    try:
        db.execute("BEGIN IMMEDIATE")
        begun = True
        existing = _existing(db)
        rootpage = _source_rootpage(db)
        if rootpage is None:
            db.rollback()
            return False
        for name in (LINKS, STATE, INDEX):
            if name in existing and _normalize(existing[name]) != _normalize(schema[name]):
                db.rollback()
                return False
        if LINKS in existing and INDEX not in existing:
            # Rebuilding an index on old links could hold the writer for minutes.
            db.rollback()
            return False
        for name in (LINKS, STATE, INDEX):
            if name not in existing:
                db.execute(schema[name])
        for name, sql in schema.items():
            if name in (LINKS, STATE, INDEX):
                continue
            if name in existing and _normalize(existing[name]) != _normalize(sql):
                db.rollback()
                return False
        if _coverage(db) is None:
            target = db.execute("SELECT MAX(rowid) FROM runtime_items").fetchone()[0]
            db.execute(f"INSERT INTO {STATE} VALUES(1,?,?,NULL,?,0,0,0,?) ON CONFLICT(id) DO UPDATE SET "
                       "epoch=excluded.epoch,source_rootpage=excluded.source_rootpage,cursor=NULL,"
                       "target=excluded.target,complete=0,scanned_rows=0,source_bytes=0,updated=excluded.updated",
                       (uuid.uuid4().hex, rootpage, target, time.time()))
            for name, sql in schema.items():
                if name not in (LINKS, STATE, INDEX) and name not in existing:
                    db.execute(sql)
        db.commit()
        return _coverage(db) is not None
    except sqlite3.OperationalError as error:
        if begun:
            db.rollback()
        if "locked" in str(error).lower() or "busy" in str(error).lower():
            return False
        raise
    except BaseException:
        if begun:
            db.rollback()
        raise
    finally:
        db.execute("PRAGMA busy_timeout=" + str(previous_timeout))


@dataclass(frozen=True)
class Batch:
    rows: int = 0
    source_bytes: int = 0
    complete: bool = False
    busy: bool = False
    writer_ms: float = 0
    oversized_row: bool = False


def backfill_batch(db: sqlite3.Connection, *, row_limit: int = ROW_LIMIT,
                   byte_limit: int = BYTE_LIMIT, seconds_limit: float = SECONDS_LIMIT) -> Batch:
    """Read one bounded row at a time, then commit links and cursor.

    A source row is indivisible. One oversized row can exceed the read byte/time
    budget, but its JSON decode does not hold a writer lock. Byte counters cover
    consumed records, not physical I/O. The next size probe can read one body
    before we know that it does not fit. The writer copies
    only IDs and scalar fields. Concurrent source writes take priority over the
    staged legacy value through the persistent triggers and epoch guard.
    """
    if row_limit <= 0 or byte_limit <= 0 or seconds_limit <= 0:
        raise ValueError("Turn item link batch limits must be positive")
    if db.in_transaction:
        raise RuntimeError("Turn item link backfill requires its own transaction")
    if not ensure_tables(db):
        return Batch()
    coverage = _coverage(db)
    if coverage is None or coverage.complete:
        return Batch(complete=bool(coverage and coverage.complete))
    start = time.monotonic()
    staged: list[tuple[str, str, object, float, str]] = []
    source_bytes = 0
    scanned_rows = 0
    last = coverage.cursor
    complete = False
    if coverage.target is None:
        complete = True
    else:
        for _ in range(row_limit):
            if scanned_rows and (source_bytes >= byte_limit
                                 or time.monotonic() - start >= seconds_limit):
                break
            query = "SELECT rowid,id,length(CAST(record AS BLOB)) FROM runtime_items WHERE rowid<=?"
            args: tuple[int, ...] = (coverage.target,)
            if last is not None:
                query += " AND rowid>?"
                args += (last,)
            # CAST can load the record body inside SQLite. LIMIT 1 prevents a
            # page of large bodies from loading before the next budget check.
            cursor = db.execute(query + " ORDER BY rowid LIMIT 1", args)
            try:
                metadata = cursor.fetchone()
            finally:
                cursor.close()
            if metadata is None:
                # New and changed rows outside this captured range have already
                # passed through the triggers. Only an empty range proves done.
                complete = True
                last = coverage.target
                break
            source_rowid, source_id, size = metadata
            if scanned_rows and source_bytes + size > byte_limit:
                break
            cursor = db.execute("SELECT id,agent,json_extract(record,'$.turnId'),created "
                                "FROM runtime_items WHERE rowid=? AND id=?", (source_rowid, source_id))
            try:
                row = cursor.fetchone()
            finally:
                cursor.close()
            if row:
                staged.append((row[0], row[1], row[2], row[3], coverage.epoch))
            source_bytes += size
            last = source_rowid
            scanned_rows += 1
    previous_timeout = int(db.execute("PRAGMA busy_timeout").fetchone()[0])
    db.execute("PRAGMA busy_timeout=40")
    started = time.perf_counter()
    try:
        db.execute("BEGIN IMMEDIATE")
        if _coverage(db) != coverage:
            db.rollback()
            return Batch()
        db.executemany(f"INSERT INTO {LINKS}(id,agent,turn_id,created,epoch) VALUES(?,?,?,?,?) "
                       "ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,"
                       "created=excluded.created,epoch=excluded.epoch "
                       f"WHERE {LINKS}.epoch IS NOT excluded.epoch", staged)
        db.execute(f"UPDATE {STATE} SET cursor=?,complete=?,scanned_rows=scanned_rows+?,"
                   "source_bytes=source_bytes+?,updated=? WHERE id=1",
                   (last, int(complete), scanned_rows, source_bytes, time.time()))
        db.commit()
        return Batch(len(staged), source_bytes, complete, False,
                     (time.perf_counter() - started) * 1000, source_bytes > byte_limit)
    except sqlite3.OperationalError as error:
        db.rollback()
        if "locked" in str(error).lower() or "busy" in str(error).lower():
            return Batch(busy=True, writer_ms=(time.perf_counter() - started) * 1000)
        raise
    except BaseException:
        db.rollback()
        raise
    finally:
        db.execute("PRAGMA busy_timeout=" + str(previous_timeout))


def update_turn_status(db: sqlite3.Connection, agent: str, turn_id: str | None,
                       status: str, *, since: float | None = None) -> None:
    """Preserve the exact old predicate until all legacy rows have coverage."""
    coverage = _coverage(db)
    where = "agent=? AND json_extract(record,'$.turnId')=?"
    args: tuple[object, ...] = (status, agent, turn_id)
    if since is not None:
        where += " AND created>=?"
        args += (since,)
    if coverage is not None and coverage.complete:
        source_where = "source.agent=? AND json_extract(source.record,'$.turnId')=?"
        if since is not None:
            source_where += " AND source.created>=?"
        where = (f"id IN (SELECT link.id FROM {LINKS} AS link INDEXED BY {INDEX} "
                 "CROSS JOIN runtime_items AS source ON source.id=link.id "
                 "WHERE link.agent=? AND link.turn_id=? AND link.epoch=?"
                 + (" AND link.created>=?" if since is not None else "")
                 + " AND " + source_where + ")")
        args = ((status, agent, turn_id, coverage.epoch)
                + ((since,) if since is not None else ()) + args[1:])
    db.execute("UPDATE runtime_items SET record=json_set(record,'$.turnStatus',?) WHERE " + where, args)
