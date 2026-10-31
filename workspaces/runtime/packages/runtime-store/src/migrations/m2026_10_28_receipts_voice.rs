//! Capture voice-stop authority before removing old lifecycle receipt rows.
use crate::outbox::{Outbox, OutboxRow};
use crate::receipt_migration_contract::{
    BatchBudget, MigrationCheckpoint, ReceiptFamilyMigration, ReceiptMigrationError, Result,
    SourceReceipt, admitted_owner_token, delete_source, import_outbox, read_receipt_batch,
    source_exists, unclassifiable,
};
use crate::request_results::AdmittedRequest;
use crate::{VoiceEffect, VoiceEffectAuthority, VoiceEffectDisposition};
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
const RECEIPT_SOURCE: &str = "runtime_operation_receipts";
pub(super) const SOURCES: &[&str] = &["voice_sessions", RECEIPT_SOURCE];
const VOICE: &str = "source.id GLOB 'agent-voice-stop:*' OR source.id GLOB 'agent-delete-voice:*'";

fn migrate_session_scopes(
    tx: &Transaction<'_>,
    checkpoint: &MigrationCheckpoint,
) -> Result<MigrationCheckpoint> {
    let mut next = checkpoint.clone();
    let mut query = tx.prepare("SELECT rowid, length(CAST(id AS BLOB)) + length(CAST(agent AS BLOB)) + COALESCE(length(CAST(account_key AS BLOB)),0) + COALESCE(length(CAST(connection_id AS BLOB)),0) FROM voice_sessions WHERE rowid>?1 AND ended IS NULL AND state IS NOT NULL ORDER BY rowid LIMIT ?2")?;
    let rows = query
        .query_map(
            params![
                checkpoint.cursor,
                crate::receipt_migration_contract::MIGRATION_BATCH_ROWS as i64
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(query);
    let mut budget = BatchBudget::default();
    for (rowid, bytes) in rows {
        if !budget.include(usize::try_from(bytes).unwrap_or(usize::MAX)) {
            break;
        }
        // A matching current connection does not prove a legacy generation.
        // An exact retained call may establish the scope in the receipt phase.
        tx.execute(
            "UPDATE voice_sessions SET state='unknown' WHERE rowid=?1",
            [rowid],
        )?;
        next.cursor = rowid;
    }
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM voice_sessions WHERE rowid>?1 AND ended IS NULL AND state IS NOT NULL)",
        [next.cursor],
        |row| row.get::<_, bool>(0),
    )? {
        next.status = MigrationStatus::Complete;
    }
    Ok(next)
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct VoiceDeleteSession {
    session_id: String,
    agent_id: String,
    epoch: i64,
    account_key: String,
    thread_id: String,
    connection_id: String,
    #[serde(default)]
    generation: Option<u64>,
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

#[derive(Deserialize)]
struct LegacySubmission {
    call: LegacyCall,
}
#[derive(Deserialize)]
struct LegacyCall {
    scope: ConnectionScope,
    operation: String,
    actor: Option<RequestActor>,
    transfer: Option<studio_runtime_schema::provider_call_contract::TransferAuthorization>,
    deleted_actor_session: Option<String>,
    method: Method,
    params: Value,
}
#[derive(Deserialize)]
struct LegacySettled {
    result: RequestResult,
}
#[derive(Deserialize)]
struct LegacyStop {
    agent: String,
    epoch: i64,
    status: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyVoiceAgent {
    account_key: Option<String>,
    thread_id: Option<String>,
}

fn prior_call(
    tx: &Transaction<'_>,
    session: &VoiceDeleteSession,
    operation: &str,
) -> Result<Option<(String, String, Call)>> {
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
    let invalid = || unclassifiable(RECEIPT_SOURCE, &id, &signature);
    let saved: LegacySubmission = serde_json::from_str(&raw).map_err(|_| invalid())?;
    let old = saved.call;
    let call = Call {
        root_request_id: String::new(),
        admitted_request: None,
        scope: old.scope,
        operation: old.operation,
        actor: old.actor,
        transfer: old.transfer,
        deleted_actor_session: old.deleted_actor_session,
        method: old.method,
        params: old.params,
    };
    let actor = call.actor.as_ref().ok_or_else(invalid)?;
    if call.operation != operation
        || call.method != Method::ThreadRealtimeStop
        || actor.agent != session.agent_id
        || actor.epoch > session.epoch
        || call.scope.account != session.account_key
        || call.params["threadId"] != session.thread_id
    {
        return Err(invalid());
    }
    Ok(Some((id, signature, call)))
}

fn prior(
    tx: &Transaction<'_>,
    session: &mut VoiceDeleteSession,
    operation: &str,
    cutover: i64,
) -> Result<Option<VoiceStopOutcome>> {
    let Some((id, signature, call)) = prior_call(tx, session, operation)? else {
        return Ok(None);
    };
    let invalid = || unclassifiable(RECEIPT_SOURCE, &id, &signature);
    let prefix = format!(
        "account-worker:{}",
        transport_key(&session.account_key, operation)
    );
    let actor = call.actor.as_ref().ok_or_else(invalid)?;
    if call.deleted_actor_session.as_deref()
        != operation
            .starts_with("voice:delete-stop:")
            .then_some(session.session_id.as_str())
    {
        return Err(invalid());
    }
    session.epoch = actor.epoch;
    session.generation = Some(call.scope.generation);
    if source_exists(tx, "voice_sessions")? {
        let scope = studio_runtime_schema::persistence::ProviderScope {
            generation: call.scope.generation,
            connection: call.scope.connection.clone(),
        };
        let encoded = serde_json::to_string(&scope).map_err(|_| invalid())?;
        tx.execute(
            "UPDATE voice_sessions SET provider_scope=?1 WHERE id=?2 AND agent=?3 AND account_key=?4 AND native_thread=?5 AND connection_id=?6",
            params![encoded, session.session_id, session.agent_id, session.account_key, session.thread_id, session.connection_id],
        )?;
    }
    let _root = crate::receipt_migration_contract::hydrate_source_root(
        tx,
        RECEIPT_SOURCE,
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
    let settled: LegacySettled = serde_json::from_str(&settled).map_err(|_| invalid())?;
    Ok(Some(match settled.result {
        RequestResult::Reply(_) => VoiceStopOutcome::Written,
        RequestResult::Rejected(_) => VoiceStopOutcome::Rejected,
        RequestResult::NotSent => VoiceStopOutcome::NotSent,
        RequestResult::Unknown => VoiceStopOutcome::Unknown,
    }))
}

fn conflicting_scopes(
    tx: &Transaction<'_>,
    session: &VoiceDeleteSession,
    saved: Option<&studio_runtime_schema::persistence::ProviderScope>,
) -> Result<bool> {
    let mut scope = saved.map(|saved| ConnectionScope {
        account: session.account_key.clone(),
        connection: saved.connection.clone(),
        generation: saved.generation,
    });
    if scope
        .as_ref()
        .is_some_and(|scope| scope.connection != session.connection_id)
    {
        return Ok(true);
    }
    for operation in [
        format!("voice:stop:{}", session.session_id),
        format!("voice:delete-stop:{}", session.session_id),
    ] {
        if let Some((_, _, call)) = prior_call(tx, session, &operation)? {
            if call.scope.connection != session.connection_id
                || scope.as_ref().is_some_and(|old| old != &call.scope)
            {
                return Ok(true);
            }
            scope = Some(call.scope);
        }
    }
    Ok(false)
}
fn reserve(
    tx: &Transaction<'_>,
    owner: &AdmittedRequest,
    root: &str,
    session: &mut VoiceDeleteSession,
    deleted: bool,
) -> Result<()> {
    let generation = session
        .generation
        .ok_or_else(|| unclassifiable(RECEIPT_SOURCE, owner.id(), owner.signature()))?;
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
            generation,
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
                i64::try_from(generation)
                    .map_err(|_| unclassifiable(RECEIPT_SOURCE, owner.id(), owner.signature()))?,
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

fn proven_session_scope(
    tx: &Transaction<'_>,
    session: &VoiceDeleteSession,
) -> Result<Option<studio_runtime_schema::persistence::ProviderScope>> {
    if !source_exists(tx, "voice_sessions")? {
        return Ok(None);
    }
    let raw: Option<String> = tx.query_row(
        "SELECT provider_scope FROM voice_sessions WHERE id=?1 AND agent=?2 AND account_key=?3 AND connection_id=?4",
        params![session.session_id,session.agent_id,session.account_key,session.connection_id],
        |row| row.get(0),
    ).optional()?.flatten();
    raw.map(|raw| {
        serde_json::from_str(&raw)
            .map_err(|_| unclassifiable("voice_sessions", &session.session_id, "provider_scope"))
    })
    .transpose()
}
fn capture(
    tx: &Transaction<'_>,
    row: &SourceReceipt,
    value: &LegacyStop,
) -> Result<VoiceStopPayload> {
    let invalid = || unclassifiable(RECEIPT_SOURCE, &row.id, &row.signature);
    let agent = (!value.agent.is_empty())
        .then_some(value.agent.as_str())
        .ok_or_else(invalid)?;
    let epoch = value.epoch;
    if value.status != "pending" && value.status != "unknown" {
        return Err(invalid());
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [agent],
            |r| r.get(0),
        )
        .optional()?;
    let owner: LegacyVoiceAgent =
        serde_json::from_str(&raw.ok_or_else(invalid)?).map_err(|_| invalid())?;
    let account = owner.account_key.as_deref().unwrap_or("default");
    let thread = owner.thread_id.as_deref();
    let mut sessions = Vec::new();
    if let Some(thread) = thread {
        let mut query=tx.prepare("SELECT id,connection_id,provider_scope FROM voice_sessions WHERE agent=?1 AND ended IS NULL AND state IS NOT NULL AND account_key=?2 AND native_thread=?3 AND connection_id IS NOT NULL ORDER BY created,id")?;
        for result in query.query_map(params![agent, account, thread], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })? {
            let (session_id, connection_id, raw_scope) = result?;
            let scope = raw_scope
                .map(|raw| {
                    serde_json::from_str::<studio_runtime_schema::persistence::ProviderScope>(&raw)
                        .map_err(|_| invalid())
                })
                .transpose()?;
            if scope
                .as_ref()
                .is_some_and(|scope| scope.connection != connection_id)
            {
                return Err(invalid());
            }
            let generation = scope.map(|scope| scope.generation);
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
    let outcomes = if value.status == "unknown" {
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
    let invalid = || unclassifiable(RECEIPT_SOURCE, &row.id, &row.signature);
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
        // The legacy generation was parsed from the connection string and is
        // not authority. Recover only from the exact retained call or scope.
        session.generation = None;
        session.admitted_request = None;
        let old = payload.voice_end_results.get(&session.session_id).copied();
        if !held
            && matches!(
                old,
                Some(VoiceStopOutcome::Written | VoiceStopOutcome::Rejected)
            )
        {
            continue;
        }
        let saved_scope = proven_session_scope(tx, &session)?;
        if conflicting_scopes(tx, &session, saved_scope.as_ref())? {
            return Err(unclassifiable(
                "voice_sessions",
                &session.session_id,
                "conflicting provider scopes",
            ));
        }
        session.generation = saved_scope.map(|scope| scope.generation);
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
        if session.generation.is_none() {
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
    fn prepare(&self, tx: &Transaction<'_>, _cutover_ms: i64) -> Result<()> {
        if source_exists(tx, "voice_sessions")?
            && !tx.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('voice_sessions') WHERE name='provider_scope')", [], |row| row.get::<_, bool>(0))?
        {
            tx.execute_batch("ALTER TABLE voice_sessions ADD COLUMN provider_scope TEXT")?;
        }
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
        if source == "voice_sessions" {
            return if source_exists(tx, source)? {
                migrate_session_scopes(tx, checkpoint)
            } else {
                next.status = MigrationStatus::Complete;
                Ok(next)
            };
        }
        if source != RECEIPT_SOURCE {
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
            let (kind, mut payload) = if row.id.starts_with("agent-delete-voice:")
                && row.signature == "voice-agent-delete-v1"
            {
                (
                    OutboxKind::VoiceDelete,
                    row.json::<VoiceStopPayload>(source)?,
                )
            } else if row.id.starts_with("agent-voice-stop:")
                && row.signature == "lifecycle.voice-stop.v1"
            {
                (
                    OutboxKind::VoiceStop,
                    capture(tx, &row, &row.json::<LegacyStop>(source)?)?,
                )
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
            let owner = payload.admission.as_ref().ok_or_else(invalid)?;
            for (session, held) in payload
                .voice_end_sessions
                .into_iter()
                .map(|session| (session, false))
                .chain(
                    payload
                        .voice_end_held_uncertain
                        .into_iter()
                        .map(|session| (session, true)),
                )
            {
                let effect_kind = if held { OutboxKind::VoiceStop } else { kind };
                let scope = session.generation.map(|generation| {
                    studio_runtime_schema::persistence::ProviderScope {
                        generation,
                        connection: session.connection_id.clone(),
                    }
                });
                let outcome = payload
                    .voice_end_results
                    .get(&session.session_id)
                    .copied()
                    .unwrap_or(if held {
                        VoiceStopOutcome::Unknown
                    } else {
                        VoiceStopOutcome::NotSent
                    });
                let authority = match (scope, session.admitted_request) {
                    (Some(scope), Some(admission)) => VoiceEffectAuthority::Admitted {
                        scope,
                        root_request_id: payload.root_request_id.clone(),
                        owner: owner.clone(),
                        admission,
                        outcome,
                    },
                    (scope, _) => VoiceEffectAuthority::Held {
                        scope: scope.map_or(
                            studio_runtime_schema::persistence::CapturedScope::Unproven,
                            studio_runtime_schema::persistence::CapturedScope::Known,
                        ),
                    },
                };
                let pending = matches!(
                    authority,
                    VoiceEffectAuthority::Admitted {
                        outcome: VoiceStopOutcome::NotSent,
                        ..
                    }
                ) && !held;
                let request_id = match &authority {
                    VoiceEffectAuthority::Admitted { admission, .. } => {
                        Some(admission.id().to_owned())
                    }
                    VoiceEffectAuthority::Held { .. } => None,
                };
                let effect_id = Outbox::effect_id(
                    &row.id,
                    effect_kind,
                    OwnerKind::Agent,
                    &payload.agent,
                    &[&session.session_id],
                );
                let effect_signature = if effect_kind == kind {
                    row.signature.clone()
                } else {
                    "lifecycle.voice-stop.v1".into()
                };
                import_outbox(
                    tx,
                    &OutboxRow {
                        id: effect_id,
                        kind: effect_kind,
                        owner_kind: OwnerKind::Agent,
                        owner_id: payload.agent.clone(),
                        request_id,
                        signature: effect_signature,
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
                        payload: VoiceEffect {
                            session_id: session.session_id,
                            agent: session.agent_id,
                            epoch: session.epoch,
                            account: session.account_key,
                            thread: session.thread_id,
                            disposition: if kind == OutboxKind::VoiceDelete {
                                VoiceEffectDisposition::Deleted {
                                    epoch: payload.epoch,
                                }
                            } else {
                                VoiceEffectDisposition::Active
                            },
                            authority,
                        },
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
        if source_exists(tx, RECEIPT_SOURCE)?
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
                source_table: RECEIPT_SOURCE.into(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[derive(serde::Serialize)]
    struct SubmissionFixture<'a, T> {
        call: &'a T,
    }
    use super::*;
    use crate::outbox::Outbox;
    use crate::receipt_migration_contract::{MIGRATION_BATCH_ROWS, prepare_schema};
    use rusqlite::Connection;

    #[test]
    fn voice_session_scope_batches_bound_large_rows() {
        let mut db = Connection::open_in_memory().unwrap();
        fixture(&db);
        db.execute_batch("ALTER TABLE voice_sessions ADD COLUMN state TEXT;")
            .unwrap();
        let size = crate::receipt_migration_contract::MIGRATION_BATCH_BYTES;
        let large = "x".repeat(size / 2);
        let oversized = "z".repeat(size + 1);
        for id in [format!("a{large}"), format!("b{large}"), oversized] {
            db.execute(
                "INSERT INTO voice_sessions(id,agent,created,state) VALUES(?1,'agent',1,'ready')",
                [id],
            )
            .unwrap();
        }
        let tx = db.transaction().unwrap();
        prepare_schema(&tx).unwrap();
        VoiceMigration.prepare(&tx, 1000).unwrap();
        let mut cp =
            MigrationCheckpoint::begin(&tx, VoiceMigration.id(), "voice_sessions", 1000).unwrap();
        for rowid in 1..=3 {
            cp = VoiceMigration
                .move_batch(&tx, "voice_sessions", &cp)
                .unwrap();
            assert_eq!(cp.cursor, rowid);
        }
        assert_eq!(cp.status, MigrationStatus::Complete);
    }
    #[test]
    fn voice_session_scope_migration_holds_even_a_matching_current_connection() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        db.execute_batch(&crate::agent_volatile::TABLE_SQL).unwrap();
        db.execute_batch("ALTER TABLE voice_sessions ADD COLUMN state TEXT;")
            .unwrap();
        let tx = db.transaction().unwrap();
        prepare_schema(&tx).unwrap();
        VoiceMigration.prepare(&tx, 1000).unwrap();
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('agent',?1)", [json!({"compactions":0,"id":"agent","rootId":"agent","epoch":1,"status":"idle","provider":"codex","isLead":true,"accountKey":"account","executionMode":"host"}).to_string()]).unwrap();
        crate::AgentRepository::get(&tx, &crate::AgentId::from("agent"))
            .expect("typed agent")
            .expect("agent exists");
        tx.execute("INSERT INTO runtime_account_state(account,next_generation,connection_generation,connection_id,journal_ack,recovery_required,streams) VALUES('account',43,42,'current',0,0,'null')", []).unwrap();
        tx.execute("INSERT INTO voice_sessions(id,agent,created,account_key,native_thread,connection_id,state) VALUES('current','agent',1,'account','thread','current','ready'),('old','agent',2,'account','thread','old','ready'),('empty','agent',3,'account','thread','old',NULL)", []).unwrap();
        let checkpoint =
            MigrationCheckpoint::begin(&tx, VoiceMigration.id(), "voice_sessions", 1000).unwrap();
        let next = VoiceMigration
            .move_batch(&tx, "voice_sessions", &checkpoint)
            .unwrap();
        assert_eq!(next.status, MigrationStatus::Complete);
        let (scope, state): (Option<String>, String) = tx
            .query_row(
                "SELECT provider_scope,state FROM voice_sessions WHERE id='current'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(scope.is_none());
        assert_eq!(state, "unknown");
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM voice_sessions WHERE id='empty'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(state.is_none());
        let (scope, state): (Option<String>, String) = tx
            .query_row(
                "SELECT provider_scope,state FROM voice_sessions WHERE id='old'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(scope.is_none());
        assert_eq!(state, "unknown");
    }
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
                connection_id: "legacy-connection".into(),
                generation: Some(1),
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
        VoiceMigration.prepare(&tx, 1000)?;
        let cp = MigrationCheckpoint::begin(&tx, VoiceMigration.id(), RECEIPT_SOURCE, 1000)?;
        let next = VoiceMigration.move_batch(&tx, RECEIPT_SOURCE, &cp)?;
        next.save(&tx, VoiceMigration.id(), RECEIPT_SOURCE, 1000)?;
        tx.commit()?;
        Ok(next)
    }
    #[test]
    fn voice_migration_splits_each_session_into_its_own_effect() {
        let mut db = Connection::open_in_memory().unwrap();
        fixture(&db);
        let mut legacy = payload(0, Some(VoiceStopOutcome::Unknown));
        let mut second = legacy.voice_end_sessions[0].clone();
        second.session_id = "session-second".into();
        legacy.voice_end_sessions.push(second);
        db.execute("INSERT INTO runtime_operation_receipts VALUES('agent-delete-voice:agent-0:1','voice-agent-delete-v1',?1)", [serde_json::to_string(&legacy).unwrap()]).unwrap();
        batch(&mut db).unwrap();
        let rows: i64 = db.query_row("SELECT count(*) FROM runtime_outbox WHERE kind='voice_delete' AND owner_id='agent-0'", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 2, "each captured session has an independent effect");
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
        let row = Outbox::for_owner::<VoiceEffect>(
            &db,
            OwnerKind::Agent,
            "agent-0",
            OutboxKind::VoiceDelete,
        )
        .expect("read")
        .remove(0);
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(row.next_at, None);
        assert!(matches!(
            row.payload.authority,
            VoiceEffectAuthority::Held {
                scope: studio_runtime_schema::persistence::CapturedScope::Unproven
            }
        ));
        let expected = row.payload;
        drop(db);
        let mut db = Connection::open(path).expect("reopen");
        batch(&mut db).expect("second open");
        let row = Outbox::for_owner::<VoiceEffect>(
            &db,
            OwnerKind::Agent,
            "agent-0",
            OutboxKind::VoiceDelete,
        )
        .expect("read")
        .remove(0);
        assert_eq!(row.payload, expected);
        assert!(
            Outbox::for_owner::<VoiceEffect>(
                &db,
                OwnerKind::Agent,
                "agent-1",
                OutboxKind::VoiceDelete
            )
            .expect("read")
            .is_empty()
        );
        assert_eq!(
            Outbox::pending::<Value>(&db, OutboxKind::VoiceDelete, 2000, 500)
                .expect("safe pending")
                .len(),
            0
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
            let cp = MigrationCheckpoint::load(&tx, VoiceMigration.id(), RECEIPT_SOURCE)
                .expect("load")
                .expect("checkpoint");
            VoiceMigration
                .move_batch(&tx, RECEIPT_SOURCE, &cp)
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
            matches!(batch(&mut db),Err(ReceiptMigrationError::Unclassifiable{table,key,signature}) if table==RECEIPT_SOURCE&&key=="agent-delete-voice:bad:1"&&signature=="unknown-signature")
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .expect("source preserved"),
            1
        );
    }
    #[test]
    fn lifecycle_capture_ignores_null_state_sessions() {
        let mut db = Connection::open_in_memory().unwrap();
        fixture(&db);
        db.execute_batch("ALTER TABLE voice_sessions ADD COLUMN state TEXT;")
            .unwrap();
        db.execute(
            "INSERT INTO runtime_agents VALUES('agent',?1)",
            [json!({"accountKey":"account","threadId":"thread"}).to_string()],
        )
        .unwrap();
        db.execute("INSERT INTO voice_sessions(id,agent,created,account_key,native_thread,connection_id,state) VALUES('empty','agent',1,'account','thread','connection',NULL)", []).unwrap();
        db.execute("INSERT INTO runtime_operation_receipts VALUES('agent-voice-stop:agent:1','lifecycle.voice-stop.v1',?1)", [json!({"agent":"agent","epoch":1,"status":"pending"}).to_string()]).unwrap();
        batch(&mut db).unwrap();
        assert!(
            Outbox::for_owner::<VoiceEffect>(&db, OwnerKind::Agent, "agent", OutboxKind::VoiceStop)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn receipts_voice_lifecycle_captures_authority_and_prior_submitted_is_unsendable() {
        let mut db = Connection::open_in_memory().expect("database");
        fixture(&db);
        db.execute_batch("ALTER TABLE voice_sessions ADD COLUMN state TEXT;")
            .unwrap();
        db.execute(
            "INSERT INTO runtime_agents VALUES('agent',?1)",
            [json!({"accountKey":"account","threadId":"thread"}).to_string()],
        )
        .expect("agent");
        db.execute(
            "INSERT INTO voice_sessions VALUES('session','agent',1,NULL,'account','thread','legacy-connection','ready')",
            [],
        )
        .expect("session");
        db.execute("INSERT INTO runtime_operation_receipts VALUES('agent-voice-stop:agent:2','lifecycle.voice-stop.v1',?1)",[json!({"agent":"agent","epoch":2,"status":"pending"}).to_string()]).expect("lifecycle");
        let operation = "voice:stop:session";
        let prefix = format!(
            "account-worker:retained:account:{}",
            crate::sha256_hex(operation.as_bytes())
        );
        let call = json!({"operation":operation,"method":"thread/realtime/stop","actor":{"agent":"agent","epoch":1},"scope":{"account":"account","connection":"legacy-connection","generation":1},"params":{"threadId":"thread"}});
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            params![
                format!("{prefix}:submitted"),
                serde_json::to_string(&SubmissionFixture { call: &call }).expect("call")
            ],
        )
        .expect("submitted");
        batch(&mut db).expect("migrate");
        let row =
            Outbox::for_owner::<VoiceEffect>(&db, OwnerKind::Agent, "agent", OutboxKind::VoiceStop)
                .expect("read")
                .remove(0);
        assert_eq!(row.status, OutboxStatus::Unknown);
        let VoiceEffectAuthority::Admitted {
            admission: token,
            outcome: VoiceStopOutcome::Submitted,
            ..
        } = &row.payload.authority
        else {
            panic!("prior admission")
        };
        assert_eq!(token.id(), format!("{prefix}:result"));
        assert_eq!(token.created(), 1000);
        let saved_scope: String = db
            .query_row(
                "SELECT provider_scope FROM voice_sessions WHERE id='session'",
                [],
                |row| row.get(0),
            )
            .expect("proven scope");
        assert_eq!(
            serde_json::from_str::<studio_runtime_schema::persistence::ProviderScope>(&saved_scope)
                .expect("scope"),
            studio_runtime_schema::persistence::ProviderScope {
                generation: 1,
                connection: "legacy-connection".into()
            }
        );
        assert!(
            Outbox::pending::<Value>(&db, OutboxKind::VoiceStop, 2000, 500)
                .expect("selector")
                .is_empty()
        );
    }
    #[test]
    fn conflicting_retained_scopes_refuse_two_owners_after_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scope.db");
        let mut db = Connection::open(&path).expect("database");
        fixture(&db);
        insert(&db, 0, None);
        db.execute("INSERT INTO runtime_operation_receipts VALUES('agent-delete-voice:agent-0:2','voice-agent-delete-v1',?1)", [serde_json::to_string(&payload(0, None)).unwrap()]).unwrap();
        for (operation, generation, deleted) in [
            ("voice:stop:session-0", 1, None),
            ("voice:delete-stop:session-0", 2, Some("session-0")),
        ] {
            let call: Call = Call {
                root_request_id: String::new(),
                admitted_request: None,
                scope: ConnectionScope {
                    account: "account".into(),
                    connection: "legacy-connection".into(),
                    generation,
                },
                operation: operation.into(),
                actor: Some(RequestActor {
                    agent: "agent-0".into(),
                    epoch: 1,
                }),
                transfer: None,
                deleted_actor_session: deleted.map(str::to_owned),
                method: Method::ThreadRealtimeStop,
                params: json!({"threadId":"thread"}),
            };
            let key = format!(
                "account-worker:{}:submitted",
                transport_key("account", operation)
            );
            db.execute(
                "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
                params![
                    key,
                    serde_json::to_string(&SubmissionFixture { call: &call }).expect("call")
                ],
            )
            .expect("call");
        }
        assert!(
            matches!(batch(&mut db), Err(ReceiptMigrationError::Unclassifiable { key, .. }) if key == "session-0")
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
        drop(db);
        let mut db = Connection::open(path).unwrap();
        assert!(
            matches!(batch(&mut db), Err(ReceiptMigrationError::Unclassifiable { key, .. }) if key == "session-0")
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4
        );
    }
    #[test]
    fn retained_scope_conflicting_with_stored_scope_is_refused() {
        let mut db = Connection::open_in_memory().unwrap();
        fixture(&db);
        db.execute_batch("ALTER TABLE voice_sessions ADD COLUMN provider_scope TEXT;")
            .unwrap();
        db.execute("INSERT INTO voice_sessions(id,agent,created,account_key,native_thread,connection_id,provider_scope) VALUES('session-0','agent-0',1,'account','thread','legacy-connection',?1)", [json!({"generation":2,"connection":"legacy-connection"}).to_string()]).unwrap();
        insert(&db, 0, None);
        let operation = "voice:delete-stop:session-0";
        let call: Call = Call {
            root_request_id: String::new(),
            admitted_request: None,
            scope: ConnectionScope {
                account: "account".into(),
                connection: "legacy-connection".into(),
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
        let key = format!(
            "account-worker:{}:submitted",
            transport_key("account", operation)
        );
        db.execute(
            "INSERT INTO runtime_operation_receipts VALUES(?1,'submitted',?2)",
            params![
                key,
                serde_json::to_string(&SubmissionFixture { call: &call }).expect("call")
            ],
        )
        .unwrap();
        assert!(
            matches!(batch(&mut db), Err(ReceiptMigrationError::Unclassifiable { key, .. }) if key == "session-0")
        );
        let scope: String = db
            .query_row(
                "SELECT provider_scope FROM voice_sessions WHERE id='session-0'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&scope).unwrap(),
            json!({"generation":2,"connection":"legacy-connection"})
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM runtime_operation_receipts", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }
    #[test]
    fn receipts_voice_delete_priority_holds_unproven_sessions() {
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
                connection: "legacy-connection".into(),
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
            params![
                format!("{key}:submitted"),
                serde_json::to_string(&SubmissionFixture { call: &call }).expect("call")
            ],
        )
        .expect("delete admission");
        batch(&mut db).expect("migrate");
        let old = Outbox::for_owner::<VoiceEffect>(
            &db,
            OwnerKind::Agent,
            "agent-0",
            OutboxKind::VoiceDelete,
        )
        .expect("read")
        .remove(0);
        assert_eq!(old.status, OutboxStatus::Unknown);
        let VoiceEffectAuthority::Admitted {
            admission: old_token,
            ..
        } = &old.payload.authority
        else {
            panic!("original token")
        };
        assert_eq!(old_token.id(), format!("{key}:result"));
        let fresh = Outbox::for_owner::<VoiceEffect>(
            &db,
            OwnerKind::Agent,
            "agent-1",
            OutboxKind::VoiceDelete,
        )
        .expect("read")
        .remove(0);
        assert_eq!(fresh.status, OutboxStatus::Unknown);
        assert!(fresh.next_at.is_none());
        assert!(matches!(
            fresh.payload.authority,
            VoiceEffectAuthority::Held {
                scope: studio_runtime_schema::persistence::CapturedScope::Unproven
            }
        ));
        assert!(
            Outbox::get::<Value>(
                &db,
                &transport_key("account", "voice:delete-stop:session-1")
            )
            .expect("read")
            .is_none()
        );
    }
}
