//! Track changed cost group identities for incremental readers.
use crate::StoreResult;
use rusqlite::Transaction;

pub(super) fn apply(tx: &Transaction<'_>) -> StoreResult<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS analytics_cost_group_revisions (id INTEGER PRIMARY KEY, revision INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS analytics_cost_group_revisions_revision ON analytics_cost_group_revisions(revision);
        CREATE TRIGGER IF NOT EXISTS analytics_cost_group_revision_insert AFTER INSERT ON analytics_cost_groups BEGIN
            UPDATE analytics_usage_revision_clock SET revision=revision+1;
            INSERT INTO analytics_cost_group_revisions VALUES(NEW.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
        END;
        CREATE TRIGGER IF NOT EXISTS analytics_cost_group_revision_update AFTER UPDATE ON analytics_cost_groups BEGIN
            UPDATE analytics_usage_revision_clock SET revision=revision+1;
            INSERT INTO analytics_cost_group_revisions VALUES(OLD.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
            INSERT INTO analytics_cost_group_revisions VALUES(NEW.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
        END;
        CREATE TRIGGER IF NOT EXISTS analytics_cost_group_revision_delete AFTER DELETE ON analytics_cost_groups BEGIN
            UPDATE analytics_usage_revision_clock SET revision=revision+1;
            INSERT INTO analytics_cost_group_revisions VALUES(OLD.rowid,(SELECT revision FROM analytics_usage_revision_clock))
                ON CONFLICT(id) DO UPDATE SET revision=excluded.revision;
        END;")?;
    Ok(())
}
