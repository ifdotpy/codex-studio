"""Server-wide orchestration settings; the Rust scheduler and account workers read the same row."""
from __future__ import annotations

import json
import sqlite3

from codex_shared_values import CHILD_RESULT_COALESCE_DEFAULT_SECONDS, IDLE_SESSION_UNLOAD_DEFAULT_MINUTES

ROW = "global"
CHILD_RESULT_COALESCE_FIELD = "childResultCoalesceSeconds"
IDLE_SESSION_UNLOAD_FIELD = "idleSessionUnloadMinutes"
DEFAULTS = {
    CHILD_RESULT_COALESCE_FIELD: CHILD_RESULT_COALESCE_DEFAULT_SECONDS,
    IDLE_SESSION_UNLOAD_FIELD: IDLE_SESSION_UNLOAD_DEFAULT_MINUTES,
}


def _prepare(db: sqlite3.Connection) -> None:
    db.execute("CREATE TABLE IF NOT EXISTS runtime_orchestration_settings (id TEXT PRIMARY KEY, record TEXT NOT NULL)")


def read(db: sqlite3.Connection) -> dict[str, int]:
    """Return the stored settings, with the shared default for each unset field."""
    _prepare(db)
    row = db.execute("SELECT record FROM runtime_orchestration_settings WHERE id=?", (ROW,)).fetchone()
    record = json.loads(row[0]) if row else {}
    return {field: int(record.get(field, default)) for field, default in DEFAULTS.items()}


def write(db: sqlite3.Connection, child_result_coalesce_seconds: int, idle_session_unload_minutes: int) -> dict[str, int]:
    """Store settings the request model already validated and return them."""
    _prepare(db)
    record = {
        CHILD_RESULT_COALESCE_FIELD: child_result_coalesce_seconds,
        IDLE_SESSION_UNLOAD_FIELD: idle_session_unload_minutes,
    }
    db.execute(
        "INSERT INTO runtime_orchestration_settings(id,record) VALUES(?,?) "
        "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
        (ROW, json.dumps(record)),
    )
    return read(db)
