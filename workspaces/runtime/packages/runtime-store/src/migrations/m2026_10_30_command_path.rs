//! Tables for the one command path: the per-client watermark, and a revision for events
//! so a command reference names the exact state of the message it changed.
use rusqlite::Transaction;

/// The side table keeps the `runtime_events` columns unchanged. Read it with
/// `CommandWatermarks::event_revision`.
pub(super) fn apply(tx: &Transaction<'_>) -> crate::StoreResult<()> {
    tx.execute_batch(crate::repositories::command_watermarks::SCHEMA_SQL)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS runtime_event_revisions(id TEXT PRIMARY KEY, rev INTEGER NOT NULL) WITHOUT ROWID;
         CREATE TRIGGER IF NOT EXISTS runtime_event_revision AFTER UPDATE OF status,text ON runtime_events
         WHEN OLD.status IS NOT NEW.status OR OLD.text IS NOT NEW.text
         BEGIN INSERT INTO runtime_event_revisions(id,rev) VALUES(NEW.id,2)
               ON CONFLICT(id) DO UPDATE SET rev=rev+1; END;
         CREATE TRIGGER IF NOT EXISTS runtime_event_revision_cleanup AFTER DELETE ON runtime_events
         BEGIN DELETE FROM runtime_event_revisions WHERE id=OLD.id; END;",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn event_revision_counts_status_and_text_changes_and_apply_repeats() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch(
            "CREATE TABLE runtime_events(id TEXT PRIMARY KEY,text TEXT,status TEXT);
             INSERT INTO runtime_events VALUES('e','hello','pending');",
        )
        .expect("events");
        for _ in 0..2 {
            let tx = db.transaction().expect("transaction");
            apply(&tx).expect("apply");
            tx.commit().expect("commit");
        }
        let rev = |db: &Connection| -> i64 {
            crate::repositories::command_watermarks::CommandWatermarks::event_revision(db, "e")
                .expect("rev")
        };
        assert_eq!(rev(&db), 1);
        db.execute("UPDATE runtime_events SET status='cancelled'", [])
            .expect("update");
        assert_eq!(rev(&db), 2);
        db.execute("UPDATE runtime_events SET status='cancelled'", [])
            .expect("same value");
        assert_eq!(rev(&db), 2);
    }
}
