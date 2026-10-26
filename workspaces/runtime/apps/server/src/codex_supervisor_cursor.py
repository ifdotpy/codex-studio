"""Own the runtime's applied-event cursor for each supervisor journal handle.

The runtime skips a journal event whose sequence is at or below its saved cursor.
A sequence is meaningful only inside one journal epoch: a new supervisor journal
and a recreated handle row both number their events from one again. So the
cursor row stores `(handle, epoch, sequence)`; the epoch comes from the
supervisor open receipt (`codex_shared_values.SUPERVISOR_JOURNAL_EPOCH_FIELD`).

At each attach, `attach` makes the saved row describe the attached journal:

- same epoch: keep the cursor;
- other epoch: the cursor is void; start from the journal's acknowledged
  sequence, because a backend attached to that journal applied those events;
- legacy row without an epoch, or a supervisor that reports no epoch: a cursor
  above the journal's latest sequence is from an older journal and is void;
  otherwise it is kept. This rule cannot see a new journal that has already
  passed the old cursor, so it applies only to legacy rows and old supervisors.
"""
from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import sqlite3

TABLE_SQL = ("CREATE TABLE IF NOT EXISTS runtime_supervisor_cursor "
             "(handle TEXT PRIMARY KEY, sequence INTEGER NOT NULL, epoch TEXT)")
# A write for another epoch replaces the row; a write for the same epoch only advances it.
_SAVE_SQL = ("INSERT INTO runtime_supervisor_cursor(handle,sequence,epoch) VALUES (?,?,?) "
             "ON CONFLICT(handle) DO UPDATE SET "
             "sequence=CASE WHEN runtime_supervisor_cursor.epoch IS excluded.epoch "
             "THEN max(runtime_supervisor_cursor.sequence,excluded.sequence) ELSE excluded.sequence END, "
             "epoch=excluded.epoch")


def ensure_schema(db: "sqlite3.Connection") -> None:
    """Create the table, or add the epoch column to a table from an older backend."""
    db.execute(TABLE_SQL)
    columns = {row[1] for row in db.execute("PRAGMA table_info(runtime_supervisor_cursor)")}
    if "epoch" not in columns:
        db.execute("ALTER TABLE runtime_supervisor_cursor ADD COLUMN epoch TEXT")


def save(db: "sqlite3.Connection", handle: str, sequence: int, epoch: str | None) -> None:
    db.execute(_SAVE_SQL, (handle, sequence, epoch))


def applied(db: "sqlite3.Connection", handle: str, sequence: int, epoch: str | None) -> bool:
    """Return whether this exact journal event was already applied."""
    row = db.execute("SELECT sequence,epoch FROM runtime_supervisor_cursor WHERE handle=?",
                     (handle,)).fetchone()
    return bool(row and row[1] == epoch and sequence <= row[0])


def attach(db: "sqlite3.Connection", handle: str, epoch: str | None,
           journal_sequence: int, journal_acknowledged: int) -> dict[str, object]:
    """Align the saved cursor with the attached journal; return what happened."""
    row = db.execute("SELECT sequence,epoch FROM runtime_supervisor_cursor WHERE handle=?",
                     (handle,)).fetchone()
    result: dict[str, object] = {"handle": handle, "epoch": epoch,
                                 "journalSequence": journal_sequence,
                                 "journalAcknowledged": journal_acknowledged}
    if row is None:
        return {**result, "outcome": "none"}
    saved, saved_epoch = row[0], row[1]
    result.update(savedCursor=saved, savedEpoch=saved_epoch)
    if epoch is not None and saved_epoch == epoch:
        return {**result, "outcome": "kept"}
    if epoch is not None and saved_epoch is not None:
        outcome, cursor = "voidedOtherEpoch", journal_acknowledged
    elif saved > journal_sequence:
        # Legacy row or unknown epoch: only a cursor above this journal is provably foreign.
        outcome, cursor = "voidedAboveJournal", journal_acknowledged
    else:
        outcome, cursor = ("adoptedLegacy" if epoch is not None else "keptUnknownEpoch"), saved
    db.execute("UPDATE runtime_supervisor_cursor SET sequence=?,epoch=? WHERE handle=?",
               (cursor, epoch, handle))
    return {**result, "outcome": outcome, "cursor": cursor}
