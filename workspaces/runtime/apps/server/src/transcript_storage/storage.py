"""SQLite persistence for transcript bodies and their derived FTS index."""

import time
import sqlite3


STREAM_INDEX_DELAY_SECONDS = 2.0
INDEX_BATCH_SIZE = 32
ADDRESS_BATCH_SIZE = 8192
ITEM_BACKFILL_BATCH_SIZE = 128
ITEM_BACKFILL_TEXT_BUDGET_BYTES = 2 * 1024 * 1024


def initialize(db):
    """Create schema only; never scan historical rows during startup."""
    prior_address_map = db.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_search_rows'"
    ).fetchone() is not None
    db.executescript("""
        CREATE TABLE IF NOT EXISTS runtime_item_bodies (
            id TEXT PRIMARY KEY, body TEXT NOT NULL, version INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS runtime_search_pending (
            id TEXT PRIMARY KEY, version INTEGER NOT NULL, due_at REAL NOT NULL
        );
        CREATE INDEX IF NOT EXISTS runtime_search_pending_due ON runtime_search_pending(due_at,id);
        CREATE VIRTUAL TABLE IF NOT EXISTS runtime_search USING
            fts5(id UNINDEXED, agent UNINDEXED, kind UNINDEXED, body, tokenize='unicode61');
        CREATE TABLE IF NOT EXISTS runtime_search_indexed (id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS runtime_search_partial (id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS runtime_search_rows (
            id TEXT PRIMARY KEY, search_rowid INTEGER NOT NULL UNIQUE
        );
        CREATE TABLE IF NOT EXISTS runtime_search_address_cursor (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), rowid INTEGER NOT NULL,
            done INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS runtime_search_deletions (id TEXT PRIMARY KEY);
        CREATE TABLE IF NOT EXISTS runtime_search_item_cursor (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), rowid INTEGER NOT NULL,
            done INTEGER NOT NULL DEFAULT 0
        );
    """)
    db.execute("INSERT OR IGNORE INTO runtime_search_address_cursor(singleton,rowid,done) VALUES (1,0,?)",
               (int(prior_address_map),))
    db.execute("INSERT OR IGNORE INTO runtime_search_item_cursor(singleton,rowid,done) VALUES (1,0,0)")


def persist(db, key, agent, kind, body, *, streaming=False, now=None):
    """Commit the complete body and replace its desired index version."""
    now = time.time() if now is None else now
    row = db.execute("SELECT version FROM runtime_item_bodies WHERE id=?", (key,)).fetchone()
    version = (row[0] if row else 0) + 1
    db.execute("INSERT INTO runtime_item_bodies VALUES (?,?,?) "
               "ON CONFLICT(id) DO UPDATE SET body=excluded.body,version=excluded.version",
               (key, body, version))
    due = now + STREAM_INDEX_DELAY_SECONDS if streaming else now
    db.execute("INSERT INTO runtime_search_pending VALUES (?,?,?) "
               "ON CONFLICT(id) DO UPDATE SET version=excluded.version,due_at="
               "CASE WHEN ? THEN MIN(runtime_search_pending.due_at,excluded.due_at) ELSE excluded.due_at END",
               (key, version, due, int(streaming)))
    if not streaming:
        db.execute("SAVEPOINT transcript_final_index")
        try:
            index_item(db, key, agent, kind, body)
            db.execute("DELETE FROM runtime_search_pending WHERE id=? AND version=?", (key, version))
            db.execute("RELEASE transcript_final_index")
        except Exception:
            # Keep body + pending row durable if this derived write fails.
            db.execute("ROLLBACK TO transcript_final_index")
            db.execute("RELEASE transcript_final_index")


