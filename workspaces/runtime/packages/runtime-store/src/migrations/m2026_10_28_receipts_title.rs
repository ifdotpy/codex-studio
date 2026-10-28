//! Move exact title replies and pending title jobs without provider redispatch.
use crate::receipt_migration_contract::{
    self as contract, MigrationCheckpoint, ReceiptFamilyMigration, Result,
};
use crate::{
    outbox::{Outbox, OutboxRow},
    request_results::{AdmittedRequest, ResultEnvelope},
};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{
    MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, TitleStatus,
};
use studio_runtime_schema::shared_values::TITLE_EFFECT_SIGNATURE;
pub(super) struct Family;
pub(super) const SOURCES: &[&str] = &["title-effects", "title-public"];
const TABLE: &str = "runtime_operation_receipts";
const EFFECTS: &str = "source.id GLOB 'title-generation:*'";
const PUBLIC: &str = "CASE WHEN json_valid(source.result) THEN json_type(source.result,'$.id')='text' AND json_extract(source.result,'$.request_id')=source.id AND json_type(source.result,'$.status')='text' ELSE 0 END";
const DISCARD_MESSAGE: &str = "The title request was discarded during migration because its original input could not be verified.";
impl ReceiptFamilyMigration for Family {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-title"
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
        let predicate = match source {
            "title-effects" => EFFECTS,
            "title-public" => PUBLIC,
            _ => return Err(contract::unclassifiable(TABLE, source, self.id())),
        };
        let rows = contract::read_receipt_batch(tx, TABLE, checkpoint, predicate)?;
        next.status = if rows.is_empty() {
            MigrationStatus::Complete
        } else {
            MigrationStatus::Pending
        };
        let mut budget = contract::BatchBudget::default();
        for row in rows {
            let extra = if source == SOURCES[0] {
                let effect: Effect = row.json(TABLE)?;
                tx.query_row("SELECT coalesce(length(CAST(result AS BLOB)),0) FROM runtime_operation_receipts WHERE id=?1",[effect.request_id],|r|r.get::<_,i64>(0)).optional()?.unwrap_or(0)
            } else {
                0
            };
            if !budget.include(
                row.result
                    .len()
                    .saturating_add(row.id.len())
                    .saturating_add(row.signature.len())
                    .saturating_add(
                        usize::try_from(extra).map_err(|_| {
                            contract::unclassifiable(TABLE, &row.id, &row.signature)
                        })?,
                    ),
            ) {
                break;
            }
            if source == SOURCES[0] {
                move_effect(tx, &row, checkpoint.cutover_ms)?;
            } else {
                move_public(tx, &row, checkpoint.cutover_ms)?;
            }
            contract::delete_source(tx, TABLE, row.rowid)?;
            next.cursor = row.rowid;
        }
        next.save(tx, self.id(), source, checkpoint.cutover_ms)?;
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        if !contract::source_exists(tx, TABLE)? {
            return Ok(());
        }
        let remains: bool = tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM {TABLE} AS source WHERE ({EFFECTS}) OR ({PUBLIC}))"
            ),
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
struct Effect {
    request_id: String,
    agent_id: String,
    state: TitleStatus,
    fingerprint: Option<String>,
    epoch: Option<i64>,
    title: Option<String>,
    #[serde(rename = "titleFingerprintProvider20261026")]
    old_provider: Option<Value>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TitleReply {
    id: String,
    request_id: String,
    status: TitleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TitleJob {
    request_id: String,
    agent_id: String,
    root_request_id: String,
    admitted_request: AdmittedRequest,
    status: TitleStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    epoch: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
}
fn rename_signature(agent: &str) -> String {
    // This frozen migration boundary preserves Python json.dumps separators.
    crate::sha256_hex(
        format!(
            "{{\"chat\": {}, \"name\": null, \"operation\": \"rename\"}}",
            json!(agent)
        )
        .as_bytes(),
    )
}
fn effect_id(request: &str) -> String {
    format!("title-generation:{}", crate::sha256_hex(request.as_bytes()))
}
fn public_reply(row: &contract::SourceReceipt) -> Result<TitleReply> {
    let reply: TitleReply = row.json(TABLE)?;
    if reply.request_id != row.id
        || reply.id.is_empty()
        || row.signature != rename_signature(&reply.id)
    {
        return Err(contract::unclassifiable(TABLE, &row.id, &row.signature));
    }
    if !matches!(
        reply.status,
        TitleStatus::Pending | TitleStatus::Applied | TitleStatus::Failed
    ) || (reply.status == TitleStatus::Applied && reply.name.is_none())
    {
        return Err(contract::unclassifiable(TABLE, &row.id, &row.signature));
    }
    Ok(reply)
}
fn read_public(tx: &Transaction<'_>, request: &str) -> Result<Option<contract::SourceReceipt>> {
    Ok(tx
        .query_row(
            "SELECT rowid,id,signature,result FROM runtime_operation_receipts WHERE id=?1",
            [request],
            |r| {
                Ok(contract::SourceReceipt {
                    rowid: r.get(0)?,
                    id: r.get(1)?,
                    signature: r.get(2)?,
                    result: r.get(3)?,
                })
            },
        )
        .optional()?)
}
fn save_reply(
    tx: &Transaction<'_>,
    row: &contract::SourceReceipt,
    reply: impl Serialize,
    cutover_ms: i64,
) -> Result<()> {
    contract::import_result(
        tx,
        &row.id,
        &row.signature,
        &ResultEnvelope {
            actor: None,
            reply,
            references: vec![],
        },
        cutover_ms,
    )
}
fn save_job(tx: &Transaction<'_>, job: &TitleJob, replace_current: bool) -> Result<()> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [&job.agent_id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(raw) = raw {
        let mut agent: Value = serde_json::from_str(&raw).map_err(|_| {
            contract::unclassifiable("runtime_agents", &job.agent_id, TITLE_EFFECT_SIGNATURE)
        })?;
        if !agent.is_object() || agent["id"] != job.agent_id {
            return Err(contract::unclassifiable(
                "runtime_agents",
                &job.agent_id,
                TITLE_EFFECT_SIGNATURE,
            ));
        }
        if replace_current
            || agent
                .get("titleJob")
                .is_none_or(|current| current["requestId"] == job.request_id)
        {
            agent["titleJob"] = json!(job);
            tx.execute(
                "UPDATE runtime_agents SET record=?2 WHERE id=?1",
                params![job.agent_id, agent.to_string()],
            )?;
        }
    }
    let status = match job.status {
        TitleStatus::NotSent | TitleStatus::Scheduled => OutboxStatus::Pending,
        TitleStatus::Pending | TitleStatus::Submitted | TitleStatus::Unknown => {
            OutboxStatus::Unknown
        }
        TitleStatus::Written => OutboxStatus::Submitted,
        TitleStatus::Invalid
        | TitleStatus::Discarded
        | TitleStatus::Applied
        | TitleStatus::Failed => return Ok(()),
    };
    contract::import_outbox(
        tx,
        &OutboxRow {
            id: Outbox::effect_id(
                &job.request_id,
                OutboxKind::TitleGeneration,
                OwnerKind::Agent,
                &job.agent_id,
                &[],
            ),
            kind: OutboxKind::TitleGeneration,
            owner_kind: OwnerKind::Agent,
            owner_id: job.agent_id.clone(),
            request_id: Some(job.request_id.clone()),
            signature: TITLE_EFFECT_SIGNATURE.into(),
            status,
            account: None,
            stream_id: None,
            generation: None,
            ordinal: None,
            next_at: None,
            payload: job,
            created: job.admitted_request.created(),
        },
    )
}
fn new_job(request: &str, agent: &str, cutover_ms: i64) -> TitleJob {
    TitleJob {
        request_id: request.into(),
        agent_id: agent.into(),
        root_request_id: RequestIdentity::issue_root(),
        admitted_request: contract::admitted_owner_token(
            request,
            &rename_signature(agent),
            cutover_ms,
        ),
        status: TitleStatus::Unknown,
        fingerprint: None,
        epoch: None,
        original_name: None,
        title: None,
    }
}
fn move_effect(tx: &Transaction<'_>, row: &contract::SourceReceipt, cutover_ms: i64) -> Result<()> {
    let invalid = || contract::unclassifiable(TABLE, &row.id, &row.signature);
    let effect: Effect = row.json(TABLE)?;
    if row.signature != TITLE_EFFECT_SIGNATURE
        || row.id != effect_id(&effect.request_id)
        || effect.agent_id.is_empty()
        || effect.old_provider.is_some()
    {
        return Err(invalid());
    }
    let public = read_public(tx, &effect.request_id)?.ok_or_else(invalid)?;
    let reply = public_reply(&public)?;
    if reply.id != effect.agent_id {
        return Err(invalid());
    }
    let mut job = new_job(&effect.request_id, &effect.agent_id, cutover_ms);
    job.status = effect.state;
    job.fingerprint = effect.fingerprint;
    job.epoch = effect.epoch;
    job.title = effect.title;
    let tagged = row
        .json::<Value>(TABLE)?
        .get("titleFingerprintProvider20261026")
        .is_some();
    if reply.status != TitleStatus::Pending {
        if !matches!(
            job.status,
            TitleStatus::Applied
                | TitleStatus::Failed
                | TitleStatus::Invalid
                | TitleStatus::Discarded
        ) {
            return Err(invalid());
        }
        save_reply(tx, &public, public.json::<Value>(TABLE)?, cutover_ms)?;
    } else if job.status == TitleStatus::Written
        || (tagged && matches!(job.status, TitleStatus::NotSent | TitleStatus::Scheduled))
    {
        // Owner-approved cosmetic cutover: without immutable original input,
        // discard this job and persist a failure instead of applying or sending.
        job.status = TitleStatus::Discarded;
        save_reply(
            tx,
            &public,
            TitleReply {
                id: job.agent_id.clone(),
                request_id: job.request_id.clone(),
                status: TitleStatus::Failed,
                name: None,
                error: Some(DISCARD_MESSAGE.into()),
            },
            cutover_ms,
        )?;
    } else if matches!(
        job.status,
        TitleStatus::Applied | TitleStatus::Failed | TitleStatus::Invalid | TitleStatus::Discarded
    ) {
        // A terminal internal state with a still-pending public reply was never
        // fully settled. Retain the obligation for explicit reconciliation.
        job.status = TitleStatus::Unknown;
    }
    save_job(tx, &job, true)
}
fn move_public(tx: &Transaction<'_>, row: &contract::SourceReceipt, cutover_ms: i64) -> Result<()> {
    let reply = public_reply(row)?;
    let saved: Option<String> = tx
        .query_row(
            "SELECT signature FROM request_results WHERE id=?1",
            [&row.id],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(signature) = saved {
        if signature != row.signature {
            return Err(contract::unclassifiable(TABLE, &row.id, &row.signature));
        }
        if reply.status != TitleStatus::Pending {
            save_reply(tx, row, row.json::<Value>(TABLE)?, cutover_ms)?;
        }
        return Ok(());
    }
    if reply.status != TitleStatus::Pending {
        return save_reply(tx, row, row.json::<Value>(TABLE)?, cutover_ms);
    }
    let id = Outbox::effect_id(
        &row.id,
        OutboxKind::TitleGeneration,
        OwnerKind::Agent,
        &reply.id,
        &[],
    );
    let existing = Outbox::get::<TitleJob>(tx, &id)
        .map_err(|_| contract::unclassifiable(TABLE, &row.id, &row.signature))?;
    if existing.is_none() {
        save_job(tx, &new_job(&row.id, &reply.id, cutover_ms), false)?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::{Value, json};
    fn signature(agent: &str) -> String {
        crate::sha256_hex(
            format!(
                "{{\"chat\": {}, \"name\": null, \"operation\": \"rename\"}}",
                serde_json::to_string(agent).expect("agent")
            )
            .as_bytes(),
        )
    }
    fn effect(request: &str) -> String {
        format!("title-generation:{}", crate::sha256_hex(request.as_bytes()))
    }
    fn fixture() -> Connection {
        fixture_at(None)
    }
    fn fixture_at(path: Option<&std::path::Path>) -> Connection {
        let mut db = path
            .map(Connection::open)
            .unwrap_or_else(Connection::open_in_memory)
            .expect("db");
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT);CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);").expect("tables");
        let tx = db.transaction().expect("tx");
        contract::prepare_schema(&tx).expect("schema");
        for source in SOURCES {
            MigrationCheckpoint::begin(&tx, Family.id(), source, 1000).expect("begin");
        }
        tx.commit().expect("commit");
        db
    }
    fn put(db: &Connection, id: &str, sig: &str, result: Value) {
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,?2,?3)",
            params![id, sig, result.to_string()],
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
    #[test]
    fn receipts_title_byte_budget_includes_correlated_public_reply() {
        let mut db = fixture();
        for i in 0..2 {
            let request = format!("r{i}");
            put(
                &db,
                &request,
                &signature("a"),
                json!({"id":"a","request_id":request,"status":"failed","error":"x".repeat(contract::MIGRATION_BATCH_BYTES/2)}),
            );
            put(
                &db,
                &effect(&request),
                "title-generation-effect-v1",
                json!({"request_id":request,"agent_id":"a","state":"failed"}),
            );
        }
        assert_eq!(
            batch(&mut db, SOURCES[0])
                .expect("bounded correlated input")
                .cursor,
            2
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM request_results", [], |r| r
                .get::<_, i64>(0))
                .expect("one reply"),
            1
        );
        assert_eq!(batch(&mut db, SOURCES[0]).expect("tail").cursor, 4);
    }
    #[test]
    fn receipts_title_tagged_unsent_is_discarded_and_missing_effect_is_held() {
        let mut db = fixture();
        for (request, state) in [("tagged", Some("not_sent")), ("missing", None)] {
            put(
                &db,
                request,
                &signature("a"),
                json!({"id":"a","request_id":request,"status":"pending"}),
            );
            if let Some(state) = state {
                put(
                    &db,
                    &effect(request),
                    "title-generation-effect-v1",
                    json!({"request_id":request,"agent_id":"a","state":state,"fingerprint":"historical","titleFingerprintProvider20261026":null}),
                );
            }
        }
        migrate(&mut db);
        let raw: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id='tagged'",
                [],
                |r| r.get(0),
            )
            .expect("discard failure");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("reply")["reply"]["status"],
            "failed"
        );
        assert_eq!(
            db.query_row(
                "SELECT status FROM runtime_outbox WHERE request_id='missing'",
                [],
                |r| r.get::<_, String>(0)
            )
            .expect("missing effect held"),
            "unknown"
        );
    }
    #[test]
    fn receipts_title_fixture_preserves_unknown_and_exact_final_replies_and_reopens() {
        let mut db = fixture();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [json!({"id":"a","name":"Current name","epoch":3}).to_string()],
        )
        .expect("agent");
        put(
            &db,
            "done",
            &signature("a"),
            json!({"id":"a","request_id":"done","status":"applied","name":"Historical name"}),
        );
        put(
            &db,
            "pending",
            &signature("a"),
            json!({"id":"a","request_id":"pending","status":"pending"}),
        );
        put(
            &db,
            &effect("pending"),
            "title-generation-effect-v1",
            json!({"request_id":"pending","agent_id":"a","state":"unknown","fingerprint":"exact","epoch":3}),
        );
        migrate(&mut db);
        let raw: String = db
            .query_row(
                "SELECT result FROM request_results WHERE id='done'",
                [],
                |r| r.get(0),
            )
            .expect("exact response");
        let saved: Value = serde_json::from_str(&raw).expect("result");
        assert_eq!(saved["reply"]["name"], "Historical name");
        let (status, payload, next): (String, String, Option<i64>) = db
            .query_row(
                "SELECT status,payload,next_at FROM runtime_outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("outbox");
        assert_eq!(status, "unknown");
        assert_eq!(next, None);
        let payload: Value = serde_json::from_str(&payload).expect("job");
        assert_eq!(payload["fingerprint"], "exact");
        assert_eq!(payload["admittedRequest"]["created"], 1000);
        let before = payload;
        migrate(&mut db);
        let raw: String = db
            .query_row("SELECT payload FROM runtime_outbox", [], |r| r.get(0))
            .expect("same job");
        assert_eq!(serde_json::from_str::<Value>(&raw).expect("job"), before);
    }
    #[test]
    fn receipts_title_written_without_original_proof_is_terminal_failure() {
        let mut db = fixture();
        db.execute(
            "INSERT INTO runtime_agents VALUES('a',?1)",
            [json!({"id":"a","name":"Keep me","epoch":3}).to_string()],
        )
        .expect("agent");
        put(
            &db,
            "r",
            &signature("a"),
            json!({"id":"a","request_id":"r","status":"pending"}),
        );
        put(
            &db,
            &effect("r"),
            "title-generation-effect-v1",
            json!({"request_id":"r","agent_id":"a","state":"written","title":"Generated","epoch":3,"fingerprint":"old"}),
        );
        migrate(&mut db);
        let raw: String = db
            .query_row("SELECT result FROM request_results WHERE id='r'", [], |r| {
                r.get(0)
            })
            .expect("failure");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("reply")["reply"]["status"],
            "failed"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("settled"),
            0
        );
        let raw: String = db
            .query_row("SELECT record FROM runtime_agents WHERE id='a'", [], |r| {
                r.get(0)
            })
            .expect("agent");
        let agent: Value = serde_json::from_str(&raw).expect("agent");
        assert_eq!(agent["name"], "Keep me");
        assert_eq!(agent["titleJob"]["status"], "discarded");
    }
    #[test]
    fn receipts_title_batches_resume_and_bad_signature_rolls_back() {
        let directory = tempfile::tempdir().expect("temp database");
        let path = directory.path().join("canvas.sqlite3");
        let mut db = fixture_at(Some(&path));
        db.execute(
            "INSERT INTO runtime_agents VALUES('a','{\"id\":\"a\"}')",
            [],
        )
        .expect("agent");
        for i in 0..501 {
            let request = format!("r{i}");
            put(
                &db,
                &request,
                &signature("a"),
                json!({"id":"a","request_id":request,"status":"pending"}),
            );
            put(
                &db,
                &effect(&request),
                "title-generation-effect-v1",
                json!({"request_id":request,"agent_id":"a","state":"unknown"}),
            );
        }
        let first = batch(&mut db, SOURCES[0]).expect("first");
        db.close().expect("interrupted between batches");
        let mut db = Connection::open(&path).expect("reopen");
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("bounded"),
            500
        );
        {
            let tx = db.transaction().expect("tx");
            Family.move_batch(&tx, SOURCES[0], &first).expect("second");
            tx.rollback().expect("crash");
        }
        batch(&mut db, SOURCES[0]).expect("resume");
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_outbox", [], |r| r
                .get::<_, i64>(0))
                .expect("once"),
            501
        );
        let mut db = fixture();
        put(
            &db,
            &effect("bad"),
            "wrong",
            json!({"request_id":"bad","agent_id":"a","state":"unknown"}),
        );
        assert!(matches!(
            batch(&mut db, SOURCES[0]),
            Err(contract::ReceiptMigrationError::Unclassifiable { .. })
        ));
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source"),
            1
        );
    }
}
