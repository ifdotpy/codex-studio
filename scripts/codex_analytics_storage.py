"""Resumable online copy and retirement of analytics tables in canvas.sqlite3."""
from __future__ import annotations

import json
import shutil
import sqlite3
import threading
import time

COPY_SPACE_BYTES = 16_500_000_000
BATCH_ROWS = 128
BATCH_BYTES = 1024 * 1024
TABLES = {
    "analytics_meta": ("key",),
    "analytics_usage_roots": ("root",),
    "analytics_turns": ("id",),
    "analytics_notifications": ("id",),
    "analytics_limits": ("id",),
    "analytics_agents": ("id",),
    "analytics_usage": ("id",),
    "analytics_items": ("id",),
    "analytics_history": ("id",),
}


def copy_step(analytics_path, canvas_path):
    """Copy or retire one bounded page. Returns (advanced, status, timing)."""
    db = sqlite3.connect(analytics_path, timeout=15)
    try:
        db.row_factory = sqlite3.Row
        db.execute("PRAGMA journal_mode=WAL")
        uri = canvas_path.absolute().as_uri() + "?mode=ro"
        db.execute("ATTACH DATABASE ? AS canvas", (uri,))
        source = {row[0] for row in db.execute(
            "SELECT name FROM canvas.sqlite_master WHERE type='table' AND name LIKE 'analytics_%'")}
        state = db.execute("SELECT value FROM analytics_meta WHERE key='fileMigrationV1'").fetchone()
        migration = json.loads(state[0]) if state else {"phase": "checkSpace", "tables": {}, "copied": 0,
                                                         "batches": 0, "writesMs": []}
        if migration["phase"] == "checkSpace":
            unknown = sorted(source - TABLES.keys())
            if unknown:
                migration["phase"] = "unsupportedTables"
                migration["unsupportedTables"] = unknown
                _save(db, migration)
                db.commit()
                return False, migration["phase"], 0
            if source and shutil.disk_usage(analytics_path.parent).free < COPY_SPACE_BYTES:
                migration["phase"] = "insufficientSpace"
                migration["requiredBytes"] = COPY_SPACE_BYTES
                migration["freeBytes"] = shutil.disk_usage(analytics_path.parent).free
                _save(db, migration)
                db.commit()
                return False, migration["phase"], 0
            for table in sorted(source & TABLES.keys()):
                present = db.execute("SELECT 1 FROM main.sqlite_master WHERE type='table' AND name=?",
                                     (table,)).fetchone()
                if not present:
                    migration["phase"] = "missingTargetTable"
                    migration["missingTargetTable"] = table
                    _save(db, migration)
                    db.commit()
                    return False, migration["phase"], 0
                max_row = db.execute(f"SELECT COALESCE(MAX(rowid),0) FROM canvas.{table}").fetchone()[0]
                migration["tables"][table] = {"cursor": 0, "highWater": max_row, "copied": 0,
                                               "deleteCursor": 0}
            migration["phase"] = "copy" if migration["tables"] else "complete"
            _save(db, migration)
            db.commit()
            return True, migration["phase"], 0

        if migration["phase"] == "copy":
            for table, progress in migration["tables"].items():
                if progress["cursor"] >= progress["highWater"]:
                    continue
                rows = db.execute(f"SELECT rowid AS _source_rowid,* FROM canvas.{table} "
                                  "WHERE rowid>? AND rowid<=? ORDER BY rowid LIMIT ?",
                                  (progress["cursor"], progress["highWater"], BATCH_ROWS)).fetchall()
                start = time.perf_counter()
                db.execute("BEGIN")
                copied = 0
                size = 0
                for row in rows:
                    values = dict(row)
                    source_rowid = values.pop("_source_rowid")
                    progress["cursor"] = source_rowid
                    size += sum(len(str(value)) for value in values.values() if value is not None)
                    _copy_row(db, table, values)
                    progress["copied"] += 1
                    copied += 1
                    if size >= BATCH_BYTES:
                        break
                migration["copied"] += copied
                migration["batches"] += 1
                _save(db, migration)
                db.commit()
                elapsed = (time.perf_counter() - start) * 1000
                migration["writesMs"].append(elapsed)
                if len(migration["writesMs"]) > 1000:
                    migration["writesMs"] = migration["writesMs"][-1000:]
                db.execute("BEGIN")
                _save(db, migration)
                db.commit()
                return True, "copy", elapsed
            migration["phase"] = "retire"
            _save(db, migration)
            db.commit()
            return True, migration["phase"], 0

        if migration["phase"] == "retire":
            for table, progress in migration["tables"].items():
                if not db.execute("SELECT 1 FROM canvas.sqlite_master WHERE type='table' AND name=?",
                                  (table,)).fetchone():
                    continue
                row = db.execute(f"SELECT rowid FROM canvas.{table} WHERE rowid>? ORDER BY rowid LIMIT ?",
                                 (progress["deleteCursor"], BATCH_ROWS)).fetchall()
                if not row:
                    continue
                start = time.perf_counter()
                last = row[-1][0]
                source = sqlite3.connect(canvas_path, timeout=15)
                try:
                    source.execute("BEGIN IMMEDIATE")
                    source.execute(f"DELETE FROM {table} WHERE rowid>? AND rowid<=?",
                                   (progress["deleteCursor"], last))
                    source.commit()
                except BaseException:
                    source.rollback()
                    raise
                finally:
                    source.close()
                progress["deleteCursor"] = last
                elapsed = (time.perf_counter() - start) * 1000
                migration.setdefault("retireWritesMs", []).append(elapsed)
                if len(migration["retireWritesMs"]) > 1000:
                    migration["retireWritesMs"] = migration["retireWritesMs"][-1000:]
                db.execute("BEGIN")
                _save(db, migration)
                db.commit()
                return True, "retire", elapsed
            source = sqlite3.connect(canvas_path, timeout=15)
            try:
                source.execute("BEGIN IMMEDIATE")
                for table in migration["tables"]:
                    source.execute(f"DROP TABLE IF EXISTS {table}")
                source.commit()
            except BaseException:
                source.rollback()
                raise
            finally:
                source.close()
            migration["phase"] = "complete"
            migration["completedAt"] = time.time()
            _save(db, migration)
            db.commit()
            return True, "complete", 0
        return False, migration["phase"], 0
    finally:
        db.close()