def body(db, key, fallback=None, *, agent=None):
    """Read authoritative text, then legacy FTS text, then supplied excerpt."""
    if agent is not None:
        owner = db.execute("SELECT 1 FROM runtime_items WHERE id=? AND agent=?", (key, agent)).fetchone()
        if owner is None:
            raise ValueError("Transcript item does not belong to the requested agent")
    try:
        row = db.execute("SELECT body FROM runtime_item_bodies WHERE id=?", (key,)).fetchone()
    except sqlite3.OperationalError as error:
        if "no such table" not in str(error):
            raise
        row = None
    if row is not None:
        return row[0]
    item = db.execute("SELECT record FROM runtime_items WHERE id=?", (key,)).fetchone()
    if item is not None:
        import json
        item = json.loads(item[0])
        if item.get("truncated") and fallback == item.get("text", ""):
            fallback = None
    else:
        item = None
    try:
        partial = db.execute("SELECT 1 FROM runtime_search_partial WHERE id=?", (key,)).fetchone()
    except sqlite3.OperationalError as error:
        if "no such table" not in str(error):
            raise
        partial = None
    if partial:
        return None
    # Legacy FTS is read-only compatibility for items written before body storage.
    try:
        address = db.execute("SELECT search_rowid FROM runtime_search_rows WHERE id=?", (key,)).fetchone()
    except sqlite3.OperationalError as error:
        if "no such table" not in str(error):
            raise
        address = None
    if address:
        try:
            row = db.execute("SELECT body FROM runtime_search WHERE rowid=?", (address[0],)).fetchone()
        except sqlite3.OperationalError as error:
            if "no such table" not in str(error):
                raise
            row = None
    else:
        # Compatibility for one requested legacy item while bounded address
        # migration has not reached it. Current items hit runtime_item_bodies
        # above and never scan FTS. Ownership was checked against runtime_items.
        try:
            row = db.execute("SELECT body FROM runtime_search WHERE id=? LIMIT 1", (key,)).fetchone()
        except sqlite3.OperationalError as error:
            if not any(message in str(error) for message in ("no such table", "no such column")):
                raise
            row = None
    if row is None:
        return fallback
    if item is not None and item.get("truncated") and row[0] == item.get("text", ""):
        return None
    return row[0]


def index_item(db, key, agent, kind, text):
    """Replace one derived FTS row and its address without touching body data."""
    row = db.execute("SELECT search_rowid FROM runtime_search_rows WHERE id=?", (key,)).fetchone()
    if row:
        db.execute("DELETE FROM runtime_search WHERE rowid=?", (row[0],))
    else:
        # Only an item marked as legacy indexed may need this one-time lookup.
        # Brand-new item IDs take the no-scan path while bounded migration runs.
        known = db.execute("SELECT 1 FROM runtime_search_indexed WHERE id=?", (key,)).fetchone()
        if known:
            db.execute("DELETE FROM runtime_search WHERE id=?", (key,))
    cursor = db.execute("INSERT INTO runtime_search(id,agent,kind,body) VALUES (?,?,?,?)",
                        (key, agent, kind, text))
    db.execute("INSERT INTO runtime_search_rows VALUES (?,?) ON CONFLICT(id) DO UPDATE "
               "SET search_rowid=excluded.search_rowid", (key, cursor.lastrowid))
    db.execute("INSERT OR IGNORE INTO runtime_search_indexed VALUES (?)", (key,))
    db.execute("DELETE FROM runtime_search_partial WHERE id=?", (key,))
    db.execute("DELETE FROM runtime_search_deletions WHERE id=?", (key,))


def ensure_indexed(db, key, agent, kind, text):
    """Index an item only when neither the address map nor legacy marker knows it."""
    if db.execute("SELECT 1 FROM runtime_search_rows WHERE id=?", (key,)).fetchone():
        return False
    if db.execute("SELECT 1 FROM runtime_search_indexed WHERE id=?", (key,)).fetchone():
        return False
    index_item(db, key, agent, kind, text)
    return True


