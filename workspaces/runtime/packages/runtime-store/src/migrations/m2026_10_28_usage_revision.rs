//! Track the latest committed mutation of each analytics usage identity.
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS analytics_usage_revision_clock (revision INTEGER NOT NULL);
         INSERT INTO analytics_usage_revision_clock SELECT 0 WHERE NOT EXISTS (SELECT 1 FROM analytics_usage_revision_clock);
         CREATE TABLE IF NOT EXISTS analytics_usage_revisions (id TEXT PRIMARY KEY, revision INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS analytics_usage_revisions_revision ON analytics_usage_revisions(revision);
         CREATE TRIGGER IF NOT EXISTS analytics_usage_revision_insert AFTER INSERT ON analytics_usage BEGIN
           UPDATE analytics_usage_revision_clock SET revision=revision+1;
           INSERT INTO analytics_usage_revisions VALUES(NEW.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
         END;
         CREATE TRIGGER IF NOT EXISTS analytics_usage_revision_update AFTER UPDATE ON analytics_usage BEGIN
           UPDATE analytics_usage_revision_clock SET revision=revision+1;
           INSERT INTO analytics_usage_revisions VALUES(OLD.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
           INSERT INTO analytics_usage_revisions VALUES(NEW.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
         END;
         CREATE TRIGGER IF NOT EXISTS analytics_usage_revision_delete AFTER DELETE ON analytics_usage BEGIN
           UPDATE analytics_usage_revision_clock SET revision=revision+1;
           INSERT INTO analytics_usage_revisions VALUES(OLD.id,(SELECT revision FROM analytics_usage_revision_clock))
             ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
         END;",
    )?;
    Ok(())
}
