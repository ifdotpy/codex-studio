//! Preserve execution history while moving exact attempt replies into replay storage.
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, ReceiptProjection, Result,
    admitted_owner_token, import_result, read_source_batch,
};
use crate::request_results::{AdmittedRequest, ResultEnvelope};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    AttemptSubmission, ExecutionEffectKind, ExecutionEffectStatus, ExecutionNodeStatus,
    ExecutionReceiptKind, ExecutionStatus, MigrationStatus,
};

pub(super) struct ExecutionMigration;
pub(super) const SOURCES: &[&str] = &[
    "runtime_execution_runs",
    "runtime_execution_attempts",
    "runtime_execution_effects",
    "runtime_execution_nodes",
    "runtime_agents",
];

fn classify<T: DeserializeOwned>(value: &Value, source: &str, id: &str, field: &str) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|_| ReceiptMigrationError::Unclassifiable {
        table: source.into(),
        key: id.into(),
        signature: format!("{field}={value}"),
    })
}
impl ReceiptFamilyMigration for ExecutionMigration {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-execution"
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
        if !SOURCES.contains(&source) {
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
                signature: "'execution-owner'",
                result: "source.record",
            },
            "1",
        )?;
        for row in batch {
            let (rowid, id, raw) = (row.rowid, row.id, row.result);
            let invalid = || ReceiptMigrationError::Unclassifiable {
                table: source.into(),
                key: id.clone(),
                signature: "execution-owner".into(),
            };
            let mut record: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
            if !record.is_object() {
                return Err(invalid());
            }
            let before = record.clone();
            match source {
                "runtime_execution_runs" => {
                    let _: ExecutionStatus = classify(&record["status"], source, &id, "status")?;
                }
                "runtime_execution_attempts" => {
                    let _: AttemptSubmission =
                        classify(&record["submission"], source, &id, "submission")?;
                    if !record["turnStatus"].is_null() {
                        let _: ExecutionStatus =
                            classify(&record["turnStatus"], source, &id, "turnStatus")?;
                    }
                    let run:String=tx.query_row("SELECT r.record FROM runtime_execution_runs r JOIN runtime_execution_attempts a ON a.run=r.id WHERE a.id=?1",[&id],|r|r.get(0)).optional()?.ok_or_else(invalid)?;
                    let run: Value = serde_json::from_str(&run).map_err(|_| invalid())?;
                    let actor = run["agent"].as_str().ok_or_else(invalid)?;
                    let epoch = record["epoch"].as_i64().ok_or_else(invalid)?;
                    let admission: AdmittedRequest = match record.get("admission") {
                        Some(value) => classify(value, source, &id, "admission")?,
                        None => admitted_owner_token(
                            &json!(["execution-attempt", id]).to_string(),
                            &json!({"agent":actor,"epoch":epoch,"attempt":id}).to_string(),
                            checkpoint.cutover_ms,
                        ),
                    };
                    if let Some(receipts) = record.get("receipts") {
                        let receipts = receipts.as_object().ok_or_else(invalid)?;
                        for (kind, receipt) in receipts {
                            let _: ExecutionReceiptKind =
                                classify(&json!(kind), source, &id, "receiptKind")?;
                            let input = receipt.get("input").ok_or_else(invalid)?;
                            let reply = receipt.get("result").ok_or_else(invalid)?;
                            let result = admission.derive_result(
                                "execution-attempt-result",
                                &[kind.as_str()],
                                &input.to_string(),
                            );
                            import_result(
                                tx,
                                result.id(),
                                result.signature(),
                                &ResultEnvelope {
                                    actor: None,
                                    reply,
                                    references: vec![id.clone()],
                                },
                                checkpoint.cutover_ms,
                            )?;
                        }
                    }
                    record
                        .as_object_mut()
                        .ok_or_else(invalid)?
                        .remove("receipts");
                    record["admission"] = serde_json::to_value(admission).map_err(|_| invalid())?;
                }
                "runtime_execution_effects" => {
                    let _: ExecutionEffectStatus =
                        classify(&record["status"], source, &id, "status")?;
                    let kind: String = tx.query_row(
                        "SELECT kind FROM runtime_execution_effects WHERE id=?1",
                        [&id],
                        |r| r.get(0),
                    )?;
                    let _: ExecutionEffectKind = classify(&json!(kind), source, &id, "kind")?;
                    if record["kind"] != kind {
                        return Err(invalid());
                    }
                }
                "runtime_execution_nodes" => {
                    if !record["status"].is_null() {
                        let _: ExecutionNodeStatus =
                            classify(&record["status"], source, &id, "status")?;
                    }
                }
                "runtime_agents" => {
                    // A fork may retain source-thread provenance within this same owner scope.
                    let account = match record.get("accountKey") {
                        None => "default",
                        Some(value) => value.as_str().ok_or_else(invalid)?,
                    };
                    let epoch = record["epoch"].as_i64().ok_or_else(invalid)?;
                    let pointer:Option<(String,Option<String>)>=tx.query_row("SELECT id,json_extract(record,'$.latestAttemptId') FROM runtime_execution_runs WHERE agent=?1 AND account=?2 AND epoch=?3 ORDER BY (id=(SELECT run FROM runtime_execution_attempts WHERE id=?4)) DESC,created DESC,rowid DESC LIMIT 1",params![id,account,epoch,record["startAttempt"]["id"].as_str()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                    if let Some((run, attempt)) = pointer {
                        record["activeExecution"] = json!({"runId":run,"attemptId":attempt});
                    } else {
                        record
                            .as_object_mut()
                            .ok_or_else(invalid)?
                            .remove("activeExecution");
                    }
                }
                _ => return Err(invalid()),
            }
            if record != before {
                tx.execute(
                    &format!("UPDATE {source} SET record=?1 WHERE rowid=?2"),
                    params![record.to_string(), rowid],
                )?;
            }
            next.cursor = rowid;
        }
        next.status = if tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {source} WHERE rowid>?1)"),
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
        if crate::receipt_migration_contract::source_exists(tx,"runtime_execution_attempts")? && tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_execution_attempts WHERE json_type(record,'$.receipts') IS NOT NULL)",[],|r|r.get::<_,bool>(0))? {
            return Err(ReceiptMigrationError::Incomplete{migration:self.id().into(),source_table:"runtime_execution_attempts".into()});
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt_migration_contract::{MIGRATION_BATCH_ROWS, prepare_schema};
    use rusqlite::Connection;
    fn fixture(db: &Connection) {
        db.execute_batch("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,created REAL,record TEXT); CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY,run TEXT,record TEXT); CREATE TABLE runtime_execution_effects(id TEXT PRIMARY KEY,run TEXT,kind TEXT,reference TEXT,record TEXT); CREATE TABLE runtime_execution_nodes(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);").expect("tables");
        db.execute("INSERT INTO runtime_execution_runs VALUES('run','agent','account',1,1,?1)",[json!({"id":"run","agent":"agent","accountKey":"account","epoch":1,"created":1,"status":"unknown","latestAttemptId":"attempt-0"}).to_string()]).expect("run");
        db.execute(
            "INSERT INTO runtime_agents VALUES('agent',?1)",
            [json!({"id":"agent","rootId":"agent","status":"idle","epoch":1,"accountKey":"account","startAttempt":{"id":"attempt-0"}}).to_string()],
        )
        .expect("agent");
    }
    fn insert(db: &Connection, n: usize, status: &str) {
        db.execute("INSERT INTO runtime_execution_attempts VALUES(?1,'run',?2)",params![format!("attempt-{n}"),json!({"id":format!("attempt-{n}"),"epoch":1,"submission":status,"receipts":{"submission":{"input":{"version":n},"result":{"submitted":true}}}}).to_string()]).expect("attempt");
    }
    fn batch(db: &mut Connection, source: &str) -> Result<MigrationCheckpoint> {
        let tx = db.transaction()?;
        prepare_schema(&tx)?;
        let cp = MigrationCheckpoint::begin(&tx, ExecutionMigration.id(), source, 1000)?;
        let next = ExecutionMigration.move_batch(&tx, source, &cp)?;
        next.save(&tx, ExecutionMigration.id(), source, 1000)?;
        tx.commit()?;
        Ok(next)
    }
    #[test]
    fn receipts_execution_fixture_reopen_preserves_unknown_and_exact_reply() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        insert(&db, 0, "unknown");
        for source in SOURCES {
            batch(&mut db, source).expect("migrate");
        }
        let raw: String = db
            .query_row("SELECT record FROM runtime_execution_attempts", [], |r| {
                r.get(0)
            })
            .expect("attempt");
        let record: Value = serde_json::from_str(&raw).expect("JSON");
        assert_eq!(record["submission"], "unknown");
        assert!(record.get("receipts").is_none());
        let token: AdmittedRequest =
            serde_json::from_value(record["admission"].clone()).expect("admission");
        let derived = token.derive_result(
            "execution-attempt-result",
            &["submission"],
            &json!({"version":0}).to_string(),
        );
        let reply: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [derived.id()],
                |r| r.get(0),
            )
            .expect("exact result");
        assert_eq!(
            serde_json::from_str::<Value>(&reply).expect("JSON")["reply"],
            json!({"submitted":true})
        );
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        for source in SOURCES {
            batch(&mut db, source).expect("second open");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("count"),
            1
        );
    }
    #[test]
    fn receipts_execution_crash_resumes_and_unknown_kind_aborts() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        {
            let setup = db.transaction().expect("fixture transaction");
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert(&setup, n, "unsent");
            }
            setup.commit().expect("fixture commit");
        }
        assert_eq!(batch(&mut db, SOURCES[1]).expect("batch").cursor, 500);
        {
            let tx = db.transaction().expect("tx");
            let cp = MigrationCheckpoint::load(&tx, ExecutionMigration.id(), SOURCES[1])
                .expect("load")
                .expect("checkpoint");
            ExecutionMigration
                .move_batch(&tx, SOURCES[1], &cp)
                .expect("uncommitted batch");
        }
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        assert_eq!(batch(&mut db, SOURCES[1]).expect("resume").cursor, 501);
        db.execute(
            "INSERT INTO runtime_execution_effects(id,kind,record) VALUES('bad','new-kind',?1)",
            [json!({"kind":"new-kind","status":"pending"}).to_string()],
        )
        .expect("bad fixture");
        assert!(
            matches!(batch(&mut db,SOURCES[2]),Err(ReceiptMigrationError::Unclassifiable{table,key,signature:_}) if table==SOURCES[2]&&key=="bad")
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_execution_effects", [], |r| {
                r.get::<_, i64>(0)
            })
            .expect("preserved"),
            1
        );
    }
    #[test]
    fn receipts_execution_current_pointer_excludes_previous_account_and_epoch() {
        for (account, epoch) in [("account", 2), ("moved-account", 1)] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&db);
            db.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'",[json!({"id":"agent","rootId":"agent","status":"idle","accountKey":account,"epoch":epoch,"startAttempt":{"id":"old-attempt"}}).to_string()]).expect("advanced owner");
            db.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES('old-attempt','run','{}')",[]).expect("old start identity");
            batch(&mut db, "runtime_agents").expect("pointer hydration");
            let raw: String = db
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id='agent'",
                    [],
                    |r| r.get(0),
                )
                .expect("owner");
            let agent: Value = serde_json::from_str(&raw).expect("JSON");
            let actor = crate::AgentRecord::from_value(agent.clone()).expect("current actor");
            assert_eq!(
                crate::WorkItemsRepository::submission_identity(&db, &actor)
                    .expect("current identity has no history fallback"),
                None
            );
            assert!(
                agent.get("activeExecution").is_none(),
                "previous account/epoch history cannot become current: {agent}"
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM runtime_execution_runs", [], |r| r
                    .get::<_, i64>(0))
                    .expect("history"),
                1
            );
        }
    }
    #[test]
    fn receipts_execution_current_pointer_retains_terminal_fork_provenance_and_nullable_attempt() {
        for attempt in [Some("current-attempt"), None] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&db);
            db.execute("UPDATE runtime_agents SET record=?1 WHERE id='agent'",[json!({"id":"agent","rootId":"agent","status":"idle","accountKey":"account","epoch":2,"threadId":"fork-thread","startAttempt":{"id":"old-attempt"}}).to_string()]).expect("owner after fork");
            db.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES('old-attempt','run','{}')",[]).expect("old start identity");
            db.execute("INSERT INTO runtime_execution_runs VALUES('current','agent','account',2,2,?1)",[json!({"id":"current","agent":"agent","accountKey":"account","epoch":2,"threadId":"source-thread","created":2,"status":"completed","latestAttemptId":attempt}).to_string()]).expect("terminal current run");
            db.execute("INSERT INTO runtime_execution_runs VALUES('other','agent','other-account',2,10,?1)",[json!({"id":"other","agent":"agent","accountKey":"other-account","epoch":2,"created":10,"status":"running","latestAttemptId":"other-attempt"}).to_string()]).expect("newer foreign-account history");
            batch(&mut db, "runtime_agents").expect("current identity");
            let raw: String = db
                .query_row("SELECT record FROM runtime_agents", [], |r| r.get(0))
                .expect("owner");
            let owner: Value = serde_json::from_str(&raw).expect("JSON");
            assert_eq!(
                owner["activeExecution"],
                json!({"runId":"current","attemptId":attempt})
            );
            let actor = crate::AgentRecord::from_value(owner).expect("current actor");
            assert_eq!(
                crate::WorkItemsRepository::submission_identity(&db, &actor)
                    .expect("migrated pointer is usable by work submission"),
                Some(("current".into(), attempt.map(str::to_owned)))
            );
        }
    }
    #[test]
    fn receipts_execution_current_start_is_preferred_over_newer_same_scope_history() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        insert(&db, 0, "unknown");
        db.execute("INSERT INTO runtime_execution_runs VALUES('newer','agent','account',1,10,?1)",[json!({"id":"newer","agent":"agent","accountKey":"account","epoch":1,"created":10,"status":"running","latestAttemptId":"other-attempt"}).to_string()]).expect("newer history");
        batch(&mut db, "runtime_agents").expect("prefer exact admitted start");
        let raw: String = db
            .query_row("SELECT record FROM runtime_agents", [], |r| r.get(0))
            .expect("owner");
        let actor = crate::AgentRecord::from_value(serde_json::from_str(&raw).expect("JSON"))
            .expect("actor");
        assert_eq!(
            crate::WorkItemsRepository::submission_identity(&db, &actor)
                .expect("submission identity"),
            Some(("run".into(), Some("attempt-0".into())))
        );
    }
    #[test]
    fn receipts_execution_frozen_monitor_lost_history_is_preserved_on_reopen() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        db.execute_batch("CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT)")
            .expect("monitor owner");
        let monitor = "6a1cbd51-e035-54ce-af5c-7fd9850e3065";
        let id = format!("monitors:{monitor}");
        let record = json!({"id":id,"runId":"run","kind":"monitors","referenceId":monitor,"status":"lost","outcome":null,"resultReference":null});
        db.execute("INSERT INTO runtime_execution_effects(id,run,kind,reference,record) VALUES(?1,'run','monitors',?2,?3)",params![id,monitor,record.to_string()]).expect("frozen effect");
        db.execute(
            "INSERT INTO runtime_monitors(id,record) VALUES(?1,?2)",
            params![monitor, json!({"id":monitor,"status":"lost"}).to_string()],
        )
        .expect("conclusively unavailable owner");
        batch(&mut db, "runtime_execution_effects").expect("classify frozen domains");
        let before: (String, String) = db
            .query_row(
                "SELECT kind,record FROM runtime_execution_effects WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("preserved");
        assert_eq!(before, ("monitors".into(), record.to_string()));
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("no runnable effect"),
            0
        );
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        batch(&mut db, "runtime_execution_effects").expect("second open");
        let after: (String, String) = db
            .query_row(
                "SELECT kind,record FROM runtime_execution_effects WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("same history");
        assert_eq!(before, after);
    }
}
