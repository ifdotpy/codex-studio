//! Capture voice-stop authority before removing old lifecycle receipt rows.
use crate::outbox::OutboxRow;
use crate::receipt_migration_contract::{
    MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, Result, SourceReceipt,
    admitted_owner_token, delete_source, import_outbox, read_receipt_batch, source_exists,
    unclassifiable,
};
use crate::request_results::AdmittedRequest;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_providers::Method;
use studio_runtime_schema::persistence::{
    MigrationStatus, OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, VoiceStopOutcome,
};
use studio_runtime_schema::provider_call_contract::{
    ConnectionScope, ProviderCallIntent, RequestActor, RequestResult, RetainedCall, transport_key,
};
type Call = ProviderCallIntent<Method, AdmittedRequest>;
pub(super) struct VoiceMigration;
pub(super) const SOURCES: &[&str] = &["runtime_operation_receipts"];
const VOICE: &str = "source.id GLOB 'agent-voice-stop:*' OR source.id GLOB 'agent-delete-voice:*'";
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct VoiceDeleteSession {
    session_id: String,
    agent_id: String,
    epoch: i64,
    account_key: String,
    thread_id: String,
    connection_id: String,
    generation: u64,
    #[serde(default)]
    admitted_request: Option<AdmittedRequest>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct VoiceStopPayload {
    agent: String,
    epoch: i64,
    #[serde(default)]
    root_request_id: String,
    #[serde(default)]
    admission: Option<AdmittedRequest>,
    voice_end_sessions: Vec<VoiceDeleteSession>,
    #[serde(default)]
    voice_end_held_uncertain: Vec<VoiceDeleteSession>,
    #[serde(default)]
    voice_end_results: BTreeMap<String, VoiceStopOutcome>,
}
#[derive(Serialize)]
#[serde(tag = "family", content = "payload", rename_all = "snake_case")]
enum VoiceProviderCall {
    Retained(RetainedCall<Method, AdmittedRequest>),
}

fn prior(
    tx: &Transaction<'_>,
    session: &mut VoiceDeleteSession,
    operation: &str,
    cutover: i64,
) -> Result<Option<VoiceStopOutcome>> {
    let prefix = format!(
        "account-worker:{}",
        transport_key(&session.account_key, operation)
    );
    let id = format!("{prefix}:submitted");
    let saved: Option<(String, String)> = tx
        .query_row(
            "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((signature, raw)) = saved else {
        return Ok(None);
    };
    let invalid = || unclassifiable(SOURCES[0], &id, &signature);
    let mut saved: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
    saved["call"]["root_request_id"] = json!("");
    saved["call"]["admitted_request"] = Value::Null;
    let call: Call = serde_json::from_value(saved["call"].clone()).map_err(|_| invalid())?;
    let actor = call.actor.as_ref().ok_or_else(invalid)?;
    if call.operation != operation
        || call.method != Method::ThreadRealtimeStop
        || actor.agent != session.agent_id
        || actor.epoch > session.epoch
        || call.scope.account != session.account_key
        || call.scope.connection != session.connection_id
        || call.scope.generation != session.generation
        || call.params["threadId"] != session.thread_id
    {
        return Err(invalid());
    }
    if call.deleted_actor_session.as_deref()
        != operation
            .starts_with("voice:delete-stop:")
            .then_some(session.session_id.as_str())
    {
        return Err(invalid());
    }
    session.epoch = actor.epoch;
    let _root = crate::receipt_migration_contract::hydrate_source_root(
        tx,
        SOURCES[0],
        &id,
        &signature,
        "result",
        "$.call.root_request_id",
    )?;
    session.admitted_request = Some(admitted_owner_token(
        &format!("{prefix}:result"),
        &call.fingerprint(&call.scope.account, call.method.as_str()),
        cutover,
    ));
    let settled: Option<String> = tx
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [format!("{prefix}:result")],
            |r| r.get(0),
        )
        .optional()?;
    let Some(settled) = settled else {
        return Ok(Some(VoiceStopOutcome::Submitted));
    };
    let settled: Value = serde_json::from_str(&settled).map_err(|_| invalid())?;
    let result: RequestResult =
        serde_json::from_value(settled["result"].clone()).map_err(|_| invalid())?;
    Ok(Some(match result {
        RequestResult::Reply(_) => VoiceStopOutcome::Written,
        RequestResult::Rejected(_) => VoiceStopOutcome::Rejected,
        RequestResult::NotSent => VoiceStopOutcome::NotSent,
        RequestResult::Unknown => VoiceStopOutcome::Unknown,
    }))
}
fn reserve(
    tx: &Transaction<'_>,
    owner: &AdmittedRequest,
    root: &str,
    session: &mut VoiceDeleteSession,
    deleted: bool,
) -> Result<()> {
    let operation = format!(
        "voice:{}:{}",
        if deleted { "delete-stop" } else { "stop" },
        session.session_id
    );
    let mut call = Call {
        root_request_id: root.into(),
        admitted_request: None,
        scope: ConnectionScope {
            account: session.account_key.clone(),
            connection: session.connection_id.clone(),
            generation: session.generation,
        },
        operation: operation.clone(),
        actor: Some(RequestActor {
            agent: session.agent_id.clone(),
            epoch: session.epoch,
        }),
        transfer: None,
        deleted_actor_session: deleted.then(|| session.session_id.clone()),
        method: Method::ThreadRealtimeStop,
        params: json!({"threadId":session.thread_id}),
    };
    let signature = call.fingerprint(&call.scope.account, call.method.as_str());
    let parts = call.effect_discriminator(&call.scope.account);
    let final_token = owner.derive_result(
        "provider-call",
        &parts.iter().map(String::as_str).collect::<Vec<_>>(),
        &signature,
    );
    call.admitted_request = Some(final_token.clone());
    session.admitted_request = Some(final_token.clone());
    let vm: bool = if source_exists(tx, "runtime_agents")? {
        tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE id=?1 AND json_extract(record,'$.executionMode')='vm')",[&session.agent_id],|r|r.get(0))?
    } else {
        false
    };
    import_outbox(
        tx,
        &OutboxRow {
            id: transport_key(&session.account_key, &operation),
            kind: OutboxKind::ProviderCall,
            owner_kind: OwnerKind::Agent,
            owner_id: session.agent_id.clone(),
            request_id: Some(final_token.id().into()),
            signature,
            status: OutboxStatus::Pending,
            account: Some(session.account_key.clone()),
            stream_id: Some(if vm {
                format!("{}:vm:{}", session.account_key, session.agent_id)
            } else {
                session.account_key.clone()
            }),
            generation: Some(
                i64::try_from(session.generation)
                    .map_err(|_| unclassifiable(SOURCES[0], owner.id(), owner.signature()))?,
            ),
            ordinal: None,
            next_at: None,
            created: owner.created(),
            payload: VoiceProviderCall::Retained(RetainedCall {
                call,
                request: None,
                admitted: final_token,
                written: false,
                outcome: None,
            }),
        },
    )
}
fn capture(tx: &Transaction<'_>, row: &SourceReceipt, value: &Value) -> Result<VoiceStopPayload> {
    let invalid = || unclassifiable(SOURCES[0], &row.id, &row.signature);
    let agent = value["agent"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(invalid)?;
    let epoch = value["epoch"].as_i64().ok_or_else(invalid)?;
    if value["status"] != "pending" && value["status"] != "unknown" {
        return Err(invalid());
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent],
            |r| r.get(0),
        )
        .optional()?;
    let owner: Value = serde_json::from_str(&raw.ok_or_else(invalid)?).map_err(|_| invalid())?;
    if !owner.is_object() {
        return Err(invalid());
    }
    let account = owner["accountKey"].as_str().unwrap_or("default");
    let thread = owner["threadId"].as_str();
    let mut sessions = Vec::new();
    if let Some(thread) = thread {
        let mut query=tx.prepare("SELECT id,connection_id FROM voice_sessions WHERE agent=?1 AND ended IS NULL AND account_key=?2 AND native_thread=?3 AND connection_id IS NOT NULL ORDER BY created,id")?;
        for result in query.query_map(params![agent, account, thread], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (session_id, connection_id) = result?;
            let generation = connection_id.parse().unwrap_or(0);
            sessions.push(VoiceDeleteSession {
                session_id,
                agent_id: agent.into(),
                epoch,
                account_key: account.into(),
                thread_id: thread.into(),
                connection_id,
                generation,
                admitted_request: None,
            });
        }
    }
    let outcomes = if value["status"] == "unknown" {
        sessions
            .iter()
            .map(|s| (s.session_id.clone(), VoiceStopOutcome::Unknown))
            .collect()
    } else {
        BTreeMap::new()
    };
    Ok(VoiceStopPayload {
        agent: agent.into(),
        epoch,
        root_request_id: String::new(),
        admission: None,
        voice_end_sessions: sessions,
        voice_end_held_uncertain: vec![],
        voice_end_results: outcomes,
    })
}
fn convert(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    payload: &mut VoiceStopPayload,
    kind: OutboxKind,
    cutover: i64,
) -> Result<()> {
    let invalid = || unclassifiable(SOURCES[0], &row.id, &row.signature);
    let owner = admitted_owner_token(&row.id, &row.signature, cutover);
    payload.admission = Some(owner.clone());
    if payload.root_request_id.is_empty() {
        payload.root_request_id = RequestIdentity::issue_root();
    }
    let original = std::mem::take(&mut payload.voice_end_sessions)
        .into_iter()
        .map(|s| (s, false))
        .chain(
            std::mem::take(&mut payload.voice_end_held_uncertain)
                .into_iter()
                .map(|s| (s, true)),
        );
    for (mut session, held) in original {
        if session.agent_id != payload.agent || session.epoch > payload.epoch {
            return Err(invalid());
        }
        let old = payload.voice_end_results.get(&session.session_id).copied();
        if !held
            && matches!(
                old,
                Some(VoiceStopOutcome::Written | VoiceStopOutcome::Rejected)
            )
        {
            continue;
        }
        let deleted = kind == OutboxKind::VoiceDelete;
        let operation = format!(
            "voice:{}:{}",
            if deleted && !held {
                "delete-stop"
            } else {
                "stop"
            },
            session.session_id
        );
        let mut outcome = prior(tx, &mut session, &operation, cutover)?;
        if held && outcome.is_none() {
            payload.voice_end_results.insert(
                session.session_id.clone(),
                old.unwrap_or(VoiceStopOutcome::Unknown),
            );
            payload.voice_end_held_uncertain.push(session);
            continue;
        }
        if deleted && (held || outcome.is_none()) {
            if !held {
                let operation = format!("voice:stop:{}", session.session_id);
                outcome = prior(tx, &mut session, &operation, cutover)?;
            }
            match outcome {
                Some(VoiceStopOutcome::Expired) => return Err(invalid()),
                Some(VoiceStopOutcome::Submitted | VoiceStopOutcome::Unknown) => {
                    payload
                        .voice_end_results
                        .insert(session.session_id.clone(), outcome.ok_or_else(invalid)?);
                    payload.voice_end_held_uncertain.push(session);
                    continue;
                }
                Some(VoiceStopOutcome::Written) => continue,
                Some(VoiceStopOutcome::NotSent | VoiceStopOutcome::Rejected) | None => {
                    session.epoch = payload.epoch;
                    session.admitted_request = None;
                    outcome = None;
                }
            }
        }
        outcome = outcome.or(old);
        if matches!(
            outcome,
            Some(VoiceStopOutcome::Written | VoiceStopOutcome::Rejected)
        ) {
            continue;
        }
        if session.generation == 0 {
            outcome = Some(VoiceStopOutcome::Unknown);
        }
        if matches!(outcome, None | Some(VoiceStopOutcome::NotSent))
            && session.admitted_request.is_none()
        {
            reserve(tx, &owner, &payload.root_request_id, &mut session, deleted)?;
        }
        if let Some(outcome) = outcome {
            payload
                .voice_end_results
                .insert(session.session_id.clone(), outcome);
        } else {
            payload.voice_end_results.remove(&session.session_id);
        }
        payload.voice_end_sessions.push(session);
    }
    Ok(())
}
impl ReceiptFamilyMigration for VoiceMigration {
    fn id(&self) -> &'static str {
        "2026-10-28-receipts-voice"
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
        if source != SOURCES[0] {
            return Err(ReceiptMigrationError::Incomplete {
                migration: self.id().into(),
                source_table: source.into(),
            });
        }
        if !source_exists(tx, source)? {
            next.status = MigrationStatus::Complete;
            return Ok(next);
        }
        for row in read_receipt_batch(tx, source, checkpoint, VOICE)? {
            let invalid = || unclassifiable(source, &row.id, &row.signature);
            let value: Value = row.json(source)?;
            let (kind, mut payload) = if row.id.starts_with("agent-delete-voice:")
                && row.signature == "voice-agent-delete-v1"
            {
                (
                    OutboxKind::VoiceDelete,
                    serde_json::from_value::<VoiceStopPayload>(value).map_err(|_| invalid())?,
                )
            } else if row.id.starts_with("agent-voice-stop:")
                && row.signature == "lifecycle.voice-stop.v1"
            {
                (OutboxKind::VoiceStop, capture(tx, &row, &value)?)
            } else {
                return Err(invalid());
            };
            if row.id
                != format!(
                    "{}:{}:{}",
                    if kind == OutboxKind::VoiceDelete {
                        "agent-delete-voice"
                    } else {
                        "agent-voice-stop"
                    },
                    payload.agent,
                    payload.epoch
                )
            {
                return Err(invalid());
            }
            convert(tx, &row, &mut payload, kind, checkpoint.cutover_ms)?;
            if !payload.voice_end_sessions.is_empty()
                || !payload.voice_end_held_uncertain.is_empty()
            {
                let pending = payload.voice_end_sessions.iter().any(|s| {
                    matches!(
                        payload.voice_end_results.get(&s.session_id),
                        None | Some(VoiceStopOutcome::NotSent)
                    )
                });
                import_outbox(
                    tx,
                    &OutboxRow {
                        id: row.id.clone(),
                        kind,
                        owner_kind: OwnerKind::Agent,
                        owner_id: payload.agent.clone(),
                        request_id: Some(row.id.clone()),
                        signature: row.signature.clone(),
                        status: if pending {
                            OutboxStatus::Pending
                        } else {
                            OutboxStatus::Unknown
                        },
                        account: None,
                        stream_id: None,
                        generation: None,
                        ordinal: None,
                        next_at: if pending {
                            Some(checkpoint.cutover_ms)
                        } else {
                            None
                        },
                        payload,
                        created: checkpoint.cutover_ms,
                    },
                )?;
            }
            delete_source(tx, source, row.rowid)?;
            next.cursor = row.rowid;
        }
        next.status=if tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts source WHERE source.rowid>?1 AND ({VOICE}))"),[next.cursor],|r|r.get::<_,bool>(0))?{MigrationStatus::Running}else{MigrationStatus::Complete};
        Ok(next)
    }
    fn finish(&self, tx: &Transaction<'_>) -> Result<()> {
        if source_exists(tx, SOURCES[0])?
            && tx.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts source WHERE {VOICE})"
                ),
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
    use crate::outbox::Outbox;
    use crate::receipt_migration_contract::{MIGRATION_BATCH_ROWS, prepare_schema};
    use rusqlite::Connection;
    fn fixture(db: &Connection) {
        db.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT,result TEXT); CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT); CREATE TABLE voice_sessions(id TEXT PRIMARY KEY,agent TEXT,created REAL,ended REAL,account_key TEXT,native_thread TEXT,connection_id TEXT);").expect("source");
    }
    fn payload(n: usize, outcome: Option<VoiceStopOutcome>) -> VoiceStopPayload {
        VoiceStopPayload {
            agent: format!("agent-{n}"),
            epoch: 1,
            root_request_id: String::new(),
            admission: None,
            voice_end_sessions: vec![VoiceDeleteSession {
                session_id: format!("session-{n}"),
                agent_id: format!("agent-{n}"),
                epoch: 1,
                account_key: "account".into(),
                thread_id: "thread".into(),
                connection_id: "1".into(),
                generation: 1,
                admitted_request: None,
            }],
            voice_end_held_uncertain: vec![],
            voice_end_results: outcome
                .into_iter()
                .map(|o| (format!("session-{n}"), o))
                .collect(),
        }
    }
    fn insert(db: &Connection, n: usize, outcome: Option<VoiceStopOutcome>) {
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'voice-agent-delete-v1',?2)",
            params![
                format!("agent-delete-voice:agent-{n}:1"),
                serde_json::to_string(&payload(n, outcome)).expect("JSON")
            ],
        )
        .expect("receipt");
    }
    fn batch(db: &mut Connection) -> Result<MigrationCheckpoint> {
        let tx = db.transaction()?;
        prepare_schema(&tx)?;
        let cp = MigrationCheckpoint::begin(&tx, VoiceMigration.id(), SOURCES[0], 1000)?;
        let next = VoiceMigration.move_batch(&tx, SOURCES[0], &cp)?;
        next.save(&tx, VoiceMigration.id(), SOURCES[0], 1000)?;
        tx.commit()?;
        Ok(next)
    }
    #[test]
    fn receipts_voice_deleted_owner_fixture_reopen_unknown_and_settled() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        insert(&db, 0, Some(VoiceStopOutcome::Unknown));
        insert(&db, 1, Some(VoiceStopOutcome::Written));
        insert(&db, 2, None);
        assert_eq!(
            batch(&mut db).expect("migrate").status,
            MigrationStatus::Complete
        );
        let row: OutboxRow<VoiceStopPayload> = Outbox::get(&db, "agent-delete-voice:agent-0:1")
            .expect("read")
            .expect("unknown");
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(row.next_at, None);
        assert_eq!(row.payload.voice_end_sessions[0].connection_id, "1");
        assert!(!row.payload.root_request_id.is_empty());
        let root = row.payload.root_request_id.clone();
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        batch(&mut db).expect("second open");
        let row: OutboxRow<VoiceStopPayload> = Outbox::get(&db, "agent-delete-voice:agent-0:1")
            .expect("read")
            .expect("unknown");
        assert_eq!(row.payload.root_request_id, root);
        assert!(
            Outbox::get::<Value>(&db, "agent-delete-voice:agent-1:1")
                .expect("read")
                .is_none()
        );
        assert_eq!(
            Outbox::pending::<Value>(&db, OutboxKind::VoiceDelete, 2000, 500)
                .expect("safe pending")
                .len(),
            1
        );
    }
    #[test]
    fn receipts_voice_crash_between_batches_resumes_and_bad_row_aborts() {
        let dir = tempfile::tempdir().expect("directory");
        let path = dir.path().join("test.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        {
            let setup = db.transaction().expect("fixture transaction");
            for n in 0..=MIGRATION_BATCH_ROWS {
                insert(&setup, n, Some(VoiceStopOutcome::Unknown));
            }
            setup.commit().expect("fixture commit");
        }
        assert_eq!(batch(&mut db).expect("first batch").cursor, 500);
        {
            let tx = db.transaction().expect("tx");
            let cp = MigrationCheckpoint::load(&tx, VoiceMigration.id(), SOURCES[0])
                .expect("load")
                .expect("checkpoint");
            VoiceMigration
                .move_batch(&tx, SOURCES[0], &cp)
                .expect("uncommitted move");
        }
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        assert_eq!(batch(&mut db).expect("resume").cursor, 501);
        db.execute("INSERT INTO runtime_operation_receipts VALUES('agent-delete-voice:bad:1','unknown-signature','{}')",[]).expect("unclassifiable");
        db.execute(
            "DELETE FROM runtime_payload_migrations WHERE name!='receipts:cutover'",
            [],
        )
        .expect("fresh test checkpoint");
        assert!(
            matches!(batch(&mut db),Err(ReceiptMigrationError::Unclassifiable{table,key,signature}) if table==SOURCES[0]&&key=="agent-delete-voice:bad:1"&&signature=="unknown-signature")
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source preserved"),
            1
        );
    }
    #[test]
    fn receipts_voice_lifecycle_captures_authority_and_prior_submitted_is_unsendable() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        db.execute(
            "INSERT INTO runtime_agents VALUES('agent',?1)",
            [json!({"accountKey":"account","threadId":"thread"}).to_string()],
        )
        .expect("agent");
        db.execute(
            "INSERT INTO voice_sessions VALUES('session','agent',1,NULL,'account','thread','1')",
            [],
        )
        .expect("session");
        db.execute("INSERT INTO runtime_operation_receipts VALUES('agent-voice-stop:agent:2','lifecycle.voice-stop.v1',?1)",[json!({"agent":"agent","epoch":2,"status":"pending"}).to_string()]).expect("lifecycle");
        let operation = "voice:stop:session";
        let prefix = format!(
            "account-worker:retained:account:{}",
            crate::sha256_hex(operation.as_bytes())
        );
        let call = json!({"operation":operation,"method":"thread/realtime/stop","actor":{"agent":"agent","epoch":1},"scope":{"account":"account","connection":"1","generation":1},"params":{"threadId":"thread"}});
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            params![
                format!("{prefix}:submitted"),
                json!({"call":call}).to_string()
            ],
        )
        .expect("submitted");
        batch(&mut db).expect("migrate");
        let row: OutboxRow<VoiceStopPayload> = Outbox::get(&db, "agent-voice-stop:agent:2")
            .expect("read")
            .expect("obligation");
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(
            row.payload.voice_end_results["session"],
            VoiceStopOutcome::Submitted
        );
        let token = row.payload.voice_end_sessions[0]
            .admitted_request
            .as_ref()
            .expect("prior admission");
        assert_eq!(token.id(), format!("{prefix}:result"));
        assert_eq!(token.created(), 1000);
        assert!(
            Outbox::pending::<Value>(&db, OutboxKind::VoiceStop, 2000, 500)
                .expect("selector")
                .is_empty()
        );
    }
    #[test]
    fn receipts_voice_delete_priority_and_new_reservation_keep_final_tokens() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        insert(&db, 0, None);
        insert(&db, 1, None);
        let operation = "voice:delete-stop:session-0";
        let key = format!("account-worker:{}", transport_key("account", operation));
        let call: Call = Call {
            root_request_id: String::new(),
            admitted_request: None,
            scope: ConnectionScope {
                account: "account".into(),
                connection: "1".into(),
                generation: 1,
            },
            operation: operation.into(),
            actor: Some(RequestActor {
                agent: "agent-0".into(),
                epoch: 1,
            }),
            transfer: None,
            deleted_actor_session: Some("session-0".into()),
            method: Method::ThreadRealtimeStop,
            params: json!({"threadId":"thread"}),
        };
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            params![format!("{key}:submitted"), json!({"call":call}).to_string()],
        )
        .expect("delete admission");
        batch(&mut db).expect("migrate");
        let old: OutboxRow<VoiceStopPayload> = Outbox::get(&db, "agent-delete-voice:agent-0:1")
            .expect("read")
            .expect("old obligation");
        assert_eq!(old.status, OutboxStatus::Unknown);
        assert_eq!(
            old.payload.voice_end_sessions[0]
                .admitted_request
                .as_ref()
                .expect("original token")
                .id(),
            format!("{key}:result")
        );
        assert!(old.payload.voice_end_held_uncertain.is_empty());
        let fresh: OutboxRow<VoiceStopPayload> = Outbox::get(&db, "agent-delete-voice:agent-1:1")
            .expect("read")
            .expect("pending");
        let token = fresh.payload.voice_end_sessions[0]
            .admitted_request
            .as_ref()
            .expect("reserved");
        let provider: OutboxRow<Value> = Outbox::get(
            &db,
            &transport_key("account", "voice:delete-stop:session-1"),
        )
        .expect("read")
        .expect("provider reserve");
        assert_eq!(provider.request_id.as_deref(), Some(token.id()));
        assert_eq!(provider.payload["payload"]["admitted"], json!(token));
        assert!(provider.payload["payload"]["request"].is_null());
        assert_eq!(
            provider.payload["payload"]["call"]["root_request_id"],
            fresh.payload.root_request_id
        );
    }
}
