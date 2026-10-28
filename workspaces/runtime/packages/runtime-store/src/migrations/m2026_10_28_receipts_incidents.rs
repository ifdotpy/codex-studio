//! Move diagnostic history and per-error/group dismissal times out of replay.
use rusqlite::Transaction;
use serde::Deserialize;
use studio_runtime_schema::persistence::MigrationStatus;
use studio_runtime_schema::studio_errors::{Kind, StudioError, Target};

use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, Result, delete_source,
    read_receipt_batch,
};

pub(super) struct Family;
const SOURCE: &str = "runtime_operation_receipts";
const PREDICATE: &str = "id GLOB 'studio-errors:*'";

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-incidents"
    }
    fn prepare(&self, _tx: &Transaction<'_>, _cutover_ms: i64) -> Result<()> {
        Ok(())
    }
    fn move_batch(
        &self,
        tx: &Transaction<'_>,
        source: &str,
        checkpoint: &MigrationCheckpoint,
    ) -> Result<MigrationCheckpoint> {
        let rows = read_receipt_batch(tx, source, checkpoint, PREDICATE)?;
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        for row in rows {
            let invalid = || ReceiptMigrationError::Unclassifiable {
                table: SOURCE.into(),
                key: row.id.clone(),
                signature: row.signature.clone(),
            };
            if let Some(id) = row
                .id
                .strip_prefix("studio-errors:incident:")
                .filter(|id| !id.is_empty())
            {
                if row.signature != "studio-error-incident-v1" {
                    return Err(invalid());
                }
                let mut payload: serde_json::Value = row.json(SOURCE)?;
                // Original diagnostic records predate the required level field.
                let object = payload.as_object_mut().ok_or_else(invalid)?;
                let kind: Kind =
                    serde_json::from_value(object.get("kind").cloned().ok_or_else(invalid)?)
                        .map_err(|_| invalid())?;
                object.insert(
                    "level".into(),
                    serde_json::to_value(kind.level()).map_err(|_| invalid())?,
                );
                let incident: StudioError =
                    serde_json::from_value(payload).map_err(|_| invalid())?;
                if incident.id != id
                    || !matches!(kind, Kind::HttpFailure | Kind::RecoveryFailed)
                    || !matches!(incident.target, Target::Details)
                {
                    return Err(invalid());
                }
                crate::error_incidents::put(tx, &incident).map_err(|_| invalid())?;
            } else if let Some(id) = row
                .id
                .strip_prefix("studio-errors:dismissed:")
                .filter(|id| !id.is_empty())
            {
                if row.signature != "studio-error-dismissal" {
                    return Err(invalid());
                }
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Dismissal {
                    at: f64,
                }
                let value: Dismissal = row.json(SOURCE)?;
                crate::error_incidents::dismiss(tx, id, value.at, id.starts_with("monitors:"))
                    .map_err(|_| invalid())?;
            } else {
                return Err(invalid());
            }
            delete_source(tx, SOURCE, row.rowid)?;
            next.cursor = row.rowid;
        }
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        let remaining: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {SOURCE} WHERE {PREDICATE})"),
            [],
            |row| row.get(0),
        )?;
        if remaining {
            return Err(ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: SOURCE.into(),
            });
        }
        Ok(())
    }
}
