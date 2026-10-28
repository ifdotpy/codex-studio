//! Move exact tool replies to their replay owner and aliases to the current task.
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    MigrationStatus, ToolRequestKind, ToolRequestOutcome, ToolRequestStatus,
};

use crate::receipt_migration_contract::{
    BatchBudget, MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError,
    ReceiptProjection, Result, SourceReceipt, admitted_owner_token, delete_source, import_result,
    read_source_batch, unclassifiable,
};
use crate::request_results::ResultEnvelope;

pub(super) const ID: &str = "2026-10-28-receipts-tools";
/// Aliases and saved replies move before slimming the task record.
pub(super) const SOURCES: &[&str] = &[
    "runtime_tool_request_aliases",
    "runtime_tool_results",
    "runtime_tool_requests",
];
pub(super) struct Family;

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        ID
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
        if checkpoint.status == MigrationStatus::Complete {
            return Ok(checkpoint.clone());
        }
        let projection = match source {
            "runtime_tool_request_aliases" => ReceiptProjection {
                id: "json_array(source.agent,source.alias,source.request)",
                signature: "coalesce((SELECT CASE WHEN json_valid(record) THEN json_extract(record,'$.signature') ELSE '' END FROM runtime_tool_requests WHERE id=source.request),'')",
                result: "json_object('agent',source.agent,'alias',source.alias,'request',source.request)",
            },
            "runtime_tool_results" => ReceiptProjection {
                id: "source.id",
                signature: "coalesce((SELECT CASE WHEN json_valid(record) THEN json_extract(record,'$.signature') ELSE '' END FROM runtime_tool_requests WHERE id=source.id),'')",
                result: "source.result",
            },
            "runtime_tool_requests" => ReceiptProjection {
                id: "source.id",
                signature: "CASE WHEN json_valid(source.record) THEN coalesce(json_extract(source.record,'$.signature'),'') ELSE '' END",
                result: "source.record",
            },
            _ => {
                return Err(ReceiptMigrationError::Incomplete {
                    migration: ID.into(),
                    source_table: source.into(),
                });
            }
        };
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
            [source],
            |r| r.get(0),
        )?;
        let rows = if exists {
            read_source_batch(tx, source, checkpoint, &projection, "1")?
        } else {
            Vec::new()
        };
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        let mut budget = BatchBudget::default();
        for row in rows {
            let decoded: Value = row.json(source)?;
            let blob_bytes = decoded
                .pointer("/_payloadBlobs/result/size")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let input = row
                .id
                .len()
                .saturating_add(row.signature.len())
                .saturating_add(row.result.len())
                .saturating_add(usize::try_from(blob_bytes).unwrap_or(usize::MAX));
            if !budget.include(input) {
                break;
            }
            match source {
                "runtime_tool_request_aliases" => {
                    let alias = decoded;
                    let target = alias["request"]
                        .as_str()
                        .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
                    let mut task = task(tx, source, &row, target)?;
                    if task["agent"] != alias["agent"] || !alias["alias"].is_string() {
                        return Err(unclassifiable(source, &row.id, &row.signature));
                    }
                    if task.get("aliases").is_none() {
                        task["aliases"] = json!([]);
                    }
                    let aliases = task["aliases"]
                        .as_array_mut()
                        .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
                    if !aliases.contains(&alias["alias"]) {
                        aliases.push(alias["alias"].clone());
                    }
                    tx.execute(
                        "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                        params![task.to_string(), target],
                    )?;
                    delete_source(tx, source, row.rowid)?;
                }
                "runtime_tool_results" => {
                    let task = task(tx, source, &row, &row.id)?;
                    let result = decoded;
                    save_result(tx, source, &row, &task, result, checkpoint.cutover_ms)?;
                    delete_source(tx, source, row.rowid)?;
                }
                "runtime_tool_requests" => {
                    let mut task = decoded;
                    if task["kind"] == ToolRequestKind::ProviderCallback.as_str() {
                        // Callback generation identity already has its own typed owner.
                        if !task["payload"].is_object() {
                            return Err(unclassifiable(source, &row.id, &row.signature));
                        }
                    } else {
                        classify(source, &row, &task)?;
                        let result = if let Some(reference) = task.pointer("/_payloadBlobs/result")
                        {
                            Some(load_blob(tx, source, &row, reference)?)
                        } else {
                            task.get("result").filter(|v| !v.is_null()).cloned()
                        };
                        if let Some(result) = result {
                            save_result(tx, source, &row, &task, result, checkpoint.cutover_ms)?;
                        }
                        let object = task
                            .as_object_mut()
                            .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
                        object.insert("kind".into(), json!(ToolRequestKind::ManagedTool));
                        object.entry("originKey").or_insert(Value::Null);
                        object.entry("aliases").or_insert(json!([]));
                        object.insert(
                            "admitted".into(),
                            json!(admitted_owner_token(
                                &row.id,
                                &row.signature,
                                checkpoint.cutover_ms
                            )),
                        );
                        object.remove("result");
                        if let Some(refs) = object
                            .get_mut("_payloadBlobs")
                            .and_then(Value::as_object_mut)
                        {
                            refs.remove("result");
                            if refs.is_empty() {
                                object.remove("_payloadBlobs");
                            }
                        }
                        tx.execute(
                            "UPDATE runtime_tool_requests SET record=?1 WHERE rowid=?2",
                            params![task.to_string(), row.rowid],
                        )?;
                    }
                }
                _ => unreachable!("source matched before scanning"),
            }
            next.cursor = row.rowid;
        }
        Ok(next)
    }

    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        for source in SOURCES {
            if !MigrationCheckpoint::load(tx, ID, source)?
                .is_some_and(|checkpoint| checkpoint.status == MigrationStatus::Complete)
            {
                return Err(ReceiptMigrationError::Incomplete {
                    migration: ID.into(),
                    source_table: (*source).into(),
                });
            }
        }
        Ok(())
    }
}

