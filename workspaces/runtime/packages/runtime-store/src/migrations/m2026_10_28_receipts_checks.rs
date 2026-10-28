//! Validate physical enum columns in bounded pages before adding atomic CHECKs.
//! A virtual guard lets SQLite add table checks without copying domain history,
//! replacing rowids, or rebinding its existing triggers and indexes.
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptProjection, Result, read_source_batch,
};
use rusqlite::Transaction;
use serde::Deserialize;
use studio_runtime_schema::persistence::{
    EventKind, EventStatus, ExecutionEffectKind, MigrationStatus, enum_check,
};

pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &[
    "runtime_events",
    "runtime_execution_effects",
    "runtime_payload_migrations",
];

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-checks"
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
        let (key, signature, result) = match source {
            "runtime_events" => (
                "source.id",
                "source.kind || ':' || source.status",
                "json_object('kind',source.kind,'status',source.status)",
            ),
            "runtime_execution_effects" => (
                "source.id",
                "source.kind",
                "json_object('kind',source.kind)",
            ),
            "runtime_payload_migrations" => (
                "source.name",
                "source.status",
                "json_object('status',source.status)",
            ),
            _ => {
                return Err(crate::receipt_migration_contract::unclassifiable(
                    source,
                    source,
                    "unsupported checkpoint source",
                ));
            }
        };
        let rows = read_source_batch(
            tx,
            source,
            checkpoint,
            &ReceiptProjection {
                id: key,
                signature,
                result,
            },
            "1",
        )?;
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        for row in rows {
            match source {
                "runtime_events" => {
                    #[derive(Deserialize)]
                    struct Event {
                        kind: EventKind,
                        status: EventStatus,
                    }
                    let Event { kind, status } = row.json(source)?;
                    let _typed = (kind, status);
                }
                "runtime_execution_effects" => {
                    #[derive(Deserialize)]
                    struct Effect {
                        kind: ExecutionEffectKind,
                    }
                    let Effect { kind } = row.json(source)?;
                    let _typed = kind;
                }
                "runtime_payload_migrations" => {
                    #[derive(Deserialize)]
                    struct Checkpoint {
                        status: MigrationStatus,
                    }
                    let Checkpoint { status } = row.json(source)?;
                    let _typed = status;
                }
                _ => unreachable!("source was validated before query"),
            }
            next.cursor = row.rowid;
        }
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        for (table, checks) in [
            (
                "runtime_events",
                format!(
                    "{} {}",
                    enum_check::<EventKind>("kind"),
                    enum_check::<EventStatus>("status")
                ),
            ),
            (
                "runtime_execution_effects",
                enum_check::<ExecutionEffectKind>("kind"),
            ),
            (
                "runtime_payload_migrations",
                enum_check::<MigrationStatus>("status"),
            ),
        ] {
            let guarded:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo(?1) WHERE name='receipt_enum_guard')",[table],|row|row.get(0))?;
            if !guarded {
                tx.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN receipt_enum_guard INTEGER GENERATED ALWAYS AS (NULL) VIRTUAL {checks}"))?;
            }
        }
        Ok(())
    }
}
