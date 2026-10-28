//! Shared primitives for independent, catalog-registered receipt-family migrations.
//! Each file owns its mapping and prepare/move/finish implementation. No family calls another.
use crate::request_results::{AdmittedRequest, ResultEnvelope};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};
use studio_runtime_schema::persistence::{
    MigrationStatus, ReceiptMigrationPhase, RequestIdentity, enum_check,
};

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
    #[error(
        "cannot discard completed request table={table} key={key} issuance_ms={issued_ms} is after cutover_ms={cutover_ms}"
    )]
    FutureDatedCompletedRequest {
        table: String,
        key: String,
        issued_ms: i64,
        cutover_ms: i64,
    },
}
pub type Result<T> = std::result::Result<T, ReceiptMigrationError>;

/// Raw receipt bytes at the migration boundary. Families classify before any deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReceipt {
    pub rowid: i64,
    pub id: String,
    pub signature: String,
    pub result: String,
}
impl SourceReceipt {
    pub fn json<T: DeserializeOwned>(&self, table: &str) -> Result<T> {
        serde_json::from_str(&self.result)
            .map_err(|_| unclassifiable(table, &self.id, &self.signature))
    }
}

pub fn unclassifiable(table: &str, id: &str, signature: &str) -> ReceiptMigrationError {
    ReceiptMigrationError::Unclassifiable {
        table: table.into(),
        key: id.into(),
        signature: signature.into(),
    }
}

/// Trusted static SQL expressions, owned by the independent family migration.
/// They may project composite IDs or correlate a task's original signature.
pub struct ReceiptProjection<'a> {
    pub id: &'a str,
    pub signature: &'a str,
    pub result: &'a str,
}

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn source_exists(tx: &Transaction<'_>, table: &str) -> Result<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
        [table],
        |row| row.get(0),
    )?)
}

pub fn read_receipt_batch(
    tx: &Transaction<'_>,
    table: &str,
    checkpoint: &MigrationCheckpoint,
    predicate: &str,
) -> Result<Vec<SourceReceipt>> {
    read_source_batch(
        tx,
        table,
        checkpoint,
        &ReceiptProjection {
            id: "source.id",
            signature: "source.signature",
            result: "source.result",
        },
        predicate,
    )
}

/// Size metadata is read before fetching/decoding any selected payload. The
/// predicate/expressions are trusted family SQL; source is the table alias.
pub fn read_source_batch(
    tx: &Transaction<'_>,
    table: &str,
    checkpoint: &MigrationCheckpoint,
    projection: &ReceiptProjection<'_>,
    predicate: &str,
) -> Result<Vec<SourceReceipt>> {
    if !source_exists(tx, table)? {
        return Ok(Vec::new());
    }
    let table_sql = identifier(table);
    let ReceiptProjection {
        id,
        signature,
        result,
    } = projection;
    let sql = format!(
        "SELECT source.rowid,({id}),({signature}),coalesce(length(CAST(({result}) AS BLOB)),0)+coalesce(length(CAST(({id}) AS BLOB)),0)+coalesce(length(CAST(({signature}) AS BLOB)),0) FROM {table_sql} AS source WHERE source.rowid>?1 AND ({predicate}) ORDER BY source.rowid LIMIT ?2"
    );
    let mut statement = tx.prepare(&sql)?;
    let mut metadata = statement.query(params![checkpoint.cursor, MIGRATION_BATCH_ROWS as i64])?;
    let mut budget = BatchBudget::default();
    let mut batch = Vec::new();
    while let Some(row) = metadata.next()? {
        let rowid = row.get::<_, i64>(0)?;
        let identity = || -> rusqlite::Result<(String, String)> { Ok((row.get(1)?, row.get(2)?)) };
        let (id,signature)=identity().map_err(|_| {
            let (key,signature)=tx.query_row(&format!("SELECT hex(CAST(({id}) AS BLOB)),hex(CAST(({signature}) AS BLOB)) FROM {table_sql} AS source WHERE source.rowid=?1"),[rowid],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?))).unwrap_or_else(|_|(format!("rowid:{rowid}"),"unreadable".into()));
            unclassifiable(table,&format!("rowid:{rowid};hex:{key}"),&format!("hex:{signature}"))
        })?;
        let bytes = usize::try_from(row.get::<_, i64>(3)?)
            .map_err(|_| unclassifiable(table, &id, &signature))?
            .saturating_add(std::mem::size_of::<i64>());
        if !budget.include(bytes) {
            break;
        }
        let result = tx
            .query_row(
                &format!("SELECT ({result}) FROM {table_sql} AS source WHERE source.rowid=?1"),
                [rowid],
                |row| row.get::<_, String>(0),
            )
            .map_err(|_| unclassifiable(table, &id, &signature))?;
        batch.push(SourceReceipt {
            rowid,
            id,
            signature,
            result,
        });
    }
    Ok(batch)
}