def drain(db, limit=INDEX_BATCH_SIZE, *, now=None, force=False):
    """Index up to limit due rows, always using their latest committed body."""
    now = time.time() if now is None else now
    due_clause = "" if force else "WHERE due_at<=? "
    params = (max(1, int(limit)),) if force else (now, max(1, int(limit)))
    pending = db.execute("SELECT id,version FROM runtime_search_pending " + due_clause +
                         "ORDER BY due_at,id LIMIT ?", params).fetchall()
    for entry in pending:
        item = db.execute("SELECT i.agent,i.record,b.body,b.version FROM runtime_items i "
                          "JOIN runtime_item_bodies b ON b.id=i.id WHERE i.id=?", (entry[0],)).fetchone()
        if item is None:
            db.execute("DELETE FROM runtime_search_pending WHERE id=? AND version=?", (entry[0], entry[1]))
            continue
        import json
        record = json.loads(item[1])
        index_item(db, entry[0], item[0], record.get("title", ""), item[2])
        db.execute("DELETE FROM runtime_search_pending WHERE id=? AND version=?", (entry[0], entry[1]))
    return len(pending)


def has_pending(db, agents=None):
    """Whether visible transcript rows remain outside the search snapshot."""
    where = "json_extract(i.record,'$.afterRestore') IS NULL"
    params = []
    if agents is not None:
        agents = list(agents)
        if not agents:
            return False
        where += " AND i.agent IN (" + ",".join("?" for _ in agents) + ")"
        params.extend(agents)
    if db.execute(
        "SELECT 1 FROM runtime_search_address_cursor WHERE singleton=1 AND done=0 "
        "UNION ALL SELECT 1 FROM runtime_search_item_cursor WHERE singleton=1 AND done=0 LIMIT 1"
    ).fetchone():
        return True
    return db.execute(
        "SELECT 1 FROM runtime_search_pending p JOIN runtime_items i ON i.id=p.id WHERE " + where + " LIMIT 1",
        params,
    ).fetchone() is not None


def has_partial(db, agents=None):
    """Whether visible transcript rows have only excerpt text available."""
    where = "json_extract(i.record,'$.afterRestore') IS NULL"
    params = []
    if agents is not None:
        agents = list(agents)
        if not agents:
            return False
        where += " AND i.agent IN (" + ",".join("?" for _ in agents) + ")"
        params.extend(agents)
    return db.execute(
        "SELECT 1 FROM runtime_search_partial p JOIN runtime_items i ON i.id=p.id WHERE "
        + where + " LIMIT 1", params,
    ).fetchone() is not None


def backfill_addresses(db, limit=ADDRESS_BATCH_SIZE):
    """Build legacy FTS row addresses incrementally, away from startup."""
    state = db.execute("SELECT rowid,done FROM runtime_search_address_cursor WHERE singleton=1").fetchone()
    if state[1]:
        return 0
    cursor = state[0]
    rows = db.execute("SELECT rowid,id FROM runtime_search WHERE rowid>? ORDER BY rowid LIMIT ?",
                      (cursor, max(1, int(limit)))).fetchall()
    if rows:
        for row in rows:
            deleted = db.execute("SELECT 1 FROM runtime_search_deletions WHERE id=?", (row[1],)).fetchone()
            if deleted:
                db.execute("DELETE FROM runtime_search WHERE rowid=?", (row[0],))
            else:
                address = db.execute("SELECT search_rowid FROM runtime_search_rows WHERE id=?", (row[1],)).fetchone()
                if address and address[0] != row[0]:
                    db.execute("DELETE FROM runtime_search WHERE rowid=?", (row[0],))
                elif not address:
                    db.execute("INSERT INTO runtime_search_rows VALUES (?,?)", (row[1], row[0]))
        done = int(len(rows) < max(1, int(limit)))
        db.execute("UPDATE runtime_search_address_cursor SET rowid=?,done=? WHERE singleton=1", (rows[-1][0], done))
        if done:
            db.execute("DELETE FROM runtime_search_deletions")
    else:
        db.execute("UPDATE runtime_search_address_cursor SET done=1 WHERE singleton=1")
        db.execute("DELETE FROM runtime_search_deletions")
    return len(rows)


