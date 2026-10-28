//! Move source-turn reconciliation proof onto its original execution attempt.
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, ReceiptProjection, Result,
    read_source_batch,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::Value;
use studio_runtime_schema::persistence::{ContextRepairPhase, MigrationStatus};

pub(super) struct ContextRepairMigration;
pub(super) const SOURCES: &[&str] = &["runtime_context_terminal_receipts", "runtime_agents"];

fn hydrate_agents(
    tx: &Transaction<'_>,
    checkpoint: &MigrationCheckpoint,
) -> Result<MigrationCheckpoint> {
    use crate::receipt_migration_contract::{
        ReceiptProjection, admitted_owner_token, read_source_batch, source_exists,
    };
    use crate::request_results::AdmittedRequest;
    use studio_runtime_providers::Method;
    use studio_runtime_schema::provider_call_contract::{ProviderCallIntent, transport_key};
    let mut next = checkpoint.clone();
    let source = SOURCES[1];
    if !source_exists(tx, source)? {
        next.status = MigrationStatus::Complete;
        return Ok(next);
    }
    for row in read_source_batch(
        tx,
        source,
        checkpoint,
        &ReceiptProjection {
            id: "source.id",
            signature: "'context-repair-owner'",
            result: "source.record",
        },
        "1",
    )? {
        let invalid = || ReceiptMigrationError::Unclassifiable {
            table: source.into(),
            key: row.id.clone(),
            signature: row.signature.clone(),
        };
        let mut agent: Value = row.json(source)?;
        if !agent.is_object() {
            return Err(invalid());
        }
        if agent["contextRepair"].is_object() {
            let mut repair = agent["contextRepair"].clone();
            let _: ContextRepairPhase =
                serde_json::from_value(repair["phase"].clone()).map_err(|_| invalid())?;
            let id = repair["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or_else(invalid)?;
            let operation = format!("context-repair:{id}:fork");
            let account = repair["source"]["accountKey"]
                .as_str()
                .ok_or_else(invalid)?;
            let key = format!("account-worker:{}", transport_key(account, &operation));
            let raw: Option<String> = if source_exists(tx, "runtime_operation_receipts")? {
                tx.query_row(
                    "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                    [format!("{key}:submitted")],
                    |r| r.get(0),
                )
                .optional()?
            } else {
                None
            };
            let mut provider_root = None;
            let token = if let Some(raw) = raw {
                let mut old: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
                if !old["call"].is_object() {
                    return Err(invalid());
                }
                old["call"]["root_request_id"] = serde_json::json!("");
                old["call"]["admitted_request"] = Value::Null;
                let call: ProviderCallIntent<Method, AdmittedRequest> =
                    serde_json::from_value(old["call"].clone()).map_err(|_| invalid())?;
                if call.operation != operation
                    || call.method != Method::ThreadFork
                    || call.scope.account != account
                    || call.actor.as_ref().is_none_or(|a| a.agent != row.id)
                {
                    return Err(invalid());
                }
                provider_root = Some(crate::receipt_migration_contract::hydrate_source_root(
                    tx,
                    "runtime_operation_receipts",
                    &format!("{key}:submitted"),
                    "submitted",
                    "result",
                    "$.call.root_request_id",
                )?);
                Some(admitted_owner_token(
                    &format!("{key}:result"),
                    &call.fingerprint(account, call.method.as_str()),
                    checkpoint.cutover_ms,
                ))
            } else {
                None
            };
            repair["rootRequestId"] =
                serde_json::json!(provider_root.unwrap_or_else(
                    studio_runtime_schema::persistence::RequestIdentity::issue_root
                ));
            repair["admittedRequest"] = serde_json::json!(token);
            agent["contextRepair"] = repair;
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE rowid=?2",
                params![agent.to_string(), row.rowid],
            )?;
        }
        next.cursor = row.rowid;
    }
    next.status = if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE rowid>?1)",
        [next.cursor],
        |r| r.get::<_, bool>(0),
    )? {
        MigrationStatus::Running
    } else {
        MigrationStatus::Complete
    };
    Ok(next)
}