fn task(tx: &Transaction<'_>, source: &str, row: &SourceReceipt, id: &str) -> Result<Value> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_tool_requests WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let value: Value = raw
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
    let task_row = SourceReceipt {
        rowid: row.rowid,
        id: id.into(),
        signature: row.signature.clone(),
        result: String::new(),
    };
    classify(source, &task_row, &value)?;
    Ok(value)
}

fn classify(source: &str, row: &SourceReceipt, task: &Value) -> Result<()> {
    let invalid = || unclassifiable(source, &row.id, &row.signature);
    if task["id"] != row.id
        || !task["agent"].is_string()
        || !task["tool"].is_string()
        || task["signature"] != row.signature
        || row.signature.is_empty()
    {
        return Err(invalid());
    }
    let status: ToolRequestStatus =
        serde_json::from_value(task["stage"].clone()).map_err(|_| invalid())?;
    let outcome: ToolRequestOutcome =
        serde_json::from_value(task["outcome"].clone()).map_err(|_| invalid())?;
    let valid = matches!(
        (status, outcome),
        (
            ToolRequestStatus::Queued | ToolRequestStatus::Running,
            ToolRequestOutcome::Pending
        ) | (
            ToolRequestStatus::Running
                | ToolRequestStatus::Failed
                | ToolRequestStatus::Unknown
                | ToolRequestStatus::Interrupted,
            ToolRequestOutcome::Unknown
        ) | (
            ToolRequestStatus::Completed,
            ToolRequestOutcome::Applied | ToolRequestOutcome::NotApplied
        ) | (
            ToolRequestStatus::Failed
                | ToolRequestStatus::Cancelled
                | ToolRequestStatus::Interrupted,
            ToolRequestOutcome::NotApplied
        )
    );
    if !valid {
        return Err(invalid());
    }
    Ok(())
}

fn save_result(
    tx: &Transaction<'_>,
    source: &str,
    row: &SourceReceipt,
    task: &Value,
    mut result: Value,
    cutover: i64,
) -> Result<()> {
    if let Some(refs) = result
        .as_object_mut()
        .and_then(|r| r.remove("_payloadBlobs"))
    {
        let refs = refs
            .as_object()
            .ok_or_else(|| unclassifiable(source, &row.id, &row.signature))?;
        for (field, reference) in refs {
            result[field] = load_blob(tx, source, row, reference)?;
        }
    }
    if !studio_runtime_schema::validate("PersistedToolResult", &result).unwrap_or(false) {
        return Err(unclassifiable(source, &row.id, &row.signature));
    }
    let result = crate::ToolResultRecord::from_value(result)
        .map_err(|_| unclassifiable(source, &row.id, &row.signature))?;
    import_result(
        tx,
        &row.id,
        &row.signature,
        &ResultEnvelope {
            actor: task["agent"].as_str().map(str::to_owned),
            reply: result,
            references: Vec::new(),
        },
        cutover,
    )
}

