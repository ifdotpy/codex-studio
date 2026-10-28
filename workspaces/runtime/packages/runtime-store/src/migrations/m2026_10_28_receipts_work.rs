//! Hydrate current refusal counters before moving work effect owners.
use crate::outbox::{Outbox, OutboxRow};
use crate::receipt_migration_contract::{
    self as contract, MigrationCheckpoint, ReceiptFamilyMigration, Result,
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, WorkEffectStatus,
};
use studio_runtime_schema::queue_command::Field;
use studio_runtime_schema::task_results::MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT;
pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &["work-current-counters", "runtime_operation_receipts"];
const TABLE: &str = "runtime_operation_receipts";
const PREDICATE: &str = "source.id GLOB 'work-result-failures:*' OR source.id GLOB 'work-archive:*' OR source.id GLOB 'work-write-vm-intent:*' OR CASE WHEN json_valid(source.result) THEN json_extract(source.result,'$.vmAcceptReservation')=1 ELSE 0 END";
impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-work"
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
        if source == SOURCES[0] {
            let rows = contract::read_source_batch(
                tx,
                "runtime_work",
                checkpoint,
                &contract::ReceiptProjection {
                    id: "source.id",
                    signature: "'work-result-failures-v1'",
                    result: "source.record",
                },
                "1",
            )?;
            next.status = if rows.is_empty() {
                MigrationStatus::Complete
            } else {
                MigrationStatus::Pending
            };
            let mut budget = contract::BatchBudget::default();
            for row in rows {
                if !move_counter(tx, &row, &mut budget)? {
                    break;
                }
                next.cursor = row.rowid;
            }
        } else if source == TABLE {
            let rows = contract::read_receipt_batch(tx, TABLE, checkpoint, PREDICATE)?;
            next.status = if rows.is_empty() {
                MigrationStatus::Complete
            } else {
                MigrationStatus::Pending
            };
            for row in rows {
                move_effect(tx, &row, checkpoint.cutover_ms)?;
                contract::delete_source(tx, TABLE, row.rowid)?;
                next.cursor = row.rowid;
            }
        } else {
            return Err(contract::unclassifiable(TABLE, source, self.id()));
        }
        next.save(tx, self.id(), source, checkpoint.cutover_ms)?;
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        if !contract::source_exists(tx, TABLE)? {
            return Ok(());
        }
        let remains: bool = tx.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {TABLE} AS source WHERE {PREDICATE})"),
            [],
            |r| r.get(0),
        )?;
        if remains {
            return Err(contract::ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: TABLE.into(),
            });
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    failed: u64,
    errors: Option<Vec<Value>>,
}

fn counter(row: &contract::SourceReceipt) -> Result<Counter> {
    let invalid = || contract::unclassifiable(TABLE, &row.id, &row.signature);
    let suffix = row
        .id
        .strip_prefix("work-result-failures:")
        .ok_or_else(invalid)?;
    if row.signature != "work-result-failures-v1"
        || suffix.len() != 64
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid());
    }
    let value: Counter = row.json(TABLE)?;
    if value.failed > MAX_FAILED_RESULT_SUBMITS_PER_ATTEMPT {
        return Err(invalid());
    }
    Ok(value)
}

