//! Shared primitives for independent, catalog-registered receipt-family migrations.
//! Each file owns its mapping and prepare/move/finish implementation. No family calls another.
use rusqlite::{OptionalExtension, Transaction, params};
use studio_runtime_schema::persistence::{MigrationStatus, ReceiptMigrationPhase, enum_check};

/// Maximum input rows in one migration batch. Unit: source rows.
pub const MIGRATION_BATCH_ROWS: usize = 500;
/// Maximum input size in one migration batch. Unit: bytes (8 MiB).
pub const MIGRATION_BATCH_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ReceiptMigrationError {
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error("receipt migration cannot classify table={table} key={key} signature={signature}")]
    Unclassifiable {
        table: String,
        key: String,
        signature: String,
    },
    #[error("receipt migration {migration} is incomplete at source {source_table}")]
    Incomplete {
        migration: String,
        source_table: String,
    },
}
pub type Result<T> = std::result::Result<T, ReceiptMigrationError>;

/// Budget is checked before decoding a payload. An oversized row runs alone.
#[derive(Debug, Default)]
pub struct BatchBudget {
    pub rows: usize,
    pub bytes: usize,
}
impl BatchBudget {
    pub fn include(&mut self, input_bytes: usize) -> bool {
        if self.rows >= MIGRATION_BATCH_ROWS
            || (self.rows != 0 && self.bytes.saturating_add(input_bytes) > MIGRATION_BATCH_BYTES)
        {
            return false;
        }
        self.rows += 1;
        self.bytes = self.bytes.saturating_add(input_bytes);
        true
    }
}

/// Cursor advances only over the scanned rowid page, never opportunistic phase siblings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationCheckpoint {
    pub cursor: i64,
    pub status: MigrationStatus,
    pub cutover_ms: i64,
}
impl MigrationCheckpoint {
    fn name(migration: &str, source: &str) -> String {
        // Length-prefixing keeps independent migration/source names collision-free.
        format!("receipts:{}:{migration}:{source}", migration.len())
    }