fn load_blob(
    tx: &Transaction<'_>,
    source: &str,
    row: &SourceReceipt,
    reference: &Value,
) -> Result<Value> {
    let invalid = || unclassifiable(source, &row.id, &row.signature);
    let digest = reference["sha256"].as_str().ok_or_else(invalid)?;
    let size = reference["size"].as_u64().ok_or_else(invalid)?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    let root = tx
        .path()
        .and_then(|p| std::path::Path::new(p).parent())
        .ok_or_else(invalid)?;
    let mut path = root.to_path_buf();
    for component in ["blobs", &digest[..2], &digest[2..4], digest] {
        path.push(component);
        if std::fs::symlink_metadata(&path)
            .map_err(|_| invalid())?
            .file_type()
            .is_symlink()
        {
            return Err(invalid());
        }
    }
    use std::io::Read;
    let file = std::fs::File::open(&path).map_err(|_| invalid())?;
    let metadata = file.metadata().map_err(|_| invalid())?;
    if !metadata.is_file() || metadata.len() != size {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(size.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() as u64 != size || crate::sha256_hex(&bytes) != digest {
        return Err(invalid());
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt_migration_contract::{MIGRATION_BATCH_ROWS, prepare_schema};
    use rusqlite::Connection;

    const CUTOVER: i64 = 1_000_000;

    fn fixture(db: &mut Connection) {
        db.execute_batch("CREATE TABLE runtime_tool_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_tool_results(id TEXT PRIMARY KEY,result TEXT NOT NULL); CREATE TABLE runtime_tool_request_aliases(agent TEXT,alias TEXT,request TEXT,PRIMARY KEY(agent,alias,request));").expect("old schema");
        let tx = db.transaction().expect("transaction");
        prepare_schema(&tx).expect("new schema");
        tx.commit().expect("schema commit");
    }

    fn insert(db: &Connection, id: &str, stage: &str, outcome: &str) {
        let task = json!({"id":id,"agent":"agent","tool":"orchestration_send","signature":"signature","stage":stage,"outcome":outcome,"result":{"success":true,"contentItems":[]}});
        db.execute(
            "INSERT INTO runtime_tool_requests VALUES(?1,?2)",
            params![id, task.to_string()],
        )
        .expect("old task");
    }

    fn batch(db: &mut Connection, source: &str, commit: bool) -> MigrationCheckpoint {
        let tx = db.transaction().expect("transaction");
        let checkpoint = MigrationCheckpoint::begin(&tx, ID, source, CUTOVER).expect("checkpoint");
        let next = Family
            .move_batch(&tx, source, &checkpoint)
            .expect("move batch");
        next.save(&tx, ID, source, CUTOVER).expect("save");
        if commit {
            tx.commit().expect("commit");
        } else {
            tx.rollback().expect("crash");
        }
        next
    }

    fn migrate(db: &mut Connection) {
        let tx = db.transaction().expect("transaction");
        Family.prepare(&tx, CUTOVER).expect("prepare");
        tx.commit().expect("prepare commit");
        for source in SOURCES {
            while batch(db, source, true).status != MigrationStatus::Complete {}
        }
        let tx = db.transaction().expect("transaction");
        Family.finish(&tx).expect("finish");
        tx.commit().expect("finish commit");
    }

    #[test]
    fn receipts_tools_old_reply_aliases_and_second_open() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(&db, "old", "completed", "applied");
        db.execute(
            "INSERT INTO runtime_tool_request_aliases VALUES('agent','short','old')",
            [],
        )
        .expect("alias");
        db.execute("INSERT INTO runtime_tool_results VALUES('old','{\"success\":true,\"contentItems\":[]}')",[]).expect("result");
        migrate(&mut db);
        let snapshot: (String, String, i64) = db
            .query_row(
                "SELECT signature,result,created FROM request_results WHERE id='old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("replay");
        assert_eq!(snapshot.0, "signature");
        assert_eq!(snapshot.2, CUTOVER);
        let envelope: Value = serde_json::from_str(&snapshot.1).expect("envelope");
        assert_eq!(envelope["actor"], "agent");
        assert_eq!(envelope["reply"], json!({"success":true,"contentItems":[]}));
        let task: String = db
            .query_row(
                "SELECT record FROM runtime_tool_requests WHERE id='old'",
                [],
                |r| r.get(0),
            )
            .expect("slim task");
        let task: Value = serde_json::from_str(&task).expect("task JSON");
        assert!(task.get("result").is_none());
        assert_eq!(task["aliases"], json!(["short"]));
        assert_eq!(task["kind"], ToolRequestKind::ManagedTool.as_str());
        migrate(&mut db);
        assert_eq!(
            db.query_row(
                "SELECT signature,result,created FROM request_results WHERE id='old'",
                [],
                |r| Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?
                ))
            )
            .expect("same replay"),
            snapshot
        );
        for source in &SOURCES[..2] {
            assert_eq!(
                db.query_row(&format!("SELECT count(*) FROM {source}"), [], |r| r
                    .get::<_, i64>(0))
                    .expect("drained source"),
                0
            );
        }
    }

    #[test]
    fn receipts_tools_crash_between_batches_resumes_and_pending_stays_uncertain() {
        let root = tempfile::tempdir().expect("directory");
        let path = root.path().join("canvas.sqlite3");
        {
            let mut db = Connection::open(&path).expect("database");
            fixture(&mut db);
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert(&db, &format!("request-{n}"), "running", "unknown");
            }
            assert_eq!(
                batch(&mut db, SOURCES[2], true).cursor,
                MIGRATION_BATCH_ROWS as i64
            );
            batch(&mut db, SOURCES[2], false);
        }
        let mut db = Connection::open(&path).expect("reopen after crash");
        migrate(&mut db);
        let raw: String = db
            .query_row(
                "SELECT record FROM runtime_tool_requests WHERE id='request-500'",
                [],
                |r| r.get(0),
            )
            .expect("resumed task");
        let task: Value = serde_json::from_str(&raw).expect("task");
        assert_eq!(task["outcome"], "unknown");
        assert_eq!(
            task["admitted"],
            json!({"id":"request-500","signature":"signature","created":CUTOVER})
        );
    }

    #[test]
    fn receipts_tools_unclassifiable_row_aborts_batch_atomically() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        insert(&db, "good", "completed", "applied");
        insert(&db, "bad", "unexpected", "applied");
        let tx = db.transaction().expect("transaction");
        let checkpoint =
            MigrationCheckpoint::begin(&tx, ID, SOURCES[2], CUTOVER).expect("checkpoint");
        let error = Family
            .move_batch(&tx, SOURCES[2], &checkpoint)
            .expect_err("unclassifiable");
        assert!(
            matches!(error,ReceiptMigrationError::Unclassifiable{table,key,signature} if table==SOURCES[2] && key=="bad" && signature=="signature")
        );
        tx.rollback().expect("failed batch rollback");
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("no partial destination"),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_tool_requests", [], |r| r
                .get::<_, i64>(0))
                .expect("source retained"),
            2
        );
    }

    fn blob_task(db: &Connection, root: &std::path::Path, write: bool) -> std::path::PathBuf {
        let result = json!({"success":true,"contentItems":[{"type":"inputText","text":"exact original body"}]});
        let bytes = result.to_string().into_bytes();
        let hash = crate::sha256_hex(&bytes);
        let directory = root.join("blobs").join(&hash[..2]).join(&hash[2..4]);
        std::fs::create_dir_all(&directory).expect("blob directory");
        let path = directory.join(&hash);
        if write {
            std::fs::write(&path, &bytes).expect("blob");
        }
        let task = json!({"id":"blob-task","agent":"agent","tool":"orchestration_send","signature":"signature","stage":"completed","outcome":"applied","result":{"success":true},"_payloadBlobs":{"result":{"sha256":hash,"size":bytes.len(),"preview":{}}}});
        db.execute(
            "INSERT INTO runtime_tool_requests VALUES('blob-task',?1)",
            [task.to_string()],
        )
        .expect("blob task");
        path
    }

    #[test]
    fn receipts_tools_externalized_result_is_verified_and_hydrated_before_import() {
        let root = tempfile::tempdir().expect("directory");
        let mut db = Connection::open(root.path().join("canvas.sqlite3")).expect("database");
        fixture(&mut db);
        let path = blob_task(&db, root.path(), true);
        migrate(&mut db);
        let raw: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id='blob-task'",
                [],
                |r| r.get(0),
            )
            .expect("full reply");
        let reply: Value = serde_json::from_str(&raw).expect("envelope");
        assert_eq!(
            reply["reply"]["contentItems"][0]["text"],
            "exact original body"
        );
        assert!(path.exists(), "source blob retained through commit");
        migrate(&mut db);
    }

    #[test]
    fn receipts_tools_missing_or_corrupt_blob_stops_with_source_identity() {
        for corrupt in [false, true] {
            let root = tempfile::tempdir().expect("directory");
            let mut db = Connection::open(root.path().join("canvas.sqlite3")).expect("database");
            fixture(&mut db);
            let path = blob_task(&db, root.path(), corrupt);
            if corrupt {
                let mut bytes = std::fs::read(&path).expect("bytes");
                bytes[0] = b'!';
                std::fs::write(&path, bytes).expect("corrupt blob");
            }
            let tx = db.transaction().expect("transaction");
            let checkpoint =
                MigrationCheckpoint::begin(&tx, ID, SOURCES[2], CUTOVER).expect("checkpoint");
            assert!(
                matches!(Family.move_batch(&tx,SOURCES[2],&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,signature}) if table==SOURCES[2] && key=="blob-task" && signature=="signature")
            );
            tx.rollback().expect("rollback");
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_results", [], |r| r
                    .get::<_, i64>(0))
                    .expect("no preview reply"),
                0
            );
        }
    }
}