// Source-era refusal hashes depended on the historical run selector.
// Keep that frozen mapping here; runtime reads the persisted active execution pointer.
fn source_submission_identity(
    connection: &rusqlite::Connection,
    agent: &crate::AgentRecord,
) -> crate::StoreResult<Option<(String, Option<String>)>> {
    let table_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_execution_runs')",
            [],
            |row| row.get(0),
        )?;
    if !table_exists {
        return Ok(None);
    }

    let account = agent
        .extra
        .get("accountKey")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("default");
    let thread = agent
        .extra
        .get("threadId")
        .and_then(serde_json::Value::as_str);
    let turn = agent
        .extra
        .get("turnId")
        .and_then(serde_json::Value::as_str);
    let row: Option<(String, String)> = if let Some(turn) = turn {
        let mut statement = connection.prepare_cached(
            "SELECT id, record FROM runtime_execution_runs \
                 WHERE agent=?1 AND account=?2 AND thread IS ?3 AND turn=?4 LIMIT 2",
        )?;
        let rows = statement
            .query_map(params![agent.id.0, account, thread, turn], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        (rows.len() == 1).then(|| rows.into_iter().next()).flatten()
    } else {
        connection
            .query_row(
                "SELECT id, record FROM runtime_execution_runs \
                     WHERE agent=?1 AND account=?2 AND epoch=?3 AND turn IS NULL \
                     ORDER BY created DESC LIMIT 1",
                params![agent.id.0, account, agent.epoch],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
    };
    let Some((id, record)) = row else {
        return Ok(None);
    };
    let typed = serde_json::from_str::<crate::ExecutionRunRecord>(&record).map_err(|_| {
        crate::StoreError::InvalidStoredObject {
            table: "runtime_execution_runs",
            id: id.clone(),
        }
    })?;
    if typed.id().as_ref() != id
        || crate::Table::<crate::ExecutionRunRecord>::new(connection)
            .validate(&typed)
            .is_err()
    {
        return Err(crate::StoreError::InvalidStoredObject {
            table: "runtime_execution_runs",
            id,
        });
    }
    let value = typed.value();
    if turn.is_none()
        && matches!(
            value.get("status").and_then(serde_json::Value::as_str),
            Some("completed" | "failed" | "interrupted" | "cancelled")
        )
    {
        return Ok(None);
    }
    let attempt_id = value
        .get("latestAttemptId")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    Ok(Some((id, attempt_id)))
}

fn move_counter(
    tx: &Transaction<'_>,
    row: &contract::SourceReceipt,
    budget: &mut contract::BatchBudget,
) -> Result<bool> {
    let invalid = || contract::unclassifiable("runtime_work", &row.id, &row.signature);
    let mut work: Value = row.json("runtime_work")?;
    if !work.is_object() || work["id"] != row.id {
        return Err(invalid());
    }
    let Some(owner) = work["owner"].as_str() else {
        return Ok(budget.include(row.result.len()));
    };
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [owner],
            |r| r.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(budget.include(row.result.len()));
    };
    let agent: crate::AgentRecord = serde_json::from_str(&raw).map_err(|_| invalid())?;
    if agent.id.0 != owner {
        return Err(invalid());
    }
    let version = work["version"].as_i64().ok_or_else(invalid)?;
    let attempt = source_submission_identity(tx, &agent).map_err(|_| invalid())?;
    let id = format!(
        "work-result-failures:{}",
        crate::sha256_hex(
            json!([row.id, version, owner, agent.epoch, attempt])
                .to_string()
                .as_bytes()
        )
    );
    let metadata:Option<(i64,String,i64)>=tx.query_row("SELECT rowid,signature,length(CAST(result AS BLOB)) FROM runtime_operation_receipts WHERE id=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((rowid, signature, bytes)) = metadata else {
        return Ok(budget.include(row.result.len() + raw.len()));
    };
    if !budget.include(
        row.result
            .len()
            .saturating_add(raw.len())
            .saturating_add(usize::try_from(bytes).map_err(|_| invalid())?),
    ) {
        return Ok(false);
    }
    let result = tx.query_row(
        "SELECT result FROM runtime_operation_receipts WHERE rowid=?1",
        [rowid],
        |r| r.get(0),
    )?;
    let receipt = contract::SourceReceipt {
        rowid,
        id,
        signature,
        result,
    };
    let count = counter(&receipt)?;
    let errors = if let Some(errors) = count.errors {
        errors
    } else if count.failed == 0 {
        vec![]
    } else {
        let invalid = || contract::unclassifiable(TABLE, &receipt.id, &receipt.signature);
        let refusal:Option<(i64,Option<i64>)>=tx.query_row("SELECT rowid,length(CAST(json_extract(result,'$.submissionRefusal.errors') AS BLOB)) FROM runtime_operation_receipts WHERE rowid>?1 AND json_valid(result) AND json_extract(result,'$.id')=?2 AND json_extract(result,'$.version')=?3 AND json_extract(result,'$.owner')=?4 AND json_extract(result,'$.submissionRefusal.failedSubmits')=?5 ORDER BY rowid LIMIT 1",params![rowid,row.id,version,owner,count.failed as i64],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (refusal_rowid, bytes) = refusal.ok_or_else(invalid)?;
        let bytes = usize::try_from(bytes.ok_or_else(invalid)?).map_err(|_| invalid())?;
        if budget.rows > 1 && budget.bytes.saturating_add(bytes) > contract::MIGRATION_BATCH_BYTES {
            return Ok(false);
        }
        budget.bytes = budget.bytes.saturating_add(bytes);
        let refusal:String=tx.query_row("SELECT json_extract(result,'$.submissionRefusal.errors') FROM runtime_operation_receipts WHERE rowid=?1",[refusal_rowid],|r|r.get(0))?;
        serde_json::from_str::<Vec<Value>>(&refusal)
            .map_err(|_| contract::unclassifiable(TABLE, &receipt.id, &receipt.signature))?
    };
    work["resultFailures"] = json!({"version":version,"owner":owner,"epoch":agent.epoch,"providerAttempt":attempt,"count":count.failed,"errors":errors});
    tx.execute(
        "UPDATE runtime_work SET record=?2 WHERE id=?1",
        params![row.id, work.to_string()],
    )?;
    contract::delete_source(tx, TABLE, rowid)?;
    Ok(true)
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveIntent {
    status: WorkEffectStatus,
    task_id: String,
    intent_id: String,
    actor: String,
    actor_epoch: i64,
    target: String,
    attempts: u64,
    created: f64,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}
#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum LegacyVmIntent {
    Pending(LegacyVmOwner),
    Unknown(LegacyVmOwner),
    NotSent(LegacyVmOwner),
    Completed(LegacyVmOwner),
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyVmOwner {
    task_id: String,
    request_id: String,
    owner: String,
    revision: String,
    created: f64,
    #[serde(default)]
    version: Field<i64>,
    #[serde(default)]
    result_id: Field<String>,
    #[serde(default)]
    evidence: Field<crate::vm_work_intent::Evidence>,
}
impl LegacyVmIntent {
    fn parts(self) -> (LegacyVmOwner, WorkEffectStatus) {
        match self {
            Self::Pending(owner) => (owner, WorkEffectStatus::Pending),
            Self::Unknown(owner) => (owner, WorkEffectStatus::Unknown),
            Self::NotSent(owner) => (owner, WorkEffectStatus::NotSent),
            Self::Completed(owner) => (owner, WorkEffectStatus::Completed),
        }
    }
}
fn vm_target<I, R>(
    owner: crate::vm_work_intent::Owner<I>,
    status: WorkEffectStatus,
    result: Option<R>,
) -> Option<crate::vm_work_intent::Intent<I, R>> {
    use crate::vm_work_intent::Intent;
    match (status, result) {
        (WorkEffectStatus::Pending, None) => Some(Intent::Pending(owner)),
        (WorkEffectStatus::Unknown, None) => Some(Intent::Unknown(owner)),
        (WorkEffectStatus::NotSent, None) => Some(Intent::NotSent(owner)),
        (WorkEffectStatus::Completed, Some(result)) => Some(Intent::Completed { owner, result }),
        (WorkEffectStatus::Completed, None)
        | (
            WorkEffectStatus::Pending | WorkEffectStatus::Unknown | WorkEffectStatus::NotSent,
            Some(_),
        )
        | (
            WorkEffectStatus::Terminal
            | WorkEffectStatus::Retryable
            | WorkEffectStatus::Submitted
            | WorkEffectStatus::Complete
            | WorkEffectStatus::Applied
            | WorkEffectStatus::Failed
            | WorkEffectStatus::Cancelled,
            _,
        ) => None,
    }
}
fn vm_owner<I>(
    legacy: &LegacyVmOwner,
    id: I,
    public: Option<crate::vm_work_intent::PublicRequest>,
) -> crate::vm_work_intent::Owner<I> {
    crate::vm_work_intent::Owner {
        request_id: legacy.request_id.clone(),
        owner: legacy.owner.clone(),
        revision: legacy.revision.clone(),
        id,
        public,
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Reservation {
    vm_accept_reservation: bool,
    task_id: String,
    agent_id: String,
    status: ReservationStatus,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReservationStatus {
    Reserved,
}
#[derive(Serialize)]
struct WorkReservation {
    root: String,
    agent: String,
    task: String,
    request: String,
    admitted: crate::request_results::AdmittedRequest,
    archive: bool,
}

fn read_work(tx: &Transaction<'_>, id: &str, row: &contract::SourceReceipt) -> Result<Value> {
    let invalid = || contract::unclassifiable(TABLE, &row.id, &row.signature);
    let raw: Option<String> = tx
        .query_row("SELECT record FROM runtime_work WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    let value: Value = serde_json::from_str(&raw.ok_or_else(invalid)?).map_err(|_| invalid())?;
    if !value.is_object() || value["id"] != id {
        return Err(invalid());
    }
    Ok(value)
}
fn preserve<P: Serialize>(
    tx: &Transaction<'_>,
    row: &contract::SourceReceipt,
    task: &str,
    kind: OutboxKind,
    payload: P,
    cutover_ms: i64,
) -> Result<()> {
    contract::import_outbox(
        tx,
        &OutboxRow {
            id: Outbox::effect_id(
                &row.id,
                kind,
                OwnerKind::Task,
                task,
                if kind == OutboxKind::WorkVm {
                    &["public-request"]
                } else {
                    &[]
                },
            ),
            kind,
            owner_kind: OwnerKind::Task,
            owner_id: task.into(),
            request_id: Some(row.id.clone()),
            signature: row.signature.clone(),
            status: OutboxStatus::Unknown,
            account: None,
            stream_id: None,
            generation: None,
            ordinal: None,
            next_at: None,
            payload,
            created: cutover_ms,
        },
    )
}
fn move_effect(tx: &Transaction<'_>, row: &contract::SourceReceipt, cutover_ms: i64) -> Result<()> {
    let invalid = || contract::unclassifiable(TABLE, &row.id, &row.signature);
    if row.id.starts_with("work-result-failures:") {
        counter(row)?; // Current tuples were all moved by the preceding keyset pass.
        return Ok(());
    }
    let (task, work) = if row.id.starts_with("work-archive:") {
        if row.signature != "accepted-archive-v1" {
            return Err(invalid());
        }
        let intent: ArchiveIntent = row.json(TABLE)?;
        if row.id != format!("work-archive:{}", intent.intent_id)
            || !intent
                .intent_id
                .starts_with(&format!("{}:", intent.task_id))
            || !intent.created.is_finite()
        {
            return Err(invalid());
        }
        let unresolved = match intent.status {
            WorkEffectStatus::Pending
            | WorkEffectStatus::Retryable
            | WorkEffectStatus::Submitted
            | WorkEffectStatus::Unknown
            | WorkEffectStatus::NotSent => true,
            WorkEffectStatus::Complete
            | WorkEffectStatus::Completed
            | WorkEffectStatus::Terminal => false,
            WorkEffectStatus::Applied | WorkEffectStatus::Failed | WorkEffectStatus::Cancelled => {
                return Err(invalid());
            }
        };
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_work WHERE id=?1",
                [&intent.task_id],
                |r| r.get(0),
            )
            .optional()?;
        let mut work = raw
            .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| invalid()))
            .transpose()?;
        let current = work.as_ref().is_some_and(|work| {
            work["archiveIntent"]["id"] == intent.intent_id
                || work["archiveIntent"]["intentId"] == intent.intent_id
                || (work["archiveIntent"].is_null()
                    && work["status"] == "accepted"
                    && work["owner"] == intent.target
                    && work["results"]
                        .as_array()
                        .and_then(|items| items.last())
                        .and_then(|result| result["id"].as_str())
                        .is_some_and(|result| {
                            intent.intent_id == format!("{}:{result}", intent.task_id)
                        }))
        });
        if !current {
            if unresolved {
                preserve(
                    tx,
                    row,
                    &intent.task_id,
                    OutboxKind::WorkArchive,
                    &intent,
                    cutover_ms,
                )?;
            }
            return Ok(());
        }
        let mut work = work.take().ok_or_else(invalid)?;
        let mut destination = if work["archiveIntent"].is_null() {
            serde_json::Map::from_iter([
                ("id".into(), json!(intent.intent_id)),
                ("owner".into(), json!(intent.target)),
            ])
        } else {
            work["archiveIntent"]
                .as_object()
                .cloned()
                .ok_or_else(invalid)?
        };
        let value = row.json::<Value>(TABLE)?;
        destination.extend(value.as_object().ok_or_else(invalid)?.clone());
        work["archiveIntent"] = Value::Object(destination);
        if !unresolved {
            work["archive"] = value.get("outcome").cloned().ok_or_else(invalid)?;
        }
        (intent.task_id, work)
    } else if let Some(suffix) = row.id.strip_prefix("work-write-vm-intent:") {
        if row.signature != "work-write-vm-intent-v1" {
            return Err(invalid());
        }
        let (kind, task) = suffix.split_once(':').ok_or_else(invalid)?;
        let field = match kind {
            "submission" => "vmSubmit",
            "merge" => "vmAccept",
            _ => return Err(invalid()),
        };
        let transport: Value = row.json(TABLE)?;
        let (intent, status) = serde_json::from_value::<LegacyVmIntent>(transport)
            .map_err(|_| invalid())?
            .parts();
        if intent.task_id != task
            || intent.request_id.is_empty()
            || intent.owner.is_empty()
            || intent.revision.is_empty()
            || !intent.created.is_finite()
        {
            return Err(invalid());
        }
        let mut work = read_work(tx, task, row)?;
        let public = match (
            work[field].get("publicRequestId"),
            work[field].get("publicAdmission"),
        ) {
            (None, None) => None,
            (Some(id), Some(admission)) => Some(crate::vm_work_intent::PublicRequest {
                public_request_id: id.as_str().ok_or_else(invalid)?.to_owned(),
                public_admission: serde_json::from_value(admission.clone())
                    .map_err(|_| invalid())?,
            }),
            _ => return Err(invalid()),
        };
        use crate::vm_work_intent::{Acceptance, Submission, SubmissionResult};
        work[field] = match (kind, &intent.version, &intent.result_id, &intent.evidence) {
            ("submission", Field::Value(version), Field::Missing, Field::Missing) => {
                serde_json::to_value(
                    vm_target::<_, SubmissionResult>(
                        vm_owner(&intent, Submission { version: *version }, public),
                        status,
                        None,
                    )
                    .ok_or_else(invalid)?,
                )
            }
            ("submission", Field::Value(version), Field::Missing, Field::Value(evidence)) => {
                serde_json::to_value(
                    vm_target(
                        vm_owner(&intent, Submission { version: *version }, public),
                        status,
                        Some(SubmissionResult {
                            evidence: evidence.clone(),
                        }),
                    )
                    .ok_or_else(invalid)?,
                )
            }
            ("merge", Field::Missing, Field::Value(result_id), Field::Missing)
                if !result_id.is_empty() =>
            {
                serde_json::to_value(
                    vm_target(
                        vm_owner(
                            &intent,
                            Acceptance {
                                result_id: result_id.clone(),
                            },
                            public,
                        ),
                        status,
                        (status == WorkEffectStatus::Completed).then_some(()),
                    )
                    .ok_or_else(invalid)?,
                )
            }
            _ => return Err(invalid()),
        }
        .map_err(|_| invalid())?;
        (task.into(), work)
    } else {
        let reservation: Reservation = row.json(TABLE)?;
        if !reservation.vm_accept_reservation {
            return Err(invalid());
        }
        let ReservationStatus::Reserved = reservation.status;
        let mut work = read_work(tx, &reservation.task_id, row)?;
        let admitted = contract::admitted_owner_token(&row.id, &row.signature, cutover_ms);
        if work.get("vmAccept").is_some_and(|v| !v.is_object()) {
            return Err(invalid());
        }
        match (
            work["vmAccept"].get("publicRequestId"),
            work["vmAccept"].get("publicAdmission"),
        ) {
            (None, None) => {}
            (Some(id), Some(token))
                if id.as_str() == Some(row.id.as_str())
                    && serde_json::from_value::<crate::request_results::AdmittedRequest>(
                        token.clone(),
                    )
                    .ok()
                    .as_ref()
                        == Some(&admitted) => {}
            _ => return Err(invalid()),
        }
        if work.get("vmAccept").is_none() {
            let public = crate::vm_work_intent::PublicRequest {
                public_request_id: row.id.clone(),
                public_admission: admitted.clone(),
            };
            work["vmAccept"] = serde_json::to_value(crate::vm_work_intent::Intent::<
                crate::vm_work_intent::Acceptance,
                (),
            >::UnknownUnattributed(public))
            .map_err(|_| invalid())?;
        }
        work["vmAccept"]["publicAdmission"] = json!(admitted);
        work["vmAccept"]["publicRequestId"] = json!(row.id);
        let payload = WorkReservation {
            root: work["rootId"].as_str().ok_or_else(invalid)?.into(),
            agent: reservation.agent_id,
            task: reservation.task_id.clone(),
            request: row.id.clone(),
            admitted,
            archive: false,
        };
        preserve(
            tx,
            row,
            &reservation.task_id,
            OutboxKind::WorkVm,
            payload,
            cutover_ms,
        )?;
        (reservation.task_id, work)
    };
    if !work.is_object() {
        return Err(invalid());
    }
    tx.execute(
        "UPDATE runtime_work SET record=?2 WHERE id=?1",
        params![task, work.to_string()],
    )?;
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
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);CREATE TABLE runtime_work(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);").expect("tables");
        let tx = db.transaction().expect("tx");
        contract::prepare_schema(&tx).expect("schema");
        for source in SOURCES {
            MigrationCheckpoint::begin(&tx, Family.id(), source, 1000).expect("begin");
        }
        tx.commit().expect("commit");
        db
    }
    fn put(db: &Connection, id: &str, signature: &str, result: Value) {
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
            params![id, signature, result.to_string()],
        )
        .expect("receipt");
    }
    fn batch(db: &mut Connection, source: &str) -> Result<MigrationCheckpoint> {
        let tx = db.transaction()?;
        let cp = MigrationCheckpoint::load(&tx, Family.id(), source)?.expect("checkpoint");
        let next = Family.move_batch(&tx, source, &cp)?;
        tx.commit()?;
        Ok(next)
    }
    fn migrate(db: &mut Connection) {
        for source in SOURCES {
            while batch(db, source).expect("batch").status != MigrationStatus::Complete {}
        }
    }
    fn work(db: &Connection, id: &str) -> Value {
        let raw: String = db
            .query_row("SELECT record FROM runtime_work WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .expect("work");
        serde_json::from_str(&raw).expect("JSON")
    }
    #[test]
    fn receipts_work_vm_typed_source_preserves_last_key_and_refuses_null_proofs() {
        let mut db = fixture();
        db.execute("INSERT INTO runtime_work VALUES('t','{\"id\":\"t\"}')", [])
            .unwrap();
        db.execute("INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)", params![
            "work-write-vm-intent:submission:t", "work-write-vm-intent-v1",
            r#"{"status":"pending","status":"completed","taskId":"t","requestId":"old","requestId":"rpc","owner":"a","revision":"rev","version":1,"created":1.0,"evidence":{"layrProjectId":"p","layrLine":"a","layrStateId":"rev"}}"#
        ]).unwrap();
        migrate(&mut db);
        assert_eq!(
            work(&db, "t")["vmSubmit"],
            json!({"status":"completed","requestId":"rpc","owner":"a","revision":"rev","version":1,"evidence":{"layrProjectId":"p","layrLine":"a","layrStateId":"rev"}})
        );

        for (kind, extra) in [
            ("submission", json!({"version":null})),
            ("submission", json!({"version":1,"evidence":null})),
            ("submission", json!({"version":1,"resultId":null})),
            ("merge", json!({"resultId":null})),
            ("merge", json!({})),
        ] {
            let mut db = fixture();
            db.execute("INSERT INTO runtime_work VALUES('t','{\"id\":\"t\"}')", [])
                .unwrap();
            let mut source = json!({"status":"unknown","taskId":"t","requestId":"rpc","owner":"a","revision":"rev","created":1.0});
            source
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let id = format!("work-write-vm-intent:{kind}:t");
            put(&db, &id, "work-write-vm-intent-v1", source);
            assert!(batch(&mut db, TABLE).is_err());
            assert!(work(&db, "t").get("vmSubmit").is_none());
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM runtime_operation_receipts",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
                1
            );
        }
    }
    #[test]
    fn receipts_work_vm_reservation_never_overwrites_public_proof() {
        let mut db = fixture();
        let prior = contract::admitted_owner_token("prior", "sig", 1000);
        let original = json!({"id":"t","rootId":"root","vmAccept":{"status":"unknown_unattributed","publicRequestId":"prior","publicAdmission":prior}});
        db.execute(
            "INSERT INTO runtime_work VALUES('t',?1)",
            [original.to_string()],
        )
        .unwrap();
        put(
            &db,
            "other",
            "sig",
            json!({"vmAcceptReservation":true,"taskId":"t","agentId":"root","status":"reserved"}),
        );
        assert!(batch(&mut db, TABLE).is_err());
        assert_eq!(work(&db, "t"), original);
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_operation_receipts",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn receipts_work_vm_reservation_preserves_proof_in_both_source_orders() {
        for id in ["a-public", "z-public"] {
            let mut db = fixture();
            db.execute(
                "INSERT INTO runtime_work VALUES('t','{\"id\":\"t\",\"rootId\":\"root\"}')",
                [],
            )
            .unwrap();
            put(
                &db,
                id,
                "sig",
                json!({"vmAcceptReservation":true,"taskId":"t","agentId":"root","status":"reserved"}),
            );
            put(
                &db,
                "work-write-vm-intent:merge:t",
                "work-write-vm-intent-v1",
                json!({"status":"unknown","taskId":"t","requestId":"rpc","owner":"a","revision":"rev","resultId":"r","created":1.0}),
            );
            migrate(&mut db);
            let intent = &work(&db, "t")["vmAccept"];
            assert_eq!(intent["status"], "unknown");
            assert_eq!(intent["requestId"], "rpc");
            assert_eq!(intent["publicRequestId"], id);
            assert_eq!(intent["publicAdmission"]["id"], id);
        }
    }
    #[test]
    fn receipts_work_preserves_superseded_archive_and_public_vm_reservation() {
        let mut db = fixture();
        db.execute(
            "INSERT INTO runtime_work VALUES('t',?1)",
            [
                json!({"id":"t","rootId":"root","archiveIntent":{"id":"t:new","status":"pending"}})
                    .to_string(),
            ],
        )
        .expect("task");
        put(
            &db,
            "work-archive:t:old",
            "accepted-archive-v1",
            json!({"status":"unknown","taskId":"t","intentId":"t:old","target":"a","actor":"root","actorEpoch":2,"attempts":1,"created":1.0}),
        );
        put(
            &db,
            "public",
            "sig",
            json!({"vmAcceptReservation":true,"taskId":"t","agentId":"root","status":"reserved"}),
        );
        db.execute(
            "INSERT INTO runtime_work VALUES('current',?1)",
            [
                json!({"id":"current","status":"accepted","owner":"a","results":[{"id":"r"}]})
                    .to_string(),
            ],
        )
        .expect("old current owner");
        put(
            &db,
            "work-archive:current:r",
            "accepted-archive-v1",
            json!({"status":"unknown","taskId":"current","intentId":"current:r","target":"a","actor":"root","actorEpoch":2,"attempts":1,"created":1.0}),
        );
        migrate(&mut db);
        assert_eq!(work(&db, "current")["archiveIntent"]["status"], "unknown");
        assert_eq!(work(&db, "t")["archiveIntent"]["id"], "t:new");
        assert_eq!(work(&db, "t")["vmAccept"]["status"], "unknown_unattributed");
        assert_eq!(
            work(&db, "t")["vmAccept"]["publicAdmission"]["id"],
            "public"
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM runtime_outbox WHERE status='unknown' AND next_at IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .expect("both obligations"),
            2
        );
        let raw: String = db
            .query_row(
                "SELECT payload FROM runtime_outbox WHERE kind='work_vm'",
                [],
                |r| r.get(0),
            )
            .expect("reservation");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("reservation"),
            json!({"root":"root","agent":"root","task":"t","request":"public","admitted":{"id":"public","signature":"sig","created":1000},"archive":false})
        );
    }
    #[test]
    fn receipts_work_byte_budget_moves_oversized_row_alone() {
        let mut db = fixture();
        for (i, bytes) in [(0, contract::MIGRATION_BATCH_BYTES + 1), (1, 1)] {
            let id = format!("t{i}");
            db.execute(
                "INSERT INTO runtime_work VALUES(?1,?2)",
                params![id, json!({"id":id}).to_string()],
            )
            .expect("task");
            put(
                &db,
                &format!("work-write-vm-intent:submission:{id}"),
                "work-write-vm-intent-v1",
                json!({"status":"unknown","taskId":id,"requestId":"rpc","owner":"x".repeat(bytes),"revision":"rev","version":1,"created":1.0}),
            );
        }
        assert_eq!(batch(&mut db, TABLE).expect("oversized alone").cursor, 1);
        assert!(work(&db, "t1")["vmSubmit"].is_null());
        assert_eq!(batch(&mut db, TABLE).expect("tail").cursor, 2);
    }
    #[test]
    fn receipts_work_refusal_hash_uses_source_run_identity_without_runtime_pointer() {
        let mut db = fixture();
        db.execute_batch("CREATE TABLE runtime_execution_runs(id TEXT PRIMARY KEY,agent TEXT,account TEXT,epoch INTEGER,thread TEXT,turn TEXT,created REAL,record TEXT);").expect("source history");
        let agent = json!({"id":"a","rootId":"a","epoch":2,"status":"idle"});
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [agent.to_string()],
        )
        .expect("agent");
        db.execute(
            "INSERT INTO runtime_work VALUES('t',?1)",
            [json!({"id":"t","version":1,"owner":"a","rootId":"a"}).to_string()],
        )
        .expect("work");
        db.execute("INSERT INTO runtime_execution_runs VALUES('run','a','default',2,NULL,NULL,1.0,?1)",[json!({"id":"run","agent":"a","accountKey":"default","epoch":2,"status":"running","created":1.0,"latestAttemptId":"attempt"}).to_string()]).expect("run");
        let counter = format!(
            "work-result-failures:{}",
            crate::sha256_hex(
                json!(["t", 1, "a", 2, ["run", "attempt"]])
                    .to_string()
                    .as_bytes()
            )
        );
        put(
            &db,
            &counter,
            "work-result-failures-v1",
            json!({"failed":2,"errors":[]}),
        );
        migrate(&mut db);
        assert_eq!(
            work(&db, "t")["resultFailures"]["providerAttempt"],
            json!(["run", "attempt"])
        );
        assert_eq!(work(&db, "t")["resultFailures"]["count"], 2);
    }
    #[test]
    fn receipts_work_hydrates_old_counter_before_deleting_sources_and_reopens() {
        let mut db = fixture();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [json!({"id":"a","rootId":"a","epoch":2,"status":"idle"}).to_string()],
        )
        .expect("agent");
        db.execute("INSERT INTO runtime_work VALUES('t',?1)",[json!({"id":"t","version":1,"owner":"a","rootId":"a","archiveIntent":{"id":"t:r","owner":"a","status":"pending"}}).to_string()]).expect("task");
        let counter = format!(
            "work-result-failures:{}",
            crate::sha256_hex(json!(["t", 1, "a", 2, null]).to_string().as_bytes())
        );
        put(
            &db,
            &counter,
            "work-result-failures-v1",
            json!({"failed":2}),
        );
        put(
            &db,
            "old-submit",
            "sig",
            json!({"id":"t","version":1,"owner":"a","submissionRefusal":{"failedSubmits":2,"errors":[{"path":"/x","message":"bad"}]}}),
        );
        put(
            &db,
            "work-archive:t:r",
            "accepted-archive-v1",
            json!({"status":"complete","taskId":"t","intentId":"t:r","target":"a","actor":"a","actorEpoch":2,"attempts":1,"created":1.0,"outcome":{"status":"archived"}}),
        );
        put(
            &db,
            "work-write-vm-intent:submission:t",
            "work-write-vm-intent-v1",
            json!({"status":"unknown","taskId":"t","requestId":"rpc","owner":"a","revision":"rev","version":1,"created":1.0}),
        );
        migrate(&mut db);
        assert_eq!(
            work(&db, "t")["resultFailures"],
            json!({"version":1,"owner":"a","epoch":2,"providerAttempt":null,"count":2,"errors":[{"path":"/x","message":"bad"}]})
        );
        assert_eq!(work(&db, "t")["archive"], json!({"status":"archived"}));
        assert_eq!(
            work(&db, "t")["vmSubmit"],
            json!({"status":"unknown","requestId":"rpc","owner":"a","revision":"rev","version":1})
        );
        assert_eq!(work(&db, "t")["archiveIntent"]["owner"], "a");
        let before = work(&db, "t");
        migrate(&mut db);
        assert_eq!(work(&db, "t"), before);
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("retained public refusal"),
            1
        );
    }
    #[test]
    fn receipts_work_crash_between_batches_resumes_and_unclassifiable_aborts() {
        let directory = tempfile::tempdir().expect("temp database");
        let path = directory.path().join("canvas.sqlite3");
        let mut db = fixture_at(Some(&path));
        for i in 0..501 {
            let id = format!("t{i}");
            db.execute(
                "INSERT INTO runtime_work VALUES(?1,?2)",
                params![id, json!({"id":id}).to_string()],
            )
            .expect("task");
            put(
                &db,
                &format!("work-write-vm-intent:submission:{id}"),
                "work-write-vm-intent-v1",
                json!({"status":"unknown","taskId":id,"requestId":"rpc","owner":"a","revision":"rev","version":1,"created":1.0}),
            );
        }
        let first = batch(&mut db, TABLE).expect("batch");
        db.close().expect("interrupted between batches");
        let mut db = Connection::open(&path).expect("reopen");
        assert_eq!(first.cursor, 500);
        {
            let tx = db.transaction().expect("tx");
            Family.move_batch(&tx, TABLE, &first).expect("second");
            tx.rollback().expect("crash");
        }
        assert!(work(&db, "t500")["vmSubmit"].is_null());
        batch(&mut db, TABLE).expect("resume");
        assert_eq!(work(&db, "t500")["vmSubmit"]["status"], "unknown");
        let mut db = fixture();
        put(
            &db,
            "work-archive:bad",
            "unexpected",
            json!({"status":"unknown"}),
        );
        assert!(
            matches!(batch(&mut db,TABLE),Err(contract::ReceiptMigrationError::Unclassifiable{table,key,signature}) if table==TABLE&&key=="work-archive:bad"&&signature=="unexpected")
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source preserved"),
            1
        );
    }
}
