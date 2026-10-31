//! Receipt schema and fixture support retained after historical migration retirement.
use crate::request_results::{AdmittedRequest, ResultEnvelope};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Serialize;
use studio_runtime_schema::persistence::{MigrationStatus, enum_check};

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
}
pub type Result<T> = std::result::Result<T, ReceiptMigrationError>;

fn unclassifiable(table: &str, key: &str, signature: &str) -> ReceiptMigrationError {
    ReceiptMigrationError::Unclassifiable {
        table: table.into(),
        key: key.into(),
        signature: signature.into(),
    }
}

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

pub(crate) fn admitted_owner_token(id: &str, signature: &str, cutover_ms: i64) -> AdmittedRequest {
    AdmittedRequest::from_migration(id, signature, cutover_ms)
}

pub fn seed_expired_before_ms(tx: &Transaction<'_>, cutover_ms: i64) -> Result<()> {
    tx.execute("INSERT INTO runtime_orchestration_settings(id,record) VALUES('global',json_object('expired_before_ms',?1)) ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.expired_before_ms',?1) WHERE coalesce(json_extract(record,'$.expired_before_ms'),0)<?1",[cutover_ms])?;
    Ok(())
}

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