impl ReceiptFamilyMigration for ContextRepairMigration {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-context-repair"
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
        let mut next = checkpoint.clone();
        if checkpoint.status == MigrationStatus::Complete {
            return Ok(next);
        }
        if source == SOURCES[1] {
            return hydrate_agents(tx, checkpoint);
        }
        if source != SOURCES[0] {
            return Err(ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: source.into(),
            });
        }
        if !crate::receipt_migration_contract::source_exists(tx, source)? {
            next.status = MigrationStatus::Complete;
            return Ok(next);
        }
        let batch = read_source_batch(
            tx,
            source,
            checkpoint,
            &ReceiptProjection {
                id: "source.id",
                signature: "source.operation",
                result: "source.record",
            },
            "1",
        )?;
        for row in batch {
            let (rowid, id, operation, raw) = (row.rowid, row.id, row.signature, row.result);
            let agent: String = tx.query_row(
                "SELECT agent FROM runtime_context_terminal_receipts WHERE rowid=?1",
                [rowid],
                |r| r.get(0),
            )?;
            let invalid = || ReceiptMigrationError::Unclassifiable {
                table: source.into(),
                key: id.clone(),
                signature: operation.clone(),
            };
            let record: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
            let phase: ContextRepairPhase =
                serde_json::from_value(record["phase"].clone()).map_err(|_| invalid())?;
            let account = record["source"]["accountKey"]
                .as_str()
                .ok_or_else(invalid)?;
            let thread = record["source"]["threadId"].as_str().ok_or_else(invalid)?;
            let turn = record["proof"]["turnId"].as_str().ok_or_else(invalid)?;
            if record["operationId"] != operation
                || record["source"]["id"] != agent
                || record["proof"]["threadId"] != thread
                || crate::python_json_compact(&serde_json::json!([account, thread, turn])) != id
            {
                return Err(invalid());
            }
            let attempt = record["startAttemptSnapshot"]["id"]
                .as_str()
                .filter(|id| !id.is_empty());
            let Some(attempt) = attempt else {
                // The old callback reader returned None without an attempt identity.
                // A completed proof therefore cannot settle or authorize any work.
                if phase == ContextRepairPhase::Completed
                    && record["startAttemptSnapshot"]["id"].is_null()
                {
                    tx.execute(
                        "DELETE FROM runtime_context_terminal_receipts WHERE rowid=?1",
                        [rowid],
                    )?;
                    next.cursor = rowid;
                    continue;
                }
                return Err(invalid());
            };
            let original: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_execution_attempts WHERE id=?1",
                    [attempt],
                    |r| r.get(0),
                )
                .optional()?;
            let original: Value =
                serde_json::from_str(&original.ok_or_else(invalid)?).map_err(|_| invalid())?;
            if !original.is_object()
                || original["id"] != attempt
                || original["epoch"] != record["source"]["epoch"]
            {
                return Err(invalid());
            }
            if original
                .get("terminalRepair")
                .is_some_and(|proof| proof != &record)
            {
                return Err(invalid());
            }
            tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.terminalRepair',json(?1)) WHERE id=?2",params![raw,attempt])?;
            tx.execute(
                "DELETE FROM runtime_context_terminal_receipts WHERE rowid=?1",
                [rowid],
            )?;
            next.cursor = rowid;
        }
        next.status = if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_context_terminal_receipts WHERE rowid>?1)",
            [next.cursor],
            |r| r.get::<_, bool>(0),
        )? {
            MigrationStatus::Running
        } else {
            MigrationStatus::Complete
        };
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        if crate::receipt_migration_contract::source_exists(tx, SOURCES[0])?
            && tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_context_terminal_receipts)",
                [],
                |r| r.get::<_, bool>(0),
            )?
        {
            return Err(ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: SOURCES[0].into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt_migration_contract::{MIGRATION_BATCH_ROWS, prepare_schema};
    use rusqlite::Connection;
    use serde_json::json;
    fn fixture(db: &Connection) {
        db.execute_batch("CREATE TABLE runtime_context_terminal_receipts(id TEXT PRIMARY KEY,agent TEXT,operation TEXT,record TEXT); CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,record TEXT NOT NULL);").expect("source");
    }
    fn insert(db: &Connection, n: usize, phase: &str) -> (String, Value) {
        let attempt = format!("attempt-{n}");
        let id = crate::python_json_compact(&json!(["account", "thread", format!("turn-{n}")]));
        let record = json!({"operationId":format!("repair-{n}"),"source":{"id":"agent","accountKey":"account","threadId":"thread","epoch":1},"proof":{"threadId":"thread","turnId":format!("turn-{n}"),"status":"completed"},"startAttemptSnapshot":{"id":attempt},"connectionId":"connection","connectionGeneration":1,"newThreadId":"new-thread","phase":phase});
        db.execute(
            "INSERT INTO runtime_execution_attempts VALUES(?1,?2)",
            params![attempt, json!({"id":attempt,"epoch":1}).to_string()],
        )
        .expect("attempt");
        db.execute(
            "INSERT INTO runtime_context_terminal_receipts VALUES(?1,'agent',?2,?3)",
            params![id, format!("repair-{n}"), record.to_string()],
        )
        .expect("proof");
        (attempt, record)
    }
    fn batch(db: &mut Connection) -> Result<MigrationCheckpoint> {
        let tx = db.transaction()?;
        prepare_schema(&tx)?;
        let cp = MigrationCheckpoint::begin(&tx, ContextRepairMigration.id(), SOURCES[0], 1000)?;
        let next = ContextRepairMigration.move_batch(&tx, SOURCES[0], &cp)?;
        next.save(&tx, ContextRepairMigration.id(), SOURCES[0], 1000)?;
        tx.commit()?;
        Ok(next)
    }
    #[test]
    fn receipts_context_repair_fixture_reopen_and_unknown_preserved() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        let (attempt, proof) = insert(&db, 0, "unknown");
        assert_eq!(
            batch(&mut db).expect("migrate").status,
            MigrationStatus::Complete
        );
        let saved: String = db
            .query_row(
                "SELECT record FROM runtime_execution_attempts WHERE id=?1",
                [&attempt],
                |r| r.get(0),
            )
            .expect("owner");
        assert_eq!(
            serde_json::from_str::<Value>(&saved).expect("JSON")["terminalRepair"],
            proof
        );
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        let before = db.total_changes();
        batch(&mut db).expect("second open");
        assert_eq!(
            db.total_changes() - before,
            1,
            "only runner checkpoint save"
        );
    }
    #[test]
    fn receipts_context_repair_crash_between_batches_resumes_and_bad_row_aborts() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        {
            let setup = db.transaction().expect("fixture transaction");
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert(&setup, n, "completed");
            }
            setup.commit().expect("fixture commit");
        }
        assert_eq!(batch(&mut db).expect("first batch").cursor, 500);
        {
            let tx = db.transaction().expect("tx");
            let cp = MigrationCheckpoint::load(&tx, ContextRepairMigration.id(), SOURCES[0])
                .expect("load")
                .expect("checkpoint");
            ContextRepairMigration
                .move_batch(&tx, SOURCES[0], &cp)
                .expect("uncommitted batch");
        }
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        assert_eq!(batch(&mut db).expect("resume").cursor, 501);
        let tx = db.transaction().expect("tx");
        ContextRepairMigration.finish(&tx).expect("complete");
        tx.commit().expect("commit");
        insert(&db, 999, "not-a-phase");
        db.execute(
            "DELETE FROM runtime_payload_migrations WHERE name!='receipts:cutover'",
            [],
        )
        .expect("reset test checkpoint");
        let err = batch(&mut db).expect_err("unclassifiable must abort");
        assert!(
            matches!(err,ReceiptMigrationError::Unclassifiable{table,key:_,signature} if table==SOURCES[0]&&signature=="repair-999")
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_context_terminal_receipts",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("source retained"),
            1
        );
    }
    #[test]
    fn receipts_context_repair_obsolete_without_attempt_deleted_and_owned_proof_retained() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        let (attempt, owned) = insert(&db, 0, "completed");
        let (_, mut obsolete) = insert(&db, 1, "completed");
        obsolete["startAttemptSnapshot"] = Value::Null;
        db.execute(
            "UPDATE runtime_context_terminal_receipts SET record=?1 WHERE operation='repair-1'",
            [obsolete.to_string()],
        )
        .expect("old idle proof");
        batch(&mut db).expect("classify both");
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_context_terminal_receipts",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("obsolete count"),
            0
        );
        let saved: String = db
            .query_row(
                "SELECT record FROM runtime_execution_attempts WHERE id=?1",
                [attempt],
                |r| r.get(0),
            )
            .expect("original attempt");
        assert_eq!(
            serde_json::from_str::<Value>(&saved).expect("JSON")["terminalRepair"],
            owned
        );
        let untouched: String = db
            .query_row(
                "SELECT record FROM runtime_execution_attempts WHERE id='attempt-1'",
                [],
                |r| r.get(0),
            )
            .expect("no invented owner");
        assert!(
            serde_json::from_str::<Value>(&untouched)
                .expect("JSON")
                .get("terminalRepair")
                .is_none()
        );
    }
}
