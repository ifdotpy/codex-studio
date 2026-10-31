//! Per-client command watermark: the highest applied command number of each client.
//!
//! A client is a device, an agent, or a peer. It numbers its commands from 1. A command
//! older than the `request_results` window whose number is at or below the watermark was
//! applied once and must never run again as a new command.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

/// Schema owner. The migration `2026-10-30-command-path` runs it on every database.
pub(crate) const SCHEMA_SQL: &str = "CREATE TABLE IF NOT EXISTS client_command_watermarks (
    client  TEXT PRIMARY KEY,
    applied INTEGER NOT NULL CHECK (applied >= 0)
) WITHOUT ROWID;";

pub struct CommandWatermarks;

impl CommandWatermarks {
    /// The highest applied number of `client`, or 0 when it never applied a command.
    pub fn applied(conn: &Connection, client: &str) -> rusqlite::Result<i64> {
        Ok(conn
            .query_row(
                "SELECT applied FROM client_command_watermarks WHERE client=?1",
                [client],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    /// Revision of an event: 1 until the first change, then one more for each change of its
    /// status or text (side table `runtime_event_revisions`, kept by triggers).
    pub fn event_revision(conn: &Connection, event: &str) -> rusqlite::Result<i64> {
        conn.query_row(
            "SELECT COALESCE((SELECT rev FROM runtime_event_revisions WHERE id=?1),1)",
            [event],
            |row| row.get(0),
        )
    }

    /// Raise the watermark to `number`. It never moves down.
    pub fn advance(tx: &Transaction<'_>, client: &str, number: i64) -> rusqlite::Result<()> {
        tx.execute(
            "INSERT INTO client_command_watermarks(client,applied) VALUES(?1,?2)
             ON CONFLICT(client) DO UPDATE SET applied=max(applied,excluded.applied)",
            params![client, number],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_starts_at_zero_and_never_moves_down() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch(SCHEMA_SQL).expect("schema");
        assert_eq!(CommandWatermarks::applied(&db, "a").unwrap(), 0);
        let tx = db.transaction().unwrap();
        CommandWatermarks::advance(&tx, "a", 5).unwrap();
        CommandWatermarks::advance(&tx, "a", 3).unwrap();
        CommandWatermarks::advance(&tx, "b", 1).unwrap();
        tx.commit().unwrap();
        assert_eq!(CommandWatermarks::applied(&db, "a").unwrap(), 5);
        assert_eq!(CommandWatermarks::applied(&db, "b").unwrap(), 1);
    }
}
