//! Move current account diagnostics and project lifetime fences to their owners.
use crate::receipt_migration_contract::{
    self as contract, MigrationCheckpoint, ReceiptFamilyMigration, Result,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    IdleEpisodeOutcome, MigrationStatus, ProjectRemovalReason,
};

pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &["runtime_operation_receipts"];
const TABLE: &str = "runtime_operation_receipts";
const PREDICATE: &str = "source.id GLOB 'liveness:session-start-failure:*' OR source.id GLOB 'idle-compaction:*' OR source.id GLOB 'project-destructive-removal:*'";
impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-owner-state"
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
        if source != TABLE {
            return Err(contract::unclassifiable(TABLE, source, self.id()));
        }
        let batch = contract::read_receipt_batch(tx, TABLE, checkpoint, PREDICATE)?;
        next.status = if batch.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Pending
        };
        for receipt in batch {
            move_row(tx, &receipt, checkpoint.cutover_ms)?;
            contract::delete_source(tx, TABLE, receipt.rowid)?;
            next.cursor = receipt.rowid;
        }
        next.save(tx, self.id(), source, checkpoint.cutover_ms)?;
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        if !contract::source_exists(tx, TABLE)? {
            return Ok(());
        }
        let remaining: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {TABLE} AS source WHERE {PREDICATE})"),
            [],
            |r| r.get(0),
        )?;
        if remaining {
            return Err(contract::ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: TABLE.into(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StartFailure {
    reason: String,
    at: f64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdleFire {
    outcome: IdleEpisodeOutcome,
    at: f64,
    fire_at: f64,
    context_tokens: u64,
    late_ms: f64,
    completed_at: Option<f64>,
    before: Option<Value>,
    limits: Option<Value>,
}

fn move_row(tx: &Transaction<'_>, row: &contract::SourceReceipt, cutover_ms: i64) -> Result<()> {
    let invalid = || contract::unclassifiable(TABLE, &row.id, &row.signature);
    if let Some(account) = row.id.strip_prefix("liveness:session-start-failure:") {
        if row.signature != "liveness" || account.is_empty() {
            return Err(invalid());
        }
        let failure: StartFailure = row.json(TABLE)?;
        if !failure.at.is_finite() {
            return Err(invalid());
        }
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,journal_ack,recovery_required,streams,start_failure) VALUES(?1,0,0,0,'null',?2) ON CONFLICT(account) DO UPDATE SET start_failure=excluded.start_failure",params![account,row.result])?;
    } else if let Some(project) = row.id.strip_prefix("project-destructive-removal:") {
        if row.signature != "project-destructive-removal" || project.is_empty() {
            return Err(invalid());
        }
        let sequence: i64 = row.json(TABLE)?;
        if sequence < 0 {
            return Err(invalid());
        }
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_projects WHERE id=?1",
                [project],
                |r| r.get(0),
            )
            .optional()?;
        let mut record = raw
            .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| invalid()))
            .transpose()?
            .unwrap_or_else(|| json!({"id":project}));
        let object = record.as_object_mut().ok_or_else(invalid)?;
        object.insert("removal".into(),json!({"reason":ProjectRemovalReason::Delete,"sequence":sequence,"requestId":row.id,"deletedAt":cutover_ms}));
        tx.execute("INSERT INTO runtime_projects(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![project,record.to_string()])?;
    } else if row.id.starts_with("idle-compaction:") {
        if row.signature != "idle-compaction" {
            return Err(invalid());
        }
        let fire: IdleFire = row.json(TABLE)?;
        if ![fire.at, fire.fire_at, fire.late_ms]
            .iter()
            .all(|n| n.is_finite())
            || fire.completed_at.is_some_and(|n| !n.is_finite())
        {
            return Err(invalid());
        }
        let mut statement=tx.prepare("SELECT id,record FROM runtime_agents WHERE CASE WHEN json_valid(record) THEN json_extract(record,'$.idleCompactionEpisode.id') END=?1 LIMIT 2")?;
        let owners = statement
            .query_map([&row.id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if owners.len() > 1 {
            return Err(invalid());
        }
        for (agent, raw) in owners {
            let mut record: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
            let episode = record["idleCompactionEpisode"]
                .as_object_mut()
                .ok_or_else(invalid)?;
            episode.insert("fired".into(), json!(true));
            episode.insert("outcome".into(), json!(fire.outcome));
            episode.insert("firedAt".into(), json!(fire.at));
            if let Some(before) = &fire.before {
                episode.insert("before".into(), before.clone());
            }
            if let Some(limits) = &fire.limits {
                episode.insert("limits".into(), limits.clone());
            }
            tx.execute(
                "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                params![agent, record.to_string()],
            )?;
        }
        // A superseded episode cannot be selected for admission again. Its native
        // action, if unresolved, has its own independent operation owner.
        let _context_tokens = fire.context_tokens;
    } else {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::{Value, json};
    fn fixture() -> Connection {
        fixture_at(None)
    }
    fn fixture_at(path: Option<&std::path::Path>) -> Connection {
        let mut db = path
            .map(Connection::open)
            .unwrap_or_else(Connection::open_in_memory)
            .expect("db");
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT); CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);").expect("source");
        let tx = db.transaction().expect("tx");
        contract::prepare_schema(&tx).expect("schema");
        Family.prepare(&tx, 1000).expect("prepare");
        MigrationCheckpoint::begin(&tx, Family.id(), SOURCES[0], 1000).expect("checkpoint");
        tx.commit().expect("commit");
        db
    }
    fn put(db: &Connection, id: &str, sig: &str, value: Value) {
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
            params![id, sig, value.to_string()],
        )
        .expect("fixture");
    }
    fn batch(db: &mut Connection) -> Result<MigrationCheckpoint> {
        let tx = db.transaction()?;
        let checkpoint = MigrationCheckpoint::load(&tx, Family.id(), TABLE)?.expect("checkpoint");
        let next = Family.move_batch(&tx, TABLE, &checkpoint)?;
        tx.commit()?;
        Ok(next)
    }
    #[test]
    fn receipts_owner_state_duplicate_source_refuses_and_preserves_bytes() {
        let mut db = fixture();
        let id = "liveness:session-start-failure:duplicate";
        let original = r#"{"reason":"first","reason":"second","at":1.0}"#;
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'liveness',?2)",
            params![id, original],
        )
        .expect("source");
        assert!(
            matches!(batch(&mut db), Err(contract::ReceiptMigrationError::Unclassifiable{table,key,..}) if table==TABLE && key==id)
        );
        let saved: String = db
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .expect("source remains");
        assert_eq!(saved, original);
    }
    #[test]
    fn receipts_owner_state_byte_budget_and_unknown_idle_are_preserved() {
        let mut db = fixture();
        for i in 0..2 {
            put(
                &db,
                &format!("liveness:session-start-failure:{i}"),
                "liveness",
                json!({"reason":"x".repeat(contract::MIGRATION_BATCH_BYTES/2),"at":1.0}),
            );
        }
        assert_eq!(batch(&mut db).expect("byte bounded").cursor, 1);
        assert_eq!(batch(&mut db).expect("tail").cursor, 2);
        let mut db = fixture();
        put(
            &db,
            "idle-compaction:unknown",
            "idle-compaction",
            json!({"outcome":"unknown","at":1.0,"fireAt":1.0,"contextTokens":100,"lateMs":0.0}),
        );
        assert!(matches!(
            batch(&mut db),
            Err(contract::ReceiptMigrationError::Unclassifiable { .. })
        ));
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("held"),
            1
        );
    }
    #[test]
    fn receipts_owner_state_fixture_and_second_open_noop() {
        let mut db = fixture();
        put(
            &db,
            "liveness:session-start-failure:a",
            "liveness",
            json!({"reason":"failed","at":1.5}),
        );
        put(
            &db,
            "idle-compaction:episode",
            "idle-compaction",
            json!({"outcome":"started","at":2.5,"fireAt":2.0,"contextTokens":100,"lateMs":500.0,"before":{"status":"idle"}}),
        );
        put(
            &db,
            "project-destructive-removal:p",
            "project-destructive-removal",
            json!(7),
        );
        db.execute("INSERT INTO runtime_agents VALUES('agent',?1)",[json!({"id":"agent","idleCompactionEpisode":{"id":"idle-compaction:episode","identity":{}}}).to_string()]).expect("agent");
        batch(&mut db).expect("migration");
        let failure: String = db
            .query_row(
                "SELECT start_failure FROM runtime_account_state WHERE account='a'",
                [],
                |r| r.get(0),
            )
            .expect("account owner");
        assert_eq!(
            serde_json::from_str::<Value>(&failure).expect("failure"),
            json!({"reason":"failed","at":1.5})
        );
        assert_eq!(
            db.query_row(
                "SELECT streams FROM runtime_account_state WHERE account='a'",
                [],
                |r| r.get::<_, String>(0)
            )
            .expect("absent stream"),
            "null"
        );
        let agent: String = db
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='agent'",
                [],
                |r| r.get(0),
            )
            .expect("agent");
        let agent: Value = serde_json::from_str(&agent).expect("agent");
        assert_eq!(agent["idleCompactionEpisode"]["fired"], true);
        assert_eq!(
            agent["idleCompactionEpisode"]["before"],
            json!({"status":"idle"})
        );
        let project: String = db
            .query_row(
                "SELECT record FROM runtime_projects WHERE id='p'",
                [],
                |r| r.get(0),
            )
            .expect("fence");
        assert_eq!(
            serde_json::from_str::<Value>(&project).expect("project")["removal"],
            json!({"reason":"delete","sequence":7,"requestId":"project-destructive-removal:p","deletedAt":1000})
        );
        assert_eq!(
            batch(&mut db).expect("second open").status,
            MigrationStatus::Complete
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("count"),
            0
        );
    }
    #[test]
    fn receipts_owner_state_batches_resume_and_rollback_unknown() {
        let directory = tempfile::tempdir().expect("temp database");
        let path = directory.path().join("canvas.sqlite3");
        let mut db = fixture_at(Some(&path));
        for i in 0..501 {
            put(
                &db,
                &format!("liveness:session-start-failure:a{i}"),
                "liveness",
                json!({"reason":"failed","at":1.0}),
            );
        }
        let first = batch(&mut db).expect("first batch");
        db.close().expect("interrupted between batches");
        let mut db = Connection::open(&path).expect("reopen");
        assert_eq!(first.cursor, 500);
        assert_eq!(first.status, MigrationStatus::Pending);
        {
            let tx = db.transaction().expect("tx");
            Family
                .move_batch(&tx, TABLE, &first)
                .expect("second uncommitted");
            tx.rollback().expect("crash");
        }
        assert_eq!(batch(&mut db).expect("resume").cursor, 501);
        put(
            &db,
            "liveness:session-start-failure:invalid",
            "wrong",
            json!({"status":"unknown"}),
        );
        db.execute("UPDATE runtime_operation_receipts SET rowid=502 WHERE id='liveness:session-start-failure:invalid'",[]).expect("stable source rowid");
        assert!(
            matches!(batch(&mut db),Err(contract::ReceiptMigrationError::Unclassifiable{table,key,signature}) if table==TABLE && key=="liveness:session-start-failure:invalid" && signature=="wrong")
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("preserved unknown"),
            1
        );
    }
}