/// Import exact envelopes without replacing a signature or choosing between replies.
pub fn import_result<R: Serialize>(
    tx: &Transaction<'_>,
    id: &str,
    signature: &str,
    envelope: &ResultEnvelope<R>,
    created: i64,
) -> Result<()> {
    let result = serde_json::to_string(envelope)
        .map_err(|_| unclassifiable("request_results", id, signature))?;
    let existing: Option<(String, String)> = tx
        .query_row(
            "SELECT signature,result FROM request_results WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((saved_signature, saved_result)) = existing {
        if saved_signature != signature || saved_result != result {
            return Err(unclassifiable("request_results", id, signature));
        }
        return Ok(());
    }
    let admitted = admitted_owner_token(id, signature, created);
    tx.execute(
        "INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)",
        params![
            admitted.id(),
            admitted.signature(),
            result,
            admitted.created()
        ],
    )?;
    Ok(())
}

pub fn delete_source(tx: &Transaction<'_>, table: &str, rowid: i64) -> Result<()> {
    tx.execute(
        &format!("DELETE FROM {} WHERE rowid=?1", identifier(table)),
        [rowid],
    )?;
    Ok(())
}

pub fn import_outbox<P: Serialize>(
    tx: &Transaction<'_>,
    row: &crate::outbox::OutboxRow<P>,
) -> Result<()> {
    crate::outbox::Outbox::put(tx, row).map_err(|error| match error {
        crate::outbox::OutboxError::Sql(error) => ReceiptMigrationError::Sql(error),
        crate::outbox::OutboxError::Json(_) | crate::outbox::OutboxError::Conflict(_) => {
            unclassifiable("runtime_outbox", &row.id, &row.signature)
        }
    })
}

/// Only startup migration modules may hydrate tokens from classified old owners.
pub(crate) fn admitted_owner_token(id: &str, signature: &str, cutover_ms: i64) -> AdmittedRequest {
    AdmittedRequest::from_migration(id, signature, cutover_ms)
}

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
    let cutover_ms = tx.query_row(
        "SELECT cutover_ms FROM runtime_payload_migrations WHERE name='receipts:cutover'",
        [],
        |row| row.get(0),
    )?;
    seed_expired_before_ms(tx, cutover_ms)?;
    Ok(cutover_ms)
}

/// Atomic ingress fence only. Cached results retain their independent created-time TTL.
pub fn seed_expired_before_ms(tx: &Transaction<'_>, cutover_ms: i64) -> Result<()> {
    tx.execute("INSERT INTO runtime_orchestration_settings(id,record) VALUES('global',json_object('expired_before_ms',?1)) ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.expired_before_ms',?1) WHERE coalesce(json_extract(record,'$.expired_before_ms'),0)<?1",[cutover_ms])?;
    Ok(())
}

/// Completed C rows lacking an exact reply may be deleted only if the cutover
/// fence can prevent their identity from being freshly admitted in the future.
pub fn validate_completed_discard(table: &str, id: &str, cutover_ms: i64) -> Result<()> {
    if let Ok(identity) = RequestIdentity::new("migration-validation", &[], id, &[])
        && identity.issued_ms() > cutover_ms
    {
        return Err(ReceiptMigrationError::FutureDatedCompletedRequest {
            table: table.into(),
            key: id.into(),
            issued_ms: identity.issued_ms(),
            cutover_ms,
        });
    }
    Ok(())
}