def backfill_items(db, limit=ITEM_BACKFILL_BATCH_SIZE):
    """Repair missing legacy FTS rows in bounded rowid pages after address migration."""
    address_state = db.execute("SELECT done FROM runtime_search_address_cursor WHERE singleton=1").fetchone()
    if address_state is not None and not address_state[0]:
        return 0
    state = db.execute("SELECT rowid,done FROM runtime_search_item_cursor WHERE singleton=1").fetchone()
    if state is None or state[1]:
        return 0
    limit = max(1, int(limit))
    try:
        rows = db.execute(
            "SELECT i.rowid,i.id,i.agent,i.record,a.id AS addressed,s.id AS indexed,p.id AS pending,"
            "a.search_rowid "
            "FROM runtime_items i LEFT JOIN runtime_search_rows a ON a.id=i.id "
            "LEFT JOIN runtime_search_indexed s ON s.id=i.id "
            "LEFT JOIN runtime_search_pending p ON p.id=i.id "
            "WHERE i.rowid>? ORDER BY i.rowid LIMIT ?", (state[0], limit)
        ).fetchall()
    except sqlite3.OperationalError as error:
        if "no such table" not in str(error):
            raise
        db.execute("UPDATE runtime_search_item_cursor SET done=1 WHERE singleton=1")
        return 0
    import json
    processed = 0
    text_bytes = 0
    last_rowid = state[0]
    for row in rows:
        record = json.loads(row[3])
        text = record.get("text", "")
        if not isinstance(text, str):
            text = ""
        row_text_bytes = len(text.encode("utf-8"))
        if processed and text_bytes + row_text_bytes > ITEM_BACKFILL_TEXT_BUDGET_BYTES:
            break
        if not (row[4] or row[5] or row[6]):
            index_item(db, row[1], row[2], record.get("title", "message"), text)
        if record.get("truncated"):
            complete = db.execute("SELECT 1 FROM runtime_item_bodies WHERE id=?", (row[1],)).fetchone()
            legacy = db.execute("SELECT body FROM runtime_search WHERE rowid=?", (row[7],)).fetchone() if row[7] is not None else None
            if complete or (legacy is not None and legacy[0] != text):
                db.execute("DELETE FROM runtime_search_partial WHERE id=?", (row[1],))
            else:
                db.execute("INSERT OR IGNORE INTO runtime_search_partial VALUES (?)", (row[1],))
        else:
            db.execute("DELETE FROM runtime_search_partial WHERE id=?", (row[1],))
        processed += 1
        text_bytes += row_text_bytes
        last_rowid = row[0]
    if processed:
        done = int(processed == len(rows) and len(rows) < limit)
        db.execute("UPDATE runtime_search_item_cursor SET rowid=?,done=? WHERE singleton=1", (last_rowid, done))
    else:
        db.execute("UPDATE runtime_search_item_cursor SET done=1 WHERE singleton=1")
    return processed


def remove(db, key):
    """Remove item-owned bodies and derived search state after item deletion."""
    db.execute("DELETE FROM runtime_item_bodies WHERE id=?", (key,))
    db.execute("DELETE FROM runtime_search_pending WHERE id=?", (key,))
    db.execute("DELETE FROM runtime_search_partial WHERE id=?", (key,))
    address = db.execute("SELECT search_rowid FROM runtime_search_rows WHERE id=?", (key,)).fetchone()
    if address:
        db.execute("DELETE FROM runtime_search WHERE rowid=?", (address[0],))
        db.execute("DELETE FROM runtime_search_rows WHERE id=?", (key,))
    else:
        cursor = db.execute("SELECT done FROM runtime_search_address_cursor WHERE singleton=1").fetchone()
        if cursor is not None and not cursor[0]:
            db.execute("INSERT OR IGNORE INTO runtime_search_deletions VALUES (?)", (key,))
    db.execute("DELETE FROM runtime_search_indexed WHERE id=?", (key,))
