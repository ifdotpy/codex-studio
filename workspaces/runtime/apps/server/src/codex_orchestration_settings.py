"""Server-wide orchestration settings; the Rust scheduler reads the same row."""
from __future__ import annotations

import json
import sqlite3

from codex_shared_values import CHILD_RESULT_COALESCE_DEFAULT_SECONDS

ROW = "global"
CHILD_RESULT_COALESCE_FIELD = "childResultCoalesceSeconds"


def _prepare(db: sqlite3.Connection) -> None:
    db.execute("CREATE TABLE IF NOT EXISTS runtime_orchestration_settings (id TEXT PRIMARY KEY, record TEXT NOT NULL)")


def read(db: sqlite3.Connection) -> dict[str, int]:
    """Return the stored settings, with the shared default for an unset hold."""
    _prepare(db)
    row = db.execute("SELECT record FROM runtime_orchestration_settings WHERE id=?", (ROW,)).fetchone()
    record = json.loads(row[0]) if row else {}
    seconds = record.get(CHILD_RESULT_COALESCE_FIELD, CHILD_RESULT_COALESCE_DEFAULT_SECONDS)
    return {CHILD_RESULT_COALESCE_FIELD: int(seconds)}


def write(db: sqlite3.Connection, child_result_coalesce_seconds: int) -> dict[str, int]:
    """Store a hold the request model already validated and return the settings."""
    _prepare(db)
    db.execute(
        "INSERT INTO runtime_orchestration_settings(id,record) VALUES(?,?) "
        "ON CONFLICT(id) DO UPDATE SET record=excluded.record",
        (ROW, json.dumps({CHILD_RESULT_COALESCE_FIELD: child_result_coalesce_seconds})),
    )
    return read(db)