/// Create shared destination schema and checkpoint columns once under the startup lock.
/// Existing checkpoint status CHECK rebuilding belongs to the schema cutover migration.
pub fn prepare_schema(tx: &Transaction<'_>) -> Result<()> {
    crate::receipt_schema::create(tx)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_orchestration_settings(id TEXT PRIMARY KEY,record TEXT NOT NULL)")?;
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
    fn receipt_migration_cutover_fences_fresh_ids_without_expiring_imports() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("tx");
        prepare_schema(&tx).expect("schema");
        assert_eq!(cutover_ms(&tx, 1000).expect("cutover"), 1000);
        assert_eq!(
            crate::request_results::RequestResults::expired_before_ms(&tx).expect("seeded floor"),
            1000
        );
        assert_eq!(cutover_ms(&tx, 2000).expect("same cutover"), 1000);
        let checkpoint =
            MigrationCheckpoint::begin(&tx, "family", "missing", 3000).expect("checkpoint");
        assert!(
            read_receipt_batch(&tx, "missing", &checkpoint, "1")
                .expect("fresh has no source")
                .is_empty()
        );
        assert!(validate_completed_discard("old", "ordinary opaque id", 1000).is_ok());
        let at = |ms: u128| uuid::Uuid::from_u128((ms << 80) | (7 << 76) | (2 << 62)).to_string();
        assert!(validate_completed_discard("old", &at(1000), 1000).is_ok());
        assert!(
            matches!(validate_completed_discard("old",&at(1001),1000),Err(ReceiptMigrationError::FutureDatedCompletedRequest{table,key,issued_ms:1001,cutover_ms:1000}) if table=="old"&&key==at(1001))
        );
        let token = admitted_owner_token("imported", "sig", 1000);
        crate::request_results::RequestResults::settle(
            &tx,
            &token,
            &ResultEnvelope {
                actor: None,
                reply: true,
                references: vec![],
            },
        )
        .expect("import");
        assert!(
            crate::request_results::RequestResults::replay(&tx, "imported", "sig", 1001)
                .expect("30 day grace")
                .is_some()
        );
        tx.rollback().expect("atomic rollback");
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='runtime_orchestration_settings'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("seed rolled back"),
            0
        );
    }
    #[test]
    fn receipt_migration_scan_is_keyset_bounded_and_imports_are_atomic() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        let tx = db.transaction().expect("tx");
        prepare_schema(&tx).expect("schema");
        tx.execute_batch("CREATE TABLE old_rows(id TEXT PRIMARY KEY,signature TEXT,result TEXT); INSERT INTO old_rows VALUES('first','sig','{\"ok\":true}')").expect("source");
        let oversized = "x".repeat(MIGRATION_BATCH_BYTES + 1);
        tx.execute("INSERT INTO old_rows VALUES('big','sig',?1)", [&oversized])
            .expect("oversized fixture");
        tx.execute(
            "INSERT INTO old_rows VALUES('last','sig','{\"ok\":false}')",
            [],
        )
        .expect("tail");
        let mut checkpoint =
            MigrationCheckpoint::begin(&tx, "family", "old_rows", 1000).expect("checkpoint");
        let first =
            read_receipt_batch(&tx, "old_rows", &checkpoint, "1").expect("first bounded page");
        assert_eq!(first.len(), 1);
        checkpoint.cursor = first[0].rowid;
        let big = read_receipt_batch(&tx, "old_rows", &checkpoint, "1").expect("oversized alone");
        assert_eq!(big.len(), 1);
        assert_eq!(big[0].id, "big");
        assert!(
            matches!(big[0].json::<ResultEnvelope<bool>>("old_rows"),Err(ReceiptMigrationError::Unclassifiable{table,key,signature}) if table=="old_rows"&&key=="big"&&signature=="sig")
        );
        checkpoint.cursor = big[0].rowid;
        let last = read_source_batch(
            &tx,
            "old_rows",
            &checkpoint,
            &ReceiptProjection {
                id: "source.id",
                signature: "source.signature",
                result: "source.result",
            },
            "1",
        )
        .expect("tail page");
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].id, "last");
        let result = ResultEnvelope {
            actor: Some("actor".into()),
            reply: true,
            references: vec!["attempt".into()],
        };
        import_result(&tx, "first", "sig", &result, 1000).expect("import");
        import_result(&tx, "first", "sig", &result, 2000).expect("same import keeps time");
        assert!(matches!(
            import_result(&tx, "first", "changed", &result, 1000),
            Err(ReceiptMigrationError::Unclassifiable { .. })
        ));
        assert!(matches!(
            import_result(
                &tx,
                "first",
                "sig",
                &ResultEnvelope {
                    actor: Some("actor".into()),
                    reply: false,
                    references: vec!["attempt".into()]
                },
                1000
            ),
            Err(ReceiptMigrationError::Unclassifiable { .. })
        ));
        assert_eq!(
            tx.query_row(
                "SELECT created FROM request_results WHERE id='first'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("original cutover"),
            1000
        );
        delete_source(&tx, "old_rows", first[0].rowid).expect("delete moved row");
        checkpoint
            .save(&tx, "family", "old_rows", 2000)
            .expect("checkpoint");
        tx.rollback().expect("failed batch rollback");
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='request_results'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("schema rolled back"),
            0
        );
    }
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