    pub fn load(tx: &Transaction<'_>, migration: &str, source: &str) -> Result<Option<Self>> {
        Ok(tx
            .query_row(
                "SELECT cursor,status,cutover_ms FROM runtime_payload_migrations WHERE name=?1",
                [Self::name(migration, source)],
                |row| {
                    Ok(Self {
                        cursor: row.get(0)?,
                        status: row.get(1)?,
                        cutover_ms: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn begin(tx: &Transaction<'_>, migration: &str, source: &str, now_ms: i64) -> Result<Self> {
        if let Some(saved) = Self::load(tx, migration, source)? {
            return Ok(saved);
        }
        let checkpoint = Self {
            cursor: 0,
            status: MigrationStatus::Pending,
            cutover_ms: cutover_ms(tx, now_ms)?,
        };
        checkpoint.save(tx, migration, source, now_ms)?;
        Ok(checkpoint)
    }

    /// Save with destination writes and source deletions in the caller's same transaction.
    pub fn save(
        &self,
        tx: &Transaction<'_>,
        migration: &str,
        source: &str,
        now_ms: i64,
    ) -> Result<()> {
        tx.execute("INSERT INTO runtime_payload_migrations(name,cursor,complete,status,updated,error,cutover_ms) VALUES(?1,?2,?3,?4,?5,NULL,?6) ON CONFLICT(name) DO UPDATE SET cursor=excluded.cursor,complete=excluded.complete,status=excluded.status,updated=excluded.updated,error=NULL", params![Self::name(migration,source),self.cursor,self.status==MigrationStatus::Complete,self.status,now_ms as f64/1000.0,self.cutover_ms])?;
        Ok(())
    }
}

/// One timestamp shared by all independent catalog entries and retained across restart.
pub fn cutover_ms(tx: &Transaction<'_>, now_ms: i64) -> Result<i64> {
    tx.execute("INSERT OR IGNORE INTO runtime_payload_migrations(name,cursor,complete,status,updated,cutover_ms) VALUES('receipts:cutover',0,1,?1,?2,?3)",params![MigrationStatus::Complete,now_ms as f64/1000.0,now_ms])?;
    Ok(tx.query_row(
        "SELECT cutover_ms FROM runtime_payload_migrations WHERE name='receipts:cutover'",
        [],
        |row| row.get(0),
    )?)
}

/// Create shared destination schema and checkpoint columns once under the startup lock.
/// Existing checkpoint status CHECK rebuilding belongs to the schema cutover migration.
pub fn prepare_schema(tx: &Transaction<'_>) -> Result<()> {
    crate::receipt_schema::create(tx)?;
    tx.execute_batch(&format!("CREATE TABLE IF NOT EXISTS runtime_payload_migrations(name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT '{}' {},updated REAL,error TEXT,cutover_ms INTEGER)",MigrationStatus::Pending.as_str(), enum_check::<MigrationStatus>("status")))?;
    let has_cutover: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_payload_migrations') WHERE name='cutover_ms')",[],|row|row.get(0))?;
    if !has_cutover {
        tx.execute_batch("ALTER TABLE runtime_payload_migrations ADD COLUMN cutover_ms INTEGER")?;
    }
    Ok(())
}

/// One independent migration file implements this contract and registers its own catalog ID.
/// The startup runner commits between keyset batches and checkpoints WAL outside transactions.
pub trait ReceiptFamilyMigration {
    fn id(&self) -> &'static str;
    fn prepare(&self, tx: &Transaction<'_>, cutover_ms: i64) -> Result<()>;
    /// Read WHERE rowid > checkpoint.cursor ORDER BY rowid LIMIT MIGRATION_BATCH_ROWS;
    /// apply BatchBudget before payload decoding; never use OFFSET or discard unknown rows.
    fn move_batch(
        &self,
        tx: &Transaction<'_>,
        source: &str,
        checkpoint: &MigrationCheckpoint,
    ) -> Result<MigrationCheckpoint>;
    /// Verify every source checkpoint and destination proof before a final drop transaction.
    fn finish(&self, tx: &Transaction<'_>) -> Result<()>;
}

/// Stable phase IDs for existing migration catalog registration.
pub fn phase_id(migration: &str, phase: ReceiptMigrationPhase) -> String {
    format!("{migration}-{}", phase.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_migration_independent_checkpoints_bounded_and_atomic() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        {
            let tx = db.transaction().expect("tx");
            prepare_schema(&tx).expect("schema");
            tx.commit().expect("commit");
        }
        {
            let tx = db.transaction().expect("tx");
            let a = MigrationCheckpoint::begin(&tx, "family-a", "source", 1000).expect("begin");
            let b = MigrationCheckpoint::begin(&tx, "family-b", "source", 2000).expect("begin");
            assert_eq!(a.cutover_ms, b.cutover_ms);
            assert_eq!(a.cutover_ms, 1000);
            tx.commit().expect("commit");
        }
        {
            let tx = db.transaction().expect("tx");
            let mut a =
                MigrationCheckpoint::begin(&tx, "family-a", "source", 3000).expect("resume");
            a.cursor = 500;
            a.save(&tx, "family-a", "source", 3000).expect("checkpoint");
            tx.rollback().expect("interrupted batch");
        }
        let tx = db.transaction().expect("tx");
        assert_eq!(
            MigrationCheckpoint::load(&tx, "family-a", "source")
                .expect("load")
                .expect("checkpoint")
                .cursor,
            0
        );
        let mut budget = BatchBudget::default();
        for _ in 0..MIGRATION_BATCH_ROWS {
            assert!(budget.include(1));
        }
        assert!(!budget.include(1));
        let mut budget = BatchBudget::default();
        assert!(budget.include(MIGRATION_BATCH_BYTES));
        assert!(!budget.include(1));
        let mut budget = BatchBudget::default();
        assert!(budget.include(MIGRATION_BATCH_BYTES + 1));
        assert!(!budget.include(1));
        let error = ReceiptMigrationError::Unclassifiable {
            table: "source".into(),
            key: "key".into(),
            signature: "signature".into(),
        };
        assert_eq!(
            error.to_string(),
            "receipt migration cannot classify table=source key=key signature=signature"
        );
    }
}