def _copy_row(db, table, values):
    columns = list(values)
    placeholders = ",".join("?" for _ in columns)
    query = f"INSERT OR IGNORE INTO main.{table} ({','.join(columns)}) VALUES ({placeholders})"
    try:
        db.execute(query, [values[column] for column in columns])
        if db.execute("SELECT changes()").fetchone()[0]:
            return
        primary = TABLES[table][0]
        if db.execute(f"SELECT 1 FROM main.{table} WHERE {primary}=?", (values[primary],)).fetchone():
            if table != "analytics_limits":
                return  # A concurrent target write is newer and remains authoritative.
            existing = db.execute("SELECT account,at,record FROM main.analytics_limits WHERE id=?",
                                  (values[primary],)).fetchone()
            if existing and tuple(existing) == (values["account"], values["at"], values["record"]):
                return
        # AUTOINCREMENT/rowid collision with a concurrent target insert: preserve
        # the source identity while assigning a fresh local sequence/row id.
        values = dict(values)
        values.pop("seq", None)
        if table == "analytics_limits":
            values.pop("id", None)
        columns = list(values)
        db.execute(f"INSERT OR IGNORE INTO main.{table} ({','.join(columns)}) VALUES "
                   f"({','.join('?' for _ in columns)})", [values[column] for column in columns])
    except sqlite3.OperationalError:
        raise


def _save(db, migration):
    db.execute("INSERT INTO analytics_meta(key,value) VALUES ('fileMigrationV1',?) "
               "ON CONFLICT(key) DO UPDATE SET value=excluded.value", (json.dumps(migration),))


def install_legacy_read_views(db):
    """Overlay uncopied legacy rows on read connections until copy is complete."""
    legacy = {row[0] for row in db.execute(
        "SELECT name FROM canvas.sqlite_master WHERE type='table' AND name LIKE 'analytics_%'")}
    if not legacy:
        return
    for table in sorted(legacy & TABLES.keys()):
        primary = TABLES[table][0]
        exists = db.execute("SELECT 1 FROM main.sqlite_master WHERE type='table' AND name=?", (table,)).fetchone()
        if not exists:
            continue
        db.execute(f"CREATE TEMP VIEW {table} AS SELECT * FROM main.{table} "
                   f"UNION ALL SELECT old.* FROM canvas.{table} old WHERE NOT EXISTS "
                   f"(SELECT 1 FROM main.{table} current WHERE current.{primary}=old.{primary})")


def start(runtime):
    """Start one background migrator; repeated startup is safe."""
    worker = getattr(runtime, "analytics_migration_thread", None)
    if worker and worker.is_alive():
        return False

    def run():
        while not runtime.closed:
            try:
                advanced, status, _elapsed = copy_step(runtime.analytics_db_path, runtime.db_path)
                runtime.analytics_migration_status = {"status": status, "updated": time.time()}
                if status in {"complete", "insufficientSpace", "unsupportedTables", "missingTargetTable"}:
                    return
                if not advanced:
                    time.sleep(.5)
            except Exception as error:
                runtime.analytics_migration_status = {"status": "error", "updated": time.time(),
                                                      "error": f"{type(error).__name__}: {error}"[:1000]}
                time.sleep(1)

    runtime.analytics_migration_thread = threading.Thread(target=run, daemon=True,
                                                          name="analytics-file-migration")
    runtime.analytics_migration_thread.start()
    return True
