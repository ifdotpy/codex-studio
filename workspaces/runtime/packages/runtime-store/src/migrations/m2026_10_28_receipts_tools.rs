//! Move exact tool replies to their replay owner and aliases to the current task.
use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    MigrationStatus, ToolRequestKind, ToolRequestOutcome, ToolRequestStatus,
};

use crate::receipt_migration_contract::{
    BatchBudget, MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError,
    ReceiptProjection, Result, SourceReceipt, admitted_owner_token, delete_source, import_result,
    read_source_batch, unclassifiable, validate_completed_discard,
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
// Closed W2e signature audit (89dabbd591). UI location signatures are unchanged.
const LOCATION_REPORTS: &[&str] = &[
    "deleted-completed-c:managed-project-location",
    "normalized-signature:managed-project-location",
];

impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        ID
    }

    fn prepare(&self, tx: &Transaction<'_>, cutover_ms: i64) -> Result<()> {
        for report in LOCATION_REPORTS {
            let mut count = MigrationCheckpoint::begin(tx, ID, report, cutover_ms)?;
            count.status = MigrationStatus::Complete;
            count.save(tx, ID, report, cutover_ms)?;
        }
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
        let rows = read_source_batch(tx, source, checkpoint, &projection, "1")?;
        let mut next = checkpoint.clone();
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Running
        };
        let mut budget = BatchBudget::default();
        for row in rows {
            let decoded: Value = row.json(source)?;
            let blob_bytes =
                referenced_bytes(&decoded).saturating_add(referenced_bytes(&decoded["result"]));
            let input = row
                .id
                .len()
                .saturating_add(row.signature.len())
                .saturating_add(row.result.len())
                .saturating_add(blob_bytes);
            let secondary = if source == "runtime_tool_requests" {
                location_input_metadata(tx, &row, &decoded)?
            } else {
                (0, 0)
            };
            if (budget.rows != 0
                && budget.rows.saturating_add(1 + secondary.0)
                    > crate::receipt_migration_contract::MIGRATION_BATCH_ROWS)
                || !budget.include(input.saturating_add(secondary.1))
            {
                break;
            }
            budget.rows += secondary.0;
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
                "runtime_tool_results" if is_retired_remote_spawn(tx, &row.id)? => {
                    validate_completed_discard(source, &row.id, checkpoint.cutover_ms)?;
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
                        hydrate_location_arguments(
                            tx,
                            source,
                            &row,
                            &mut task,
                            checkpoint.cutover_ms,
                        )?;
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
                        object.insert("kind".into(), json!(ToolRequestKind::ManagedTool.as_str()));
                        object.entry("originKey").or_insert(Value::Null);
                        object.entry("aliases").or_insert(json!([]));
                        object.insert(
                            "admitted".into(),
                            crate::projection_value(&admitted_owner_token(
                                &row.id,
                                &row.signature,
                                checkpoint.cutover_ms,
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
                _ => return Err(unclassifiable(source, &row.id, &row.signature)),
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

fn referenced_bytes(record: &Value) -> usize {
    record
        .get("_payloadBlobs")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|refs| refs.values())
        .filter_map(|reference| reference["size"].as_u64())
        .fold(0_usize, |total, size| {
            total.saturating_add(usize::try_from(size).unwrap_or(usize::MAX))
        })
}

fn hydrate_location_arguments(
    tx: &Transaction<'_>,
    source: &str,
    row: &SourceReceipt,
    task: &mut Value,
    cutover: i64,
) -> Result<()> {
    if task["tool"] != "orchestration_servers" {
        return Ok(());
    }
    let invalid = || unclassifiable(source, &row.id, &row.signature);
    let actor = task["agent"].as_str().ok_or_else(invalid)?;
    let id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("studio-cross-server:project-location:{actor}:{}", row.id).as_bytes(),
    )
    .to_string();
    let raw: Option<String> =
        if crate::receipt_migration_contract::source_exists(tx, "runtime_server_outbox")? {
            tx.query_row(
                "SELECT body FROM runtime_server_outbox WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()?
        } else {
            None
        };
    let body: Option<Value> = raw
        .map(|raw| serde_json::from_str(&raw).map_err(|_| invalid()))
        .transpose()?;
    let args = body
        .as_ref()
        .and_then(|body| body.pointer("/payload/request"));
    let pending = matches!(task["outcome"].as_str(), Some("pending" | "unknown"));
    migrate_location_c(tx, &id, args, pending, cutover)?;
    let Some(args) = args else {
        return Ok(());
    };
    let body = body.as_ref().ok_or_else(invalid)?;
    if body["requestId"] != id
        || body["action"] != args["action"]
        || !matches!(
            args["action"].as_str(),
            Some("add_location" | "remove_location")
        )
        || args["project"] != body["payload"]["project"]
        || !args["project"].is_string()
        || !args["server"].is_string()
        || !args["path"].is_null() && !args["path"].is_string()
    {
        return Err(invalid());
    }
    let original_signature =
        crate::sha256_hex(studio_runtime_schema::json_compat::python_json(args).as_bytes());
    if body["payload"]["signature"] != original_signature {
        return Err(unclassifiable(
            "runtime_server_outbox",
            &id,
            body["payload"]["signature"].as_str().unwrap_or(""),
        ));
    }
    if task
        .get("arguments")
        .is_some_and(|original| original != args)
    {
        return Err(invalid());
    }
    task["arguments"] = args.clone();
    if !pending {
        return Ok(());
    }
    let project = args["project"].as_str().ok_or_else(invalid)?;
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_projects WHERE id=?1",
            [project],
            |r| r.get(0),
        )
        .optional()?;
    let raw = raw.ok_or_else(invalid)?;
    let mut owner: Value = serde_json::from_str(&raw)
        .ok()
        .filter(Value::is_object)
        .ok_or_else(invalid)?;
    if owner.get("locationIntents").is_none() {
        owner["locationIntents"] = json!({});
    }
    let intents = owner["locationIntents"]
        .as_object_mut()
        .ok_or_else(invalid)?;
    let signature =
        crate::sha256_hex(studio_runtime_schema::json_compat::python_json_compact(args).as_bytes());
    let token = admitted_owner_token(&row.id, &row.signature, cutover).derive_result(
        "project-location",
        &["location"],
        &signature,
    );
    if intents.contains_key(&id) {
        let path = format!("$.locationIntents.\"{id}\".request");
        let saved: Option<String> =
            tx.query_row("SELECT json_extract(?1,?2)", params![raw, path], |r| {
                r.get(0)
            })?;
        let saved: crate::request_results::AdmittedRequest =
            serde_json::from_str(&saved.ok_or_else(invalid)?).map_err(|_| invalid())?;
        if (saved.signature() != original_signature && saved.signature() != signature)
            || (saved.id() != id && saved.id() != token.id())
        {
            return Err(unclassifiable(
                "runtime_projects",
                project,
                saved.signature(),
            ));
        }
    }
    #[derive(serde::Serialize)]
    struct LocationIntent {
        request: crate::request_results::AdmittedRequest,
    }
    intents.insert(
        id,
        crate::projection_value(&LocationIntent { request: token }),
    );
    tx.execute(
        "UPDATE runtime_projects SET record=?1 WHERE id=?2",
        params![owner.to_string(), project],
    )?;
    Ok(())
}

/// Include secondary source rows using lengths before materializing their payloads.
fn location_input_metadata(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    task: &Value,
) -> Result<(usize, usize)> {
    if task["tool"] != "orchestration_servers" {
        return Ok((0, 0));
    }
    let actor = task["agent"]
        .as_str()
        .ok_or_else(|| unclassifiable("runtime_tool_requests", &row.id, &row.signature))?;
    let id = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("studio-cross-server:project-location:{actor}:{}", row.id).as_bytes(),
    )
    .to_string();
    let mut rows = 0;
    let mut bytes = 0_usize;
    for (table, expression) in [
        (
            "runtime_operation_receipts",
            "length(CAST(id AS BLOB))+length(CAST(signature AS BLOB))+length(CAST(result AS BLOB))",
        ),
        (
            "runtime_server_outbox",
            "length(CAST(id AS BLOB))+length(CAST(body AS BLOB))",
        ),
    ] {
        if crate::receipt_migration_contract::source_exists(tx, table)? {
            let length: Option<i64> = tx
                .query_row(
                    &format!("SELECT {expression} FROM {table} WHERE id=?1"),
                    [&id],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(length) = length {
                rows += 1;
                bytes = bytes.saturating_add(usize::try_from(length).unwrap_or(usize::MAX));
            }
        }
    }
    Ok((rows, bytes))
}

/// Exact old task/actor UUIDv5 lineage proves the managed location family.
/// It alone changed from spaced to compact JSON; UI and unidentified replies do not.
fn migrate_location_c(
    tx: &Transaction<'_>,
    id: &str,
    args: Option<&Value>,
    pending: bool,
    cutover: i64,
) -> Result<()> {
    const SOURCE: &str = "runtime_operation_receipts";
    if !crate::receipt_migration_contract::source_exists(tx, SOURCE)? {
        return Ok(());
    }
    let row = crate::receipt_migration_contract::load_operation_receipt(tx, id)?;
    let Some(row) = row else {
        return Ok(());
    };
    let reply: Value = row.json(SOURCE)?;
    if reply["requestId"] != id {
        return Err(unclassifiable(SOURCE, id, &row.signature));
    }
    if let Some(args) = args {
        let original =
            crate::sha256_hex(studio_runtime_schema::json_compat::python_json(args).as_bytes());
        if row.signature != original {
            return Err(unclassifiable(SOURCE, id, &row.signature));
        }
    }
    if reply["outcome"] == "unknown" {
        // The same transaction persists its exact derived token on the pending
        // project owner below. A bodyless unknown remains held with its old marker.
        if args.is_some() && pending {
            delete_source(tx, SOURCE, row.rowid)?;
        }
        return Ok(());
    }
    if !matches!(reply["outcome"].as_str(), Some("applied" | "not_applied")) {
        return Err(unclassifiable(SOURCE, id, &row.signature));
    }
    let report = if let Some(args) = args {
        if !args.is_object()
            || !matches!(
                args["action"].as_str(),
                Some("add_location" | "remove_location")
            )
            || !args["project"].is_string()
            || !args["server"].is_string()
            || !args["path"].is_null() && !args["path"].is_string()
        {
            return Err(unclassifiable(SOURCE, id, &row.signature));
        }
        let signature = crate::sha256_hex(
            studio_runtime_schema::json_compat::python_json_compact(args).as_bytes(),
        );
        import_result(
            tx,
            id,
            &signature,
            &ResultEnvelope {
                actor: None,
                reply,
                references: vec![],
            },
            cutover,
        )?;
        LOCATION_REPORTS[1]
    } else {
        validate_completed_discard(SOURCE, id, cutover)?;
        LOCATION_REPORTS[0]
    };
    let mut count = MigrationCheckpoint::begin(tx, ID, report, cutover)?;
    count.cursor += 1;
    count.status = MigrationStatus::Complete;
    count.save(tx, ID, report, cutover)?;
    delete_source(tx, SOURCE, row.rowid)
}

/// The retired Python backend saved remote spawn replies as `remote-worker:<link>`
/// without a request row. Its only reader was deleted in 027f3cfea, so no code reads them.
fn is_retired_remote_spawn(tx: &Transaction<'_>, id: &str) -> Result<bool> {
    if !id.starts_with("remote-worker:") {
        return Ok(false);
    }
    let request: Option<i64> = tx
        .query_row(
            "SELECT 1 FROM runtime_tool_requests WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(request.is_none())
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
    let status = task["stage"].as_str().ok_or_else(invalid)?;
    let outcome = task["outcome"].as_str().ok_or_else(invalid)?;
    let valid = match outcome {
        s if s == ToolRequestOutcome::Pending.as_str() => {
            [ToolRequestStatus::Queued, ToolRequestStatus::Running]
                .iter()
                .any(|v| v.as_str() == status)
        }
        s if s == ToolRequestOutcome::Unknown.as_str() => [
            ToolRequestStatus::Running,
            ToolRequestStatus::Failed,
            ToolRequestStatus::Unknown,
            ToolRequestStatus::Interrupted,
        ]
        .iter()
        .any(|v| v.as_str() == status),
        s if s == ToolRequestOutcome::Applied.as_str() => {
            status == ToolRequestStatus::Completed.as_str()
        }
        s if s == ToolRequestOutcome::NotApplied.as_str() => [
            ToolRequestStatus::Completed,
            ToolRequestStatus::Failed,
            ToolRequestStatus::Cancelled,
            ToolRequestStatus::Interrupted,
        ]
        .iter()
        .any(|v| v.as_str() == status),
        _ => false,
    };
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
            db.execute_batch("BEGIN").expect("fixture transaction");
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert(&db, &format!("request-{n}"), "running", "unknown");
            }
            db.execute_batch("COMMIT").expect("fixture setup committed");
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

    #[test]
    fn receipts_tools_orphan_alias_and_result_refuse_without_writes() {
        for source in &SOURCES[..2] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            db.execute(
                "INSERT INTO runtime_tool_request_aliases VALUES('agent','call','missing')",
                [],
            )
            .expect("orphan alias");
            db.execute(
                "INSERT INTO runtime_tool_results VALUES('missing','{}')",
                [],
            )
            .expect("orphan result");
            let tx = db.transaction().expect("transaction");
            let checkpoint =
                MigrationCheckpoint::begin(&tx, ID, source, CUTOVER).expect("checkpoint");
            let error = Family
                .move_batch(&tx, source, &checkpoint)
                .expect_err("orphan must refuse");
            assert!(
                matches!(error, ReceiptMigrationError::Unclassifiable { table, .. } if table == *source)
            );
            tx.rollback().expect("rollback");
            for table in &SOURCES[..2] {
                assert_eq!(
                    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                        .get::<_, i64>(0))
                        .expect("source rows"),
                    1
                );
            }
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_results", [], |r| r
                    .get::<_, i64>(0))
                    .expect("destination rows"),
                0
            );
        }
    }

    #[test]
    fn receipts_tools_discards_retired_remote_spawn_replies() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute(
            "INSERT INTO runtime_tool_results VALUES('remote-worker:9d030f9d-01d5-586f-a785-11a33fb55bae','{\"success\":true}')",
            [],
        )
        .expect("retired reply");
        let tx = db.transaction().expect("transaction");
        let source = SOURCES[1];
        let mut checkpoint =
            MigrationCheckpoint::begin(&tx, ID, source, CUTOVER).expect("checkpoint");
        while checkpoint.status != MigrationStatus::Complete {
            checkpoint = Family
                .move_batch(&tx, source, &checkpoint)
                .expect("retired reply is discarded");
        }
        assert_eq!(
            tx.query_row(
                "SELECT count(*) FROM runtime_tool_results WHERE id LIKE 'remote-worker:%'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("source rows"),
            0
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

    #[test]
    fn receipts_tools_wrong_original_location_signature_aborts_without_new_authority() {
        for outcome in ["applied", "unknown"] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,body TEXT);CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT);INSERT INTO runtime_projects VALUES('project','{}')").expect("old owners");
            let task = json!({"id":"task","agent":"agent","tool":"orchestration_servers","signature":"tool signature","stage":if outcome=="unknown"{"running"}else{"completed"},"outcome":outcome});
            db.execute(
                "INSERT INTO runtime_tool_requests VALUES('task',?1)",
                [task.to_string()],
            )
            .expect("task");
            let id = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                b"studio-cross-server:project-location:agent:task",
            )
            .to_string();
            let input = json!({"action":"add_location","project":"project","server":"peer","path":"/original"});
            let signature = crate::sha256_hex(crate::python_json(&input).as_bytes());
            db.execute("INSERT INTO runtime_server_outbox VALUES(?1,?2)",params![id,json!({"requestId":id,"action":"add_location","payload":{"project":"project","request":input,"signature":signature}}).to_string()]).expect("captured request");
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'corrupt logical signature',?2)",
                params![id, json!({"requestId":id,"outcome":outcome}).to_string()],
            )
            .expect("mismatched old C");
            let tx = db.transaction().expect("transaction");
            let checkpoint = MigrationCheckpoint::begin(&tx, ID, "runtime_tool_requests", CUTOVER)
                .expect("checkpoint");
            assert!(
                matches!(Family.move_batch(&tx,"runtime_tool_requests",&checkpoint),Err(ReceiptMigrationError::Unclassifiable{table,key,signature})if table=="runtime_operation_receipts"&&key==id&&signature=="corrupt logical signature")
            );
            tx.rollback().expect("source preserved");
            assert_eq!(
                db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                    .get::<_, i64>(0))
                    .expect("old C"),
                1
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM request_results", [], |r| r
                    .get::<_, i64>(0))
                    .expect("no new replay authority"),
                0
            );
        }
    }

    #[test]
    fn receipts_tools_secondary_location_sources_share_row_and_byte_batch_limits() {
        for (count, padding, first_batch) in [(167, 0, 166), (2, 9 * 1024 * 1024, 1)] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,body TEXT)").expect("old secondary sources");
            let input = json!({"action":"add_location","project":"project","server":"peer","path":"/original"});
            for index in 0..count {
                let task_id = format!("task-{index}");
                let task = json!({"id":task_id,"agent":"agent","tool":"orchestration_servers","signature":"tool signature","stage":"completed","outcome":"applied"});
                db.execute(
                    "INSERT INTO runtime_tool_requests VALUES(?1,?2)",
                    params![task_id, task.to_string()],
                )
                .expect("task");
                let id = uuid::Uuid::new_v5(
                    &uuid::Uuid::NAMESPACE_URL,
                    format!("studio-cross-server:project-location:agent:{task_id}").as_bytes(),
                )
                .to_string();
                db.execute("INSERT INTO runtime_server_outbox VALUES(?1,?2)",params![id,json!({"requestId":id,"action":"add_location","payload":{"project":"project","request":input,"signature":crate::sha256_hex(crate::python_json(&input).as_bytes())}}).to_string()]).expect("captured request");
                db.execute("INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",params![id,crate::sha256_hex(crate::python_json(&input).as_bytes()),json!({"requestId":id,"outcome":"applied","value":{"original":"x".repeat(padding)}}).to_string()]).expect("old C reply");
            }
            let next = batch(&mut db, "runtime_tool_requests", true);
            assert_eq!(
                next.cursor, first_batch,
                "task plus secondary rows form one bounded batch"
            );
            assert_eq!(
                db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                    .get::<_, i64>(0))
                    .expect("remaining exact C rows"),
                count - first_batch
            );
            migrate(&mut db);
            assert_eq!(
                db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                    .get::<_, i64>(0))
                    .expect("all classified"),
                0
            );
        }
    }

    #[test]
    fn receipts_tools_managed_location_c_uses_exact_task_lineage_and_counts_lossy_rows() {
        for captured in [true, false] {
            let mut db = Connection::open_in_memory().expect("database");
            fixture(&mut db);
            db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,body TEXT);CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT);INSERT INTO runtime_projects VALUES('project','{}')").expect("old owners");
            let task = json!({"id":"task","agent":"agent","tool":"orchestration_servers","signature":"tool signature","stage":"completed","outcome":"applied"});
            db.execute(
                "INSERT INTO runtime_tool_requests VALUES('task',?1)",
                [task.to_string()],
            )
            .expect("task");
            let id = uuid::Uuid::new_v5(
                &uuid::Uuid::NAMESPACE_URL,
                b"studio-cross-server:project-location:agent:task",
            )
            .to_string();
            let input = json!({"action":"add_location","project":"project","server":"peer","path":"/original"});
            let old = crate::sha256_hex(crate::python_json(&input).as_bytes());
            let canonical = crate::sha256_hex(crate::python_json_compact(&input).as_bytes());
            let reply = json!({"requestId":id,"outcome":"applied","projectId":"project","value":{"serverId":"peer","gitOrigin":null}});
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
                params![id, old, reply.to_string()],
            )
            .expect("old logical result");
            if captured {
                db.execute("INSERT INTO runtime_server_outbox VALUES(?1,?2)",params![id,json!({"requestId":id,"action":"add_location","payload":{"project":"project","request":input,"signature":old}}).to_string()]).expect("immutable request");
            }
            migrate(&mut db);
            let saved =
                crate::request_results::RequestResults::replay(&db, &id, &canonical, CUTOVER + 1)
                    .expect("never conflicting marker");
            assert_eq!(saved.is_some(), captured);
            if let Some(saved) = saved {
                assert_eq!(
                    serde_json::from_str::<Value>(&saved.result).expect("envelope")["reply"],
                    reply
                );
            }
            assert_eq!(
                db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                    .get::<_, i64>(0))
                    .expect("classified old C"),
                0
            );
            let tx = db.transaction().expect("transaction");
            for (report, expected) in [
                (
                    "normalized-signature:managed-project-location",
                    i64::from(captured),
                ),
                (
                    "deleted-completed-c:managed-project-location",
                    i64::from(!captured),
                ),
            ] {
                assert_eq!(
                    MigrationCheckpoint::load(&tx, ID, report)
                        .expect("report")
                        .expect("closed family count")
                        .cursor,
                    expected
                );
            }
            tx.rollback().expect("no effects");
            migrate(&mut db);
        }
    }

    #[test]
    fn receipts_tools_pending_location_arguments_come_from_exact_outbox_identity() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&mut db);
        db.execute_batch("CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,body TEXT);CREATE TABLE runtime_projects(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);INSERT INTO runtime_projects VALUES('project','{}')")
            .expect("outboxes");
        let task = json!({"id":"old-task","agent":"agent","tool":"orchestration_servers","signature":"signature","stage":"running","outcome":"unknown"});
        db.execute(
            "INSERT INTO runtime_tool_requests VALUES('old-task',?1)",
            [task.to_string()],
        )
        .expect("pending task");
        let id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            b"studio-cross-server:project-location:agent:old-task",
        )
        .to_string();
        let args = json!({"action":"add_location","project":"project","server":"server","path":"/project"});
        let body = json!({"requestId":id,"action":"add_location","payload":{"project":"project","request":args,"signature":crate::sha256_hex(crate::python_json(&args).as_bytes())}});
        db.execute(
            "INSERT INTO runtime_server_outbox VALUES(?1,?2)",
            params![id, body.to_string()],
        )
        .expect("exact immutable outbox");
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
            params![
                id,
                crate::sha256_hex(crate::python_json(&args).as_bytes()),
                json!({"requestId":id,"projectId":"project","outcome":"unknown"}).to_string()
            ],
        )
        .expect("old unknown marker");
        migrate(&mut db);
        let raw: String = db
            .query_row("SELECT record FROM runtime_tool_requests", [], |r| r.get(0))
            .expect("task");
        let task: Value = serde_json::from_str(&raw).expect("task JSON");
        let task_raw = raw;
        assert_eq!(task["arguments"], args);
        assert_eq!(task["outcome"], "unknown");
        assert_eq!(task["admitted"]["id"], "old-task");
        let raw: String = db
            .query_row(
                "SELECT record FROM runtime_projects WHERE id='project'",
                [],
                |r| r.get(0),
            )
            .expect("project");
        let project: Value = serde_json::from_str(&raw).expect("project JSON");
        #[derive(serde::Deserialize)]
        struct TaskAdmission {
            admitted: crate::request_results::AdmittedRequest,
        }
        let parent = serde_json::from_str::<TaskAdmission>(&task_raw)
            .expect("parent admission")
            .admitted;
        let signature = crate::sha256_hex(
            studio_runtime_schema::json_compat::python_json_compact(&args).as_bytes(),
        );
        assert_eq!(
            project["locationIntents"][&id]["request"],
            crate::projection_value(&parent.derive_result(
                "project-location",
                &["location"],
                &signature
            ))
        );
        let preserved: String = db
            .query_row(
                "SELECT body FROM runtime_server_outbox WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .expect("wire");
        assert_eq!(preserved, body.to_string());
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("pending authority moved atomically to project token"),
            0
        );
    }
}
