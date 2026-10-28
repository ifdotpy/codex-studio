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
        if !crate::receipt_migration_contract::source_exists(tx, SOURCE)? {
            return Ok(());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};
    use studio_runtime_schema::studio_errors::{
        Context, Diagnostics, EffectCertainty, HttpErrorKind, RecoveryPhase,
    };

    fn incident(id: &str, kind: Kind) -> Value {
        let error = StudioError {
            id: id.into(),
            kind,
            level: kind.level(),
            reason: kind.reason().into(),
            target: Target::Details,
            detail: "Saved diagnostic".into(),
            subject: "Subject".into(),
            at: 1.25,
            monitor_id: None,
            event_id: None,
            rule_id: None,
            count: None,
            first_at: None,
            diagnostics: Diagnostics {
                context: if kind == Kind::HttpFailure {
                    Context::Http {
                        route: "/saved".into(),
                        request_id: Some("original-request".into()),
                        status: 500,
                        class: HttpErrorKind::StoreDatabase,
                    }
                } else {
                    Context::Recovery {
                        account: "account".into(),
                        phase: RecoveryPhase::Scan,
                        agent_id: None,
                    }
                },
                certainty: EffectCertainty::Unknown,
                causes: vec![],
            },
        };
        let mut value = serde_json::to_value(error).expect("incident");
        value.as_object_mut().expect("object").remove("level");
        value
    }
    fn put(db: &Connection, id: &str, signature: &str, value: &Value) {
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
            params![id, signature, value.to_string()],
        )
        .expect("source");
    }
    fn run(db: &mut Connection) -> Result<()> {
        loop {
            let tx = db.transaction()?;
            crate::receipt_migration_contract::prepare_schema(&tx)?;
            let checkpoint = MigrationCheckpoint::begin(&tx, Family.id(), SOURCE, 1000)?;
            let next = Family.move_batch(&tx, SOURCE, &checkpoint)?;
            next.save(&tx, Family.id(), SOURCE, 1000)?;
            let complete = next.status == MigrationStatus::Complete;
            if complete {
                Family.finish(&tx)?;
            }
            tx.commit()?;
            if complete {
                return Ok(());
            }
        }
    }
    type IncidentSnapshot = (String, String, String, i64, i64, Option<i64>, String);
    fn rows(db: &Connection) -> Vec<IncidentSnapshot> {
        db.prepare("SELECT id,kind,owner_id,created,last_seen,dismissed_at,payload FROM runtime_error_incidents ORDER BY id").expect("query").query_map([],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?))).expect("rows").collect::<rusqlite::Result<_>>().expect("snapshot")
    }
    #[test]
    fn receipts_incidents_old_diagnostics_and_dismissals_are_equal_after_reopen() {
        let directory = tempfile::tempdir().expect("state");
        let path = directory.path().join("canvas.sqlite3");
        let mut db = Connection::open(&path).expect("database");
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT)").expect("source table");
        put(
            &db,
            "studio-errors:dismissed:http",
            "studio-error-dismissal",
            &json!({"at":2.5}),
        );
        put(
            &db,
            "studio-errors:incident:http",
            "studio-error-incident-v1",
            &incident("http", Kind::HttpFailure),
        );
        put(
            &db,
            "studio-errors:incident:recovery",
            "studio-error-incident-v1",
            &incident("recovery", Kind::RecoveryFailed),
        );
        put(
            &db,
            "studio-errors:dismissed:monitors:rule",
            "studio-error-dismissal",
            &json!({"at":3.0}),
        );
        run(&mut db).expect("migrate");
        let first = rows(&db);
        assert_eq!(first.len(), 3);
        assert_eq!(
            (&*first[0].0, &*first[0].2, first[0].3, first[0].5),
            ("http", "original-request", 1250, Some(2500))
        );
        assert_eq!(first[1].6, "null");
        assert_eq!(first[2].2, "account");
        let mut expected = incident("http", Kind::HttpFailure);
        expected["level"] = json!(Kind::HttpFailure.level());
        let original: StudioError =
            serde_json::from_value(expected).expect("canonical expected diagnostic");
        assert_eq!(
            serde_json::from_str::<StudioError>(&first[0].6).expect("canonical incident"),
            original
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |row| row
                .get::<_, i64>(0))
                .expect("replay"),
            0
        );
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        run(&mut db).expect("second open");
        assert_eq!(first, rows(&db));
    }
    #[test]
    fn receipts_incidents_unclassifiable_signature_rolls_back_dismissals() {
        let mut db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT)").expect("source table");
        put(
            &db,
            "studio-errors:dismissed:first",
            "studio-error-dismissal",
            &json!({"at":2.5}),
        );
        put(
            &db,
            "studio-errors:incident:bad",
            "unknown-signature",
            &json!({}),
        );
        assert!(
            matches!(run(&mut db),Err(ReceiptMigrationError::Unclassifiable {table,key,signature}) if table==SOURCE && key=="studio-errors:incident:bad" && signature=="unknown-signature")
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_operation_receipts",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("both sources"),
            2
        );
    }
}
