//! Account database operations. Transaction callbacks stay synchronous and are
//! submitted to RuntimeStore's bounded writer actor.
use super::*;
use crate::notification_dispatch::{
    NotificationEffects, NotificationPublicationBatch, PreparedSubmission,
    ProviderDeliveryIdentity, dispatch_notification_with_effects, flush_notification_streams,
};
use rusqlite::{OptionalExtension, params};
use studio_runtime_store::{EventWriteRepository, NotificationDispatchRepository};

pub(super) const PUBLICATION_DRAIN_LIMIT: usize = 64;
pub(super) const MAX_PENDING_PUBLICATION_RECEIPTS: usize = 256;
pub(super) const MAX_COALESCED_TOKEN_RATE_EFFECTS: usize = 64;
const MAX_COALESCED_TOKEN_RATE_BYTES: usize = 256 * 1024;

pub(super) struct SubmissionEvidence {
    pub turn: Option<String>,
    pub submission: Option<PreparedSubmission>,
    pub guard: Option<preparation::CommitGuard>,
    pub connection_id: String,
}

fn invalid(id: &str) -> WorkerError {
    StoreError::InvalidSyncObject {
        scope: "account_worker".into(),
        id: id.into(),
    }
    .into()
}

const JOURNAL_FRAME_PAYLOAD_PREFIX: &[u8] = b"{\"frame\":";
const JOURNAL_FRAME_PAYLOAD_SUFFIX: &[u8] = b"}";

fn journal_frame_payload(frame: &Frame, operation: &str) -> Result<Vec<u8>, WorkerError> {
    let encoded = studio_runtime_providers::encode_line(frame).map_err(|_| invalid(operation))?;
    let encoded = encoded
        .strip_suffix(b"\n")
        .ok_or_else(|| invalid(operation))?;
    let mut payload = Vec::with_capacity(
        encoded.len() + JOURNAL_FRAME_PAYLOAD_PREFIX.len() + JOURNAL_FRAME_PAYLOAD_SUFFIX.len(),
    );
    payload.extend_from_slice(JOURNAL_FRAME_PAYLOAD_PREFIX);
    payload.extend_from_slice(encoded);
    payload.extend_from_slice(JOURNAL_FRAME_PAYLOAD_SUFFIX);
    Ok(payload)
}

fn receipt_id(operation: &str, phase: &str) -> String {
    format!("account-worker:{operation}:{phase}")
}
pub(super) fn receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    phase: &str,
) -> Result<Option<Value>, WorkerError> {
    EventWriteRepository::receipt(tx, &receipt_id(operation, phase))?
        .map(|r| serde_json::from_str(&r.result).map_err(|_| invalid(operation)))
        .transpose()
}
pub(super) fn save(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    phase: &str,
    value: Value,
) -> Result<(), WorkerError> {
    if let Some(old) = receipt(tx, operation, phase)? {
        return if old == value {
            Ok(())
        } else {
            Err(invalid(operation))
        };
    }
    EventWriteRepository::put_receipt(
        tx,
        &receipt_id(operation, phase),
        phase,
        &value.to_string(),
    )?;
    Ok(())
}
pub(super) fn delete_receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    phase: &str,
) -> Result<(), WorkerError> {
    tx.execute(
        "DELETE FROM runtime_operation_receipts WHERE id=?1",
        [receipt_id(operation, phase)],
    )?;
    Ok(())
}
/// Submitted account operations across all accounts, using the receipt-ID owner.
fn submitted_account_receipt_pattern() -> String {
    receipt_id("account:*", "submitted")
}
/// Unresolved thread start or resume submissions for `agent`, in any account.
/// GLOB lets SQLite derive an indexed prefix range without manual bound arithmetic.
const PENDING_THREAD_START_SQL: &str = "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts r WHERE r.id GLOB ?2 AND json_extract(r.result,'$.context.intent.agent_id')=?1 AND json_extract(r.result,'$.context.method') IN ('thread/start','thread/resume') AND NOT EXISTS(SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':resolved'))";
fn has_pending_thread_start(
    connection: &rusqlite::Connection,
    agent: &str,
) -> rusqlite::Result<bool> {
    connection.query_row(
        PENDING_THREAD_START_SQL,
        params![agent, submitted_account_receipt_pattern()],
        |row| row.get(0),
    )
}
pub(super) fn record(request: &RecoveredRequest) -> Value {
    json!({"id":request.ticket.id,"generation":request.ticket.generation,"method":request.ticket.method.as_str(),"operation":request.operation_id.0,"thread":request.thread_id,"lightweight":request.lightweight,"deadline":request.deadline_ms,"unknown":request.unknown,"remoteId":request.remote_id})
}
pub(super) fn decode(record: &Value) -> Result<RecoveredRequest, WorkerError> {
    let invalid = || invalid("recovery");
    Ok(RecoveredRequest {
        ticket: studio_runtime_providers::RequestTicket {
            id: serde_json::from_value(record["id"].clone()).map_err(|_| invalid())?,
            generation: record["generation"].as_u64().ok_or_else(invalid)?,
            method: record["method"]
                .as_str()
                .and_then(Method::from_wire)
                .ok_or_else(invalid)?,
        },
        operation_id: OperationId(record["operation"].as_str().ok_or_else(invalid)?.into()),
        thread_id: record["thread"].as_str().map(str::to_owned),
        lightweight: record["lightweight"].as_bool().ok_or_else(invalid)?,
        deadline_ms: record["deadline"].as_u64().ok_or_else(invalid)?,
        unknown: true,
        remote_id: record["remoteId"].as_u64(),
    })
}
/// `restart` is true for the account's first worker in this backend process;
/// a later worker follows a provider disconnect while the server kept running.
pub(super) async fn recover(
    store: &RuntimeStore,
    account: &str,
    generation: Generation,
    restart: bool,
) -> Result<Vec<(Pending, RecoveredRequest)>, WorkerError> {
    let account = account.to_owned();
    scheduler::lifecycle_transaction_with(store, move |tx| {
        SchedulerRepository::recover_unsubmitted_for_account(tx, &account)?;
        let at = now();
        let interrupted = SchedulerRepository::reconcile_generation(
            tx,
            &account,
            generation.0,
            at,
            restart,
        )?;
        // A provider replacement in this backend is a disconnect. Actual backend
        // restarts keep their separate history-reconciliation notification path.
        if !restart {
            for agent_id in interrupted {
                crate::lifecycle_effects::notify_parent_of_disconnect(tx, &agent_id, at)?;
            }
        }
        retire_generations(tx, &account, generation.0)?;
        // The repository preserves Python recovery records. Journal delivery needs
        // the scoped attempt restored before notifications can be acknowledged.
        let mut statement = tx.prepare("SELECT a.id FROM runtime_agents a WHERE COALESCE(json_extract(a.record,'$.accountKey'),'default')=?1 AND json_extract(a.record,'$.startAttempt') IS NULL AND COALESCE(json_extract(a.record,'$.disconnectRecovery.startAttempt.connectionGeneration'),json_extract(a.record,'$.restartRecovery.startAttempt.connectionGeneration'))=?2 AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts h WHERE h.id=(?3 || a.id) AND h.signature=?4)")?;
        let ids = statement
            .query_map(
                params![
                    account,
                    i64::try_from(generation.0)
                        .map_err(|_| StoreError::SchemaMismatch("generation overflow".into()))?,
                    studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                    studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            let mut agent =
                AgentRepository::get(tx, &AgentId::from(id.as_str()))?.ok_or_else(|| {
                    StoreError::InvalidSyncObject {
                        scope: "runtime_agents".into(),
                        id: id.clone(),
                    }
                })?;
            let attempt = agent
                .extra
                .get("disconnectRecovery")
                .or_else(|| agent.extra.get("restartRecovery"))
                .and_then(|value| value.get("startAttempt"))
                .cloned();
            if let Some(attempt) = attempt {
                agent.extra.insert("startAttempt".into(), attempt);
                AgentRepository::put(tx, &agent)?;
            }
        }
        let prefix = format!("account-worker:account:{account}:");
        let mut stmt=tx.prepare("SELECT r.result FROM runtime_operation_receipts r WHERE r.id>=?1 AND r.id<?2 AND substr(r.id,-10)=':submitted' AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':resolved') ORDER BY r.id")?;
        let rows = stmt
            .query_map(params![prefix, format!("{prefix}~"),], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut recovered = Vec::new();
        for raw in rows {
            let value: Value = serde_json::from_str(&raw).map_err(|_| invalid(&account))?;
            let context: Pending =
                serde_json::from_value(value["context"].clone()).map_err(|_| invalid(&account))?;
            if Method::from_wire(&context.method).is_none() {
                return Err(invalid(&account));
            }
            let held: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts \
                 WHERE id=?1 AND signature=?2)",
                params![
                    format!(
                        "{}{}",
                        studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX,
                        context.intent.agent_id
                    ),
                    studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE
                ],
                |row| row.get(0),
            )?;
            if held {
                // Keep the unresolved submission and its uncertainty intact,
                // but do not repeatedly decode the quarantined agent row.
                continue;
            }
            let written = receipt(tx, &context.operation, "written")?;
            let request = decode(written.as_ref().unwrap_or(&value["request"]))?;
            let vm_owned =
                AgentRepository::get(tx, &AgentId::from(context.intent.agent_id.as_str()))?
                    .is_some_and(|agent| {
                        agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
                    });
            if vm_owned {
                continue;
            }
            if context.method()? == Method::TurnStart {
                SchedulerRepository::settle_start_outcome(
                    tx,
                    &context.intent.identity(),
                    "uncertain",
                    None,
                )?;
            }
            if matches!(
                context.method()?,
                Method::ThreadStart | Method::ThreadResume
            ) {
                let current =
                    AgentRepository::get(tx, &AgentId::from(context.intent.agent_id.as_str()))?;
                if current.as_ref().is_some_and(|a| {
                    a.epoch == context.intent.epoch
                        && account_key(a) == account
                        && a.extra.get("startAttempt").is_none_or(Value::is_null)
                        && (a.extra.get("autoWake") == Some(&json!(true))
                            || a.extra
                                .get("startOutcomeHold")
                                .is_some_and(|h| h["operationId"] == context.operation))
                }) {
                    hold_thread_receipt(
                        tx,
                        &context,
                        request.ticket.generation,
                        "thread-preparation/receipt-pending",
                    )?;
                }
            }
            // An old generation's ticket cannot be restored into this transport.
            // Its durable receipt and visible Unknown hold remain for reconciliation.
            if request.ticket.generation != generation.0 {
                continue;
            }
            recovered.push((context, request));
        }
        Ok(recovered)
    })
    .await
}

/// Restore outstanding tickets belonging to one VM actor into that actor's
/// guest session. Host tickets are handled by `recover` and never cross this
/// boundary.
pub(super) fn recover_vm_actor_requests(
    connection: &rusqlite::Connection,
    account: &str,
    agent: &str,
    generation: u64,
) -> Result<Vec<(Pending, RecoveredRequest)>, WorkerError> {
    let Some(agent_record) = AgentRepository::get(connection, &AgentId::from(agent))? else {
        return Ok(Vec::new());
    };
    if agent_record
        .extra
        .get("executionMode")
        .and_then(Value::as_str)
        != Some("vm")
    {
        return Ok(Vec::new());
    }
    let prefix = format!("account-worker:account:{account}:");
    let mut statement = connection.prepare(
        "SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 AND substr(id,-10)=':submitted' AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(id,1,length(id)-10)||':resolved') ORDER BY id",
    )?;
    let rows = statement
        .query_map(params![prefix, format!("{prefix}~")], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut recovered = Vec::new();
    for (receipt, raw) in rows {
        let value: Value = serde_json::from_str(&raw).map_err(|_| invalid(&receipt))?;
        let context: Pending =
            serde_json::from_value(value["context"].clone()).map_err(|_| invalid(&receipt))?;
        if context.intent.agent_id != agent {
            continue;
        }
        let operation = receipt
            .strip_prefix("account-worker:")
            .and_then(|value| value.strip_suffix(":submitted"))
            .ok_or_else(|| invalid(&receipt))?;
        let written_id = receipt_id(operation, "written");
        let written = connection
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                [written_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let record = match written {
            Some(raw) => serde_json::from_str(&raw).map_err(|_| invalid(&receipt))?,
            None => value["request"].clone(),
        };
        let request = decode(&record)?;
        if request.ticket.generation == generation {
            recovered.push((context, request));
        }
    }
    Ok(recovered)
}
// Reconciliation has already held old-generation input. Transport receipts
// from a closed generation cannot authorize replay and no longer need journals.
fn retire_generations(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    current: u64,
) -> Result<(), WorkerError> {
    for kind in ["cursor", "journal", "callback", "recovery", "account"] {
        let prefix = format!("account-worker:{kind}:{account}:");
        let mut query =
            tx.prepare("SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2")?;
        let rows = query
            .query_map(params![prefix, format!("{prefix}~")], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, raw) in rows {
            let value: Value = serde_json::from_str(&raw).map_err(|_| invalid(&id))?;
            let generation = if kind == "account" {
                value
                    .pointer("/request/generation")
                    .or_else(|| value.get("generation"))
                    .and_then(Value::as_u64)
            } else {
                id.strip_prefix(&prefix)
                    .and_then(|tail| tail.split(':').next())
                    .and_then(|number| number.parse().ok())
            };
            if generation.is_none_or(|generation| generation >= current) {
                continue;
            }
            if kind == "cursor" {
                // A saved retained result can only be replayed after reconnect
                // while its submitted intent still exists. Keep that
                // generation's checkpoint for exactly that lifetime. Submitted
                // retained intents are durable, so each such generation pins at
                // most one checkpoint; checkpoint count therefore never exceeds
                // the number of retained receipts. Generations without retained
                // requests retire their checkpoint during reconnect churn.
                let retained_prefix = format!("account-worker:retained:{account}:");
                let generation_number = generation
                    .and_then(|generation| i64::try_from(generation).ok())
                    .ok_or_else(|| invalid(&id))?;
                let has_retained_submission: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 AND substr(id,-10)=':submitted' AND json_extract(result,'$.call.scope.generation')=?3)",
                    params![
                        retained_prefix,
                        format!("{retained_prefix}~"),
                        generation_number
                    ],
                    |row| row.get(0),
                )?;
                if has_retained_submission {
                    continue;
                }
            }
            if kind == "account"
                && matches!(
                    value
                        .pointer("/request/method")
                        .or_else(|| value.get("method"))
                        .and_then(Value::as_str),
                    Some("thread/start" | "thread/resume")
                )
            {
                // An unresolved thread request may have applied in the previous
                // child. Generation replacement is not negative delivery proof.
                continue;
            }
            if kind == "account"
                && let Some(context) = value.get("context")
            {
                let pending: Pending =
                    serde_json::from_value(context.clone()).map_err(|_| invalid(&id))?;
                if let Some(key) = effect_key(account, &Effect::Submit(pending.intent)) {
                    delete_effect(tx, &key)?;
                }
                forget_request(tx, &pending.operation)?;
            }
            if kind == "journal"
                && let (Some(generation), Some(cursor)) = (generation, value["cursor"].as_u64())
            {
                let identity = json!({"providerHandle":ProviderHandle::for_account(account).as_str(),"generation":generation,"cursor":cursor});
                tx.execute(
                    "DELETE FROM runtime_operation_receipts WHERE id=?1",
                    [format!(
                        "provider-notification:{}",
                        studio_runtime_store::sha256_hex(identity.to_string().as_bytes())
                    )],
                )?;
            }
            tx.execute("DELETE FROM runtime_operation_receipts WHERE id=?1", [id])?;
        }
    }
    Ok(())
}

pub(super) async fn claim(
    store: Arc<RuntimeStore>,
    account: &str,
    agent: &str,
    epoch: i64,
) -> Result<Option<TurnIntent>, WorkerError> {
    let account = account.to_owned();
    let agent = agent.to_owned();
    let read_store = store.clone();
    let read_agent = agent.clone();
    let (snapshot, prepared) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_claim_read",
        move || {
            let snapshot =
                AgentRepository::get(&*read_store.read()?, &AgentId::from(read_agent.as_str()))?;
            let has_radio_turn = read_store.read()?.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 \
                 AND status='pending' AND kind='radio_turn')",
                rusqlite::params![read_agent.as_str(), epoch],
                |row| row.get::<_, bool>(0),
            )?;
            let prepared = if has_radio_turn {
                let prepared = crate::host_project_runtime::PreparedHost::load(&read_store)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                prepared
                    .recheck_paths()
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                Some(prepared)
            } else {
                None
            };
            Ok::<_, WorkerError>((snapshot, prepared))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    if snapshot.as_ref().is_none_or(|record| {
        record.epoch != epoch
            || account_key(record) != account.as_str()
            || studio_runtime_store::truthy(record.extra.get("moveImportPending"))
    }) {
        return Ok(None);
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::TaskFailed)?;
    let attempt = studio_runtime_store::sha256_hex(&bytes);
    scheduler::lifecycle_transaction_with(
        &store,
        move |tx| -> Result<Option<TurnIntent>, WorkerError> {
            let record = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?;
            if record
                .as_ref()
                .is_none_or(|a| account_key(a) != account.as_str())
            {
                return Ok(None);
            }
            if record
                .as_ref()
                .is_some_and(|a| studio_runtime_store::truthy(a.extra.get("moveImportPending")))
            {
                return Ok(None);
            }
            if record.as_ref().is_some_and(|a| {
                ["claudePreInputRetry", "claudeAuthWait"]
                    .iter()
                    .any(|key| a.extra.get(*key).is_some_and(|v| !v.is_null()))
            }) {
                hold_before_claim(
                    tx,
                    &agent,
                    "preparation/claude-frozen-input-recovery-required",
                )?;
                return Ok(None);
            }
            let preparing = has_pending_thread_start(tx, agent.as_str())?;
            if preparing {
                return Ok(None);
            }
            let pending = record
                .as_ref()
                .and_then(|a| a.extra.get("startAttempt"))
                .filter(|a| {
                    a["submitted"] == false
                        && matches!(
                            a["prepareError"].as_str(),
                            Some("workspace-readiness/pending" | "model-catalog/refresh-pending")
                        )
                });
            let attempt = pending.and_then(|a| a["id"].as_str()).unwrap_or(&attempt);
            if let Some(prepared) = &prepared {
                prepared
                    .recheck(tx)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                let mut statement = tx.prepare(
                "SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events \
                 WHERE agent=?1 AND epoch=?2 AND status='pending' ORDER BY created,rowid",
            )?;
                let events = statement
                    .query_map(rusqlite::params![agent.as_str(), epoch], |row| {
                        Ok(studio_runtime_store::QueueEvent {
                            id: studio_runtime_store::QueueEventId(row.get(0)?),
                            agent: studio_runtime_store::AgentId(row.get(1)?),
                            kind: row.get(2)?,
                            text: row.get(3)?,
                            status: row.get(4)?,
                            created: row.get(5)?,
                            epoch: row.get(6)?,
                            turn_id: row.get(7)?,
                            error: row.get(8)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                let selected = crate::radio_runtime::select_pending(
                    tx,
                    prepared,
                    &record.as_ref().ok_or(WorkerError::Closed)?.value(),
                    &events,
                    now(),
                )
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                if let Some(event) = selected.first().filter(|event| event.kind == "radio_turn") {
                    return scheduler::claim_radio_turn_in_transaction(
                        tx,
                        &agent,
                        epoch,
                        attempt,
                        now(),
                        &event.id.0,
                    )
                    .map_err(Into::into);
                }
            }
            match scheduler::claim_preparation_in_transaction(tx, &agent, epoch, attempt, now()) {
                Ok(intent) => Ok(intent),
                Err(scheduler::SchedulerError::PolicyRefused(reason)) => {
                    hold_before_claim(tx, &agent, &reason)?;
                    Ok(None)
                }
                Err(error) => Err(error.into()),
            }
        },
    )
    .await
}

/// Persist a visible fail-closed state for one malformed agent without decoding
/// it through the typed repository. The original invalid field is retained for
/// repair, while this agent can no longer be selected for provider work.
pub(super) async fn hold_invalid_agent_record(
    store: &RuntimeStore,
    agent_id: &str,
    expected_pointer: &str,
    expected_reason: &str,
    diagnostic: &str,
) -> Result<(), WorkerError> {
    let server_id = store.local_server_id()?;
    let agent_id = agent_id.to_owned();
    let expected_pointer = expected_pointer.to_owned();
    let expected_reason = expected_reason.to_owned();
    let diagnostic = diagnostic.to_owned();
    store
        .transaction(move |tx| {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [agent_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(raw) = raw else {
                return Ok(());
            };
            match AgentRepository::get(tx, &AgentId::from(agent_id.as_str())) {
                Ok(_) => return Ok(()),
                Err(StoreError::InvalidSyncObjectField {
                    scope,
                    id,
                    pointer,
                    reason,
                }) if scope == "runtime_agents"
                    && id == agent_id
                    && pointer == expected_pointer
                    && reason == expected_reason => {}
                Err(StoreError::InvalidSyncObjectField { .. }) => return Ok(()),
                Err(error) => return Err(error),
            }
            let parsed: Value =
                serde_json::from_str(&raw).map_err(|_| StoreError::InvalidSyncObject {
                    scope: "runtime_agents".into(),
                    id: agent_id.clone(),
                })?;
            tx.execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3) \
             ON CONFLICT(id) DO UPDATE SET signature=excluded.signature,result=excluded.result",
                rusqlite::params![
                    format!(
                        "{}{agent_id}",
                        studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX
                    ),
                    studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                    serde_json::to_string(
                        &serde_json::json!({"held": true, "diagnostic": diagnostic})
                    )
                    .map_err(StoreError::from)?
                ],
            )?;

            // Publish a minimal safe agent view so the renderer can show that this
            // row is held while the malformed persisted bytes remain untouched.
            let public = crate::sync_views::invalid_agent_view(&agent_id, &parsed, &diagnostic);
            let context =
                crate::sync_entity_context::agent_view_context(tx, &public, server_id.as_deref())?;
            let projection =
                crate::sync_views::agent_entity_view(&public, &context).ok_or_else(|| {
                    StoreError::InvalidSyncObject {
                        scope: "runtime_agents".into(),
                        id: agent_id.clone(),
                    }
                })?;
            studio_runtime_store::SyncDocumentsRepository::put_entity(
                tx, "agent", &agent_id, projection, false,
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}
fn account_key(agent: &studio_runtime_store::AgentRecord) -> &str {
    agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
}

pub(super) async fn submitted(
    store: &RuntimeStore,
    context: Pending,
    account: &str,
    request: RecoveredRequest,
    evidence: SubmissionEvidence,
) -> Result<bool, WorkerError> {
    let SubmissionEvidence {
        turn,
        submission,
        guard,
        connection_id,
    } = evidence;
    let account = account.to_owned();
    let blocked_intent = context.intent.clone();
    let result = scheduler::lifecycle_transaction_with(store, move |tx| {
        if receipt(tx, &context.operation, "submitted")?.is_some() {
            return Ok(false);
        }
        let i = &context.intent;
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(i.agent_id.as_str()))? else {
            return Ok(false);
        };
        if account_key(&agent) != account.as_str() || i.account_key != account {
            return Ok(false);
        }
        // Retirement is a committed outcome, whereas a stale preparation must
        // roll back its freeze. Reconcile before making any preparation writes.
        if context.method()? == Method::TurnStart
            && guard.is_some()
            && !SchedulerRepository::reconcile_obsolete_before_submit(
                tx,
                &i.agent_id,
                i.epoch,
                &i.attempt_id,
            )?
        {
            return Ok(false);
        }
        if let Some(guard) = guard.as_ref() {
            preparation::commit(tx, &context, guard)?;
        }
        let authorized = match context.method()? {
            Method::TurnStart if i.provider == "claude" => preparation::commit_claude(
                tx,
                i,
                &id_string(&request.ticket.id),
                request.ticket.generation,
                &connection_id,
                submission.as_ref().ok_or_else(|| invalid(&i.attempt_id))?,
                guard
                    .as_ref()
                    .ok_or_else(|| invalid("Claude commit guard"))?,
            )?,
            Method::TurnStart => commit_prepared_turn(
                tx,
                i,
                &id_string(&request.ticket.id),
                request.ticket.generation,
                &connection_id,
                submission.as_ref().ok_or_else(|| invalid(&i.attempt_id))?,
            )?,
            Method::TurnInterrupt => SchedulerRepository::prepare_interrupt(
                tx,
                &i.agent_id,
                i.thread_id.as_deref().unwrap_or(""),
                turn.as_deref().unwrap_or(""),
                request.ticket.generation,
                &id_string(&request.ticket.id),
            )?,
            _ => {
                agent.epoch == i.epoch
                    && agent.extra.get("startAttempt").unwrap_or(&Value::Null)["id"] == i.attempt_id
                    && agent.extra.get("startAttempt").unwrap_or(&Value::Null)["submitted"] == false
                    && agent.extra.get("autoWake").unwrap_or(&Value::Null) == true
                    && agent.extra.get("nativeFailureHold").unwrap_or(&Value::Null) != true
            }
        };
        if !authorized {
            if guard.is_some() {
                return Err(WorkerError::PreparationRefused(
                    "preparation/submission-identity-changed".into(),
                ));
            }
            return Ok(false);
        }
        if let Some(compiled) = &context.compiled {
            preparation::persist_compilation(tx, compiled)?;
        }
        save(
            tx,
            &context.operation,
            "submitted",
            json!({"context":context,"request":record(&request)}),
        )?;
        Ok(true)
    })
    .await;
    match result {
        Err(WorkerError::PreparationRefused(reason)) => {
            scheduler::settle_start_outcome(store, &blocked_intent, TurnStart::Blocked { reason })
                .await?;
            Ok(false)
        }
        result => result,
    }
}
/// `session_closed` is true while the worker settles its ended session's outcomes.
pub(super) async fn outcome(
    store: Arc<RuntimeStore>,
    account: String,
    resources: Arc<preparation::PreparationResources>,
    pending: Option<Pending>,
    outcome: SessionOutcome,
    session_closed: bool,
) -> Result<(bool, Option<Effect>), WorkerError> {
    let Some(context) = pending else {
        if let SessionOutcome::GenerationClosed { generation } = outcome {
            scheduler::lifecycle_transaction(&store, move |tx| {
                disconnect_provider_generation(tx, &account, generation)
            })
            .await?;
        }
        return Ok((false, None));
    };
    let state_root = store.root().to_path_buf();
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        let i = &context.intent;
        if context.method()? == Method::TurnSteer {
            let wake = steer::outcome(tx, &context, &outcome)?;
            if let SessionOutcome::Write {
                outcome: WriteOutcome::Written,
                recovery: Some(request),
                ..
            } = &outcome
            {
                save(tx, &context.operation, "written", record(request))?;
            }
            let retired = matches!(
                outcome,
                SessionOutcome::Resolved { .. }
                    | SessionOutcome::Write {
                        outcome: WriteOutcome::NotSent,
                        ..
                    }
            );
            if retired {
                forget_request(tx, &context.operation)?;
            }
            return Ok((
                retired,
                wake.map(|(agent, epoch)| Effect::Wake(agent, epoch)),
            ));
        }
        let mut effect = None;
        let retired = match outcome {
            SessionOutcome::Write {
                outcome: WriteOutcome::Written,
                recovery: Some(request),
                ..
            } => {
                save(tx, &context.operation, "written", record(&request))?;
                false
            }
            SessionOutcome::Write {
                outcome: WriteOutcome::Written,
                ..
            } => false,
            SessionOutcome::Write {
                outcome, ticket, ..
            } => {
                let not_sent = outcome == WriteOutcome::NotSent;
                uncertain(tx, &context, ticket.as_ref(), not_sent, session_closed)? || not_sent
            }
            SessionOutcome::Resolved {
                outcome: RequestOutcome::Unknown,
                ticket,
            } => uncertain(tx, &context, Some(&ticket), false, session_closed)?,
            SessionOutcome::Resolved {
                ticket,
                outcome: RequestOutcome::Reply { body, .. },
            } => {
                match context.method()? {
                    Method::ThreadResume | Method::ThreadStart => {
                        if let ResponseBody::Result(ref value) = body
                            && let Some(next) = preparation::accept_thread(
                                tx,
                                &state_root,
                                &resources,
                                &context,
                                &ticket,
                                value,
                            )?
                        {
                            effect = Some(next);
                        } else {
                            let reason = "Provider thread resume failed; input remains pending.";
                            let current =
                                AgentRepository::get(tx, &AgentId::from(i.agent_id.as_str()))?;
                            if current.as_ref().is_some_and(|a| {
                                a.epoch == i.epoch
                                    && account_key(a) == account
                                    && a.extra
                                        .get("startOutcomeHold")
                                        .is_some_and(|h| h["operationId"] == context.operation)
                            }) {
                                hold_thread_receipt(tx, &context, ticket.generation, reason)?;
                            } else {
                                SchedulerRepository::settle_start_outcome(
                                    tx,
                                    &i.identity(),
                                    "held",
                                    Some(reason),
                                )?;
                            }
                        }
                    }
                    Method::TurnStart => match body {
                        ResponseBody::Result(value) => {
                            if let Some(turn) =
                                value["turn"]["id"].as_str().filter(|s| !s.is_empty())
                            {
                                if SchedulerRepository::settle_started_provider(
                                    tx,
                                    &i.agent_id,
                                    i.epoch,
                                    &i.attempt_id,
                                    &id_string(&ticket.id),
                                    &i.account_key,
                                    ticket.generation,
                                    i.thread_id.as_deref().unwrap_or(""),
                                    turn,
                                )? == Some(true)
                                {
                                    effect = Some(Effect::Interrupt {
                                        agent: i.agent_id.clone(),
                                        thread: i.thread_id.clone().unwrap_or_default(),
                                        turn: turn.into(),
                                    });
                                }
                                preparation::accept_turn(tx, &context, &ticket, turn)?;
                            } else {
                                SchedulerRepository::settle_start_outcome(
                                    tx,
                                    &i.identity(),
                                    "uncertain",
                                    None,
                                )?;
                                return Ok((false, None));
                            }
                        }
                        ResponseBody::Error(error) => {
                            SchedulerRepository::settle_start_rejected_provider(
                                tx,
                                &i.agent_id,
                                i.epoch,
                                &i.attempt_id,
                                &id_string(&ticket.id),
                                &i.account_key,
                                ticket.generation,
                                i.thread_id.as_deref().unwrap_or(""),
                                error,
                            )?;
                        }
                    },
                    Method::TurnInterrupt => {
                        SchedulerRepository::settle_interrupt(
                            tx,
                            &id_string(&ticket.id),
                            ticket.generation,
                            if matches!(body, ResponseBody::Error(_)) {
                                "rejected"
                            } else {
                                "accepted"
                            },
                        )?;
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        };
        if retired {
            // A retained Submit owns the same prepared/request identity. Retire
            // it with that identity's guard, never in an independent commit.
            if context.method()? == Method::TurnStart
                && let Some(key) = effect_key(&account, &Effect::Submit(i.clone()))
            {
                delete_effect(tx, &key)?;
            }
            forget_request(tx, &context.operation)?;
        }
        if let Some(effect) = &effect {
            persist_effect(tx, &account, effect)?;
        }
        Ok((retired, effect))
    })
    .await
}

pub(super) async fn disconnect_vm_actor(
    store: &RuntimeStore,
    agent_id: &str,
    epoch: i64,
    account: &str,
    generation: u64,
) -> Result<bool, WorkerError> {
    let agent_id = agent_id.to_owned();
    let account = account.to_owned();
    let disconnected = scheduler::lifecycle_transaction_with(store, move |tx| {
        SchedulerRepository::disconnect_vm_actor(tx, &agent_id, epoch, &account, generation, now())
    })
    .await?;
    Ok(disconnected)
}
/// Disconnect one ended provider generation and tell each interrupted child's
/// parent. The notice id is the stop id a later connection recovery would use
/// for the same turn or start attempt, so each parent hears of the stop once.
pub(super) fn disconnect_provider_generation(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    generation: u64,
) -> studio_runtime_store::StoreResult<()> {
    let at = now();
    for agent in SchedulerRepository::disconnect_account(tx, account, generation, at, false)? {
        crate::lifecycle_effects::notify_parent_of_disconnect(tx, &agent, at)?;
    }
    Ok(())
}
/// Settle a request whose provider outcome is unknown. Returns true when the
/// request is retired because its attempt went back to the queue.
fn uncertain(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    ticket: Option<&studio_runtime_providers::RequestTicket>,
    not_sent: bool,
    session_closed: bool,
) -> Result<bool, WorkerError> {
    let thread_request = !matches!(context.method()?, Method::TurnInterrupt | Method::TurnStart);
    // A thread request that died with its provider process never submitted the
    // input; the attempt retries on the next session instead of holding it.
    if thread_request
        && session_closed
        && SchedulerRepository::requeue_unsubmitted_attempt(tx, &context.intent.identity())?
    {
        return Ok(true);
    }
    if context.method()? == Method::TurnInterrupt {
        let ticket = ticket.ok_or_else(|| invalid(&context.operation))?;
        SchedulerRepository::settle_interrupt(
            tx,
            &id_string(&ticket.id),
            ticket.generation,
            if not_sent { "unsent" } else { "unknown" },
        )?;
    } else {
        let outcome = if not_sent {
            TurnStart::NotSubmitted
        } else if context.method()? == Method::TurnStart {
            TurnStart::Unknown
        } else {
            TurnStart::Blocked {
                reason: "Provider thread resume outcome is unknown; input remains pending.".into(),
            }
        };
        SchedulerRepository::settle_start_outcome(
            tx,
            &context.intent.identity(),
            match &outcome {
                TurnStart::NotSubmitted => "pending",
                TurnStart::Unknown => "uncertain",
                TurnStart::Blocked { .. } => "held",
            },
            match &outcome {
                TurnStart::Blocked { reason } => Some(reason.as_str()),
                _ => None,
            },
        )?;
    }
    Ok(false)
}
#[allow(clippy::too_many_arguments)]
pub(super) async fn journal_batch(
    store: Arc<RuntimeStore>,
    account: &str,
    journal_scope: &str,
    streams: &NotificationStreamBuffer,
    entries: Vec<JournalEntry>,
    claude_account_pin: &Value,
    expected_vm_actor: Option<(&str, i64)>,
    native_source: Option<crate::notification_dispatch::NativeProcessSource>,
) -> Result<JournalBatchResult, WorkerError> {
    if entries.is_empty() {
        return Ok(Some((streams.clone(), Vec::new(), Vec::new())));
    }
    let needs_limits = entries.iter().any(|entry| {
        matches!(
            &entry.payload,
            JournalPayload::Frame(Frame::Notification(frame))
                if frame.method.as_str() == "turn/completed"
        )
    });
    let read_store = Arc::clone(&store);
    let account_for_read = account.to_owned();
    let (publication, limits_observation) =
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker_journal_snapshot",
            move || {
                let publication =
                    crate::notification_dispatch::PublicationContext::load(&read_store)?.with_native_source(native_source);
                let limits_observation = if needs_limits {
                    let reader = read_store.read()?;
                    let limits = studio_runtime_store::account_state::limits_snapshots(&reader)?
                        .snapshots
                        .into_iter()
                        .find(|(key, _)| key == &account_for_read)
                        .map(|(_, limits)| limits)
                        .unwrap_or_else(|| {
                            json!({"accountKey":account_for_read,"at":null,"error":null,"data":null,"stale":true})
                        });
                    Some(
                        crate::usage_resume::LimitsObservation::from_record(limits, None)
                            .map_err(|error| {
                                WorkerError::PreparationRefused(error.to_string())
                            })?,
                    )
                } else {
                    None
                };
                Ok::<_, WorkerError>((publication, limits_observation))
            },
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
    let account = account.to_owned();
    let journal_scope = journal_scope.to_owned();
    let claude_account_pin = claude_account_pin.clone();
    let expected_vm_actor = expected_vm_actor.map(|(actor, epoch)| (actor.to_owned(), epoch));
    let streams = streams.clone();
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        let mut next = streams.clone();
        let mut committed = Vec::with_capacity(entries.len());
        let mut deferred = Vec::new();
        let mut persist_snapshot = false;
        let mut entries = entries.into_iter();
        while let Some(entry) = entries.next() {
            match journal_one(
                tx,
                JournalIdentity {
                    account: &account,
                    journal_scope: &journal_scope,
                    expected_vm_actor: expected_vm_actor
                        .as_ref()
                        .map(|(actor, epoch)| (actor.as_str(), *epoch)),
                },
                &publication,
                &next,
                &entry,
                &claude_account_pin,
                limits_observation.as_ref(),
            )? {
                Some((updated, effect, changed)) => {
                    next = updated;
                    persist_snapshot |= changed;
                    committed.push((entry, effect));
                }
                None => {
                    deferred.push(entry);
                    deferred.extend(entries);
                    break;
                }
            }
        }
        if persist_snapshot {
            persist_streams(tx, &account, &next)?;
        }
        Ok(Some((next, committed, deferred)))
    })
    .await
}

fn journal_one(
    tx: &rusqlite::Transaction<'_>,
    identity: JournalIdentity<'_>,
    publication: &crate::notification_dispatch::PublicationContext,
    streams: &NotificationStreamBuffer,
    entry: &JournalEntry,
    claude_account_pin: &Value,
    limits_observation: Option<&crate::usage_resume::LimitsObservation>,
) -> Result<Option<(NotificationStreamBuffer, Option<Effect>, bool)>, WorkerError> {
    let JournalIdentity {
        account,
        journal_scope,
        expected_vm_actor,
    } = identity;
    let operation = format!(
        "journal:{journal_scope}:{}:{}",
        entry.generation.0, entry.cursor.0
    );
    let digest = match &entry.payload {
        JournalPayload::Frame(frame) => {
            studio_runtime_store::sha256_hex(&journal_frame_payload(frame, &operation)?)
        }
        JournalPayload::RawStdout { bytes, .. } => {
            studio_runtime_store::sha256_hex(json!({"raw":bytes}).to_string().as_bytes())
        }
        JournalPayload::Stderr(value) => {
            studio_runtime_store::sha256_hex(json!({"stderr":value}).to_string().as_bytes())
        }
        JournalPayload::Exit(value) => {
            studio_runtime_store::sha256_hex(json!({"exit":value}).to_string().as_bytes())
        }
        JournalPayload::Other { kind, payload } => studio_runtime_store::sha256_hex(
            json!({"kind":kind,"payload":payload})
                .to_string()
                .as_bytes(),
        ),
    };
    if let Some((actor_id, expected_epoch)) = expected_vm_actor
        && !crate::linux_vm_provider::vm_actor_matches(tx, actor_id, expected_epoch, account, None)?
    {
        return Err(WorkerError::StaleVmActor);
    }
    if entry.cursor.0 <= checkpoint(tx, journal_scope, entry.generation.0)? {
        return Ok(Some((streams.clone(), None, false)));
    }
    if let Some(old) = receipt(tx, &operation, "settled")? {
        if old["digest"] != digest {
            return Err(invalid(&operation));
        }
        if old["committed"] == true {
            return Ok(Some((streams.clone(), None, false)));
        }
    }
    replace(
        tx,
        &operation,
        "settled",
        json!({"cursor":entry.cursor.0,"digest":digest,"ready":false,"committed":false}),
    )?;
    let mut next = streams.clone();
    let mut effect = None;
    let mut publications = Vec::new();
    if let JournalPayload::Frame(frame) = &entry.payload {
        let notice = match frame {
            Frame::Notification(f) => {
                Some((f.method.as_str(), f.params.as_ref().unwrap_or(&Value::Null)))
            }
            Frame::UnknownMethod(f) if !f.fields.contains_key("id") => Some((
                f.method.as_str(),
                f.fields.get("params").unwrap_or(&Value::Null),
            )),
            _ => None,
        };
        if let Some(("command/exec/outputDelta", params)) = notice
            && let Some(process_id) = params
                .get("processId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
        {
            crate::monitor_event_queue::enqueue_output_with_scope_in_transaction(
                tx,
                process_id,
                crate::monitor_event_queue::JournalEventScope {
                    account,
                    cursor_scope: journal_scope,
                    generation: entry.generation.0,
                    cursor: entry.cursor.0,
                    expected_agent: expected_vm_actor.map(|(actor, _)| actor),
                },
                params,
            )?;
        }
        if let Some((method, params)) = notice
            && let Some((agent, epoch)) = steer::acknowledge(tx, account, method, params)?
        {
            // Delivered busy-turn input frees the agent for its next queued input.
            effect = Some(Effect::Wake(agent, epoch));
        } else if let Some((method, params)) = notice
            && let Some(thread) = params["threadId"].as_str()
        {
            let turn = params["turn"]["id"]
                .as_str()
                .or_else(|| params["turnId"].as_str());
            let (_, attempt) = SchedulerRepository::find_attempt_for_thread(
                tx,
                account,
                entry.generation.0,
                thread,
                turn,
            )?;
            let attempt = attempt.filter(|(identity, _)| {
                expected_vm_actor.is_none_or(|(actor_id, _)| identity.agent_id == actor_id)
            });
            let attempt = match attempt {
                Some(attempt) => Some(attempt),
                None if method == "turn/completed" => {
                    crate::context_repair::source_terminal_attempt(
                        tx,
                        account,
                        entry.generation.0,
                        params,
                    )
                    .map_err(|_| studio_runtime_store::StoreError::InvalidSyncObject {
                        scope: "runtime_context_terminal_receipts".into(),
                        id: thread.to_owned(),
                    })?
                    .map(|identity| (identity, true))
                }
                None => None,
            };
            if let Some((identity, bound)) = attempt {
                match method {
                    "turn/started" => {
                        if let Some(turn) = turn
                            && let Some(interrupt) =
                                SchedulerRepository::observe_started(tx, &identity, turn)?
                        {
                            let agent = NotificationDispatchRepository::agent_record(
                                tx,
                                &identity.agent_id,
                            )?
                            .ok_or_else(|| invalid(&identity.agent_id))?;
                            let frame = json!({"method":method,"params":params});
                            let delivery = ProviderDeliveryIdentity {
                                provider_handle: ProviderHandle::for_account(journal_scope)
                                    .as_str()
                                    .into(),
                                generation: entry.generation.0,
                                cursor: entry.cursor.0,
                            };
                            next.queue_effects(
                                crate::notification_runtime_effects::RuntimeNotificationEffects
                                    .post_commit(
                                        Some(&agent),
                                        Some(&agent),
                                        &frame,
                                        &delivery,
                                        now(),
                                        &crate::notification_dispatch::DispatchOutcome::Written,
                                    ),
                            );
                            publications.push(next.take_post_commit()?);
                            if interrupt {
                                effect = Some(Effect::Interrupt {
                                    agent: identity.agent_id,
                                    thread: thread.into(),
                                    turn: turn.into(),
                                });
                            }
                        }
                    }
                    "turn/completed" => {
                        if !bound {
                            return Ok(None);
                        }
                        next = flush_notification_streams(
                            tx,
                            &identity.agent_id,
                            publication,
                            streams,
                            now(),
                        )?
                        .unwrap_or_else(|| streams.clone());
                        publications.push(next.take_post_commit()?);
                        let agent_before =
                            NotificationDispatchRepository::agent_record(tx, &identity.agent_id)?
                                .ok_or_else(|| invalid(&identity.agent_id))?;
                        if scheduler::settle_terminal_in_transaction(tx, &identity, params)? {
                            let claude_auth = if agent_before["provider"] == "claude"
                                && claude_account_pin.is_object()
                            {
                                let pin =
                                    crate::continuation_runtime::ClaudeAccountPin::from_value(
                                        claude_account_pin.clone(),
                                    )
                                    .map_err(|error| {
                                        StoreError::SchemaMismatch(error.to_string())
                                    })?;
                                let settings =
                                    crate::continuation_runtime::PreparationSettings::from_agent(
                                        &agent_before,
                                    )
                                    .map_err(|error| {
                                        StoreError::SchemaMismatch(error.to_string())
                                    })?;
                                Some(crate::continuation_runtime::ClaudeAuthObservation::checked(
                                    pin, settings,
                                ))
                            } else {
                                None
                            };
                            crate::continuation_runtime::stage(
                                tx,
                                crate::continuation_runtime::Completion::TerminalTurn {
                                    agent_id: &identity.agent_id,
                                    turn: &params["turn"],
                                    known: true,
                                    now: now(),
                                    limits: limits_observation,
                                    claude_auth: claude_auth.as_ref(),
                                },
                            )
                            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                            let agent_after = NotificationDispatchRepository::agent_record(
                                tx,
                                &identity.agent_id,
                            )?
                            .ok_or_else(|| invalid(&identity.agent_id))?;
                            let frame = json!({"method":method,"params":params});
                            let delivery = ProviderDeliveryIdentity {
                                provider_handle: ProviderHandle::for_account(journal_scope)
                                    .as_str()
                                    .into(),
                                generation: entry.generation.0,
                                cursor: entry.cursor.0,
                            };
                            next.queue_effects(
                                crate::notification_runtime_effects::RuntimeNotificationEffects
                                    .post_commit(
                                        Some(&agent_before),
                                        Some(&agent_after),
                                        &frame,
                                        &delivery,
                                        now(),
                                        &crate::notification_dispatch::DispatchOutcome::Written,
                                    ),
                            );
                            publications.push(next.take_post_commit()?);
                            crate::notification_dispatch::prune_inactive_streams(
                                tx,
                                &mut next,
                                now(),
                                false,
                            )?;
                            effect = Some(Effect::Wake(identity.agent_id, identity.epoch));
                        }
                    }
                    _ => {
                        let dispatched = dispatch_notification_with_effects(
                            tx,
                            &identity.agent_id,
                            publication,
                            streams,
                            &ProviderDeliveryIdentity {
                                provider_handle: ProviderHandle::for_account(journal_scope)
                                    .as_str()
                                    .into(),
                                generation: entry.generation.0,
                                cursor: entry.cursor.0,
                            },
                            frame,
                            now(),
                            &crate::notification_runtime_effects::RuntimeNotificationEffects,
                        )?;
                        publications.push(dispatched.post_commit);
                        let staged = dispatched.stream_update.unwrap_or_else(|| streams.clone());
                        next = if staged.pending_bytes() >= STREAM_FLUSH_BYTES {
                            let flushed = flush_notification_streams(
                                tx,
                                &identity.agent_id,
                                publication,
                                &staged,
                                now(),
                            )?;
                            if let Some(mut flushed) = flushed {
                                publications.push(flushed.take_post_commit()?);
                                flushed
                            } else {
                                staged
                            }
                        } else {
                            staged
                        };
                    }
                }
            }
        }
    }
    for batch in &publications {
        persist_publication_batch(tx, account, batch)?;
    }
    if let Some(effect) = &effect {
        persist_effect(tx, account, effect)?;
    }
    let callback = matches!(&entry.payload, JournalPayload::Frame(frame) if super::callback::response(frame).is_some());
    replace(
        tx,
        &operation,
        "settled",
        json!({"cursor":entry.cursor.0,"digest":digest,"ready":!callback,"committed":true}),
    )?;
    compact(tx, journal_scope, entry.generation.0)?;
    Ok(Some((next, effect, true)))
}
pub(super) async fn save_recovery(
    store: &RuntimeStore,
    account: &str,
    recovery: &studio_runtime_process::provider_seam::InitializationRecovery,
) -> Result<(), WorkerError> {
    let operation = format!(
        "recovery:{account}:{}",
        recovery.generation.map_or(0, |g| g.0)
    );
    let handle = recovery.handle.as_str().to_owned();
    let generation = recovery.generation.map(|g| g.0);
    let phase = format!("{:?}", recovery.phase);
    store
        .transaction_with(move |tx| {
            save(
                tx,
                &operation,
                "required",
                json!({"handle":handle,"generation":generation,"phase":phase}),
            )
        })
        .await
}
pub(super) async fn stop(
    store: &RuntimeStore,
    account: &str,
    agent: &str,
) -> Result<Option<Effect>, WorkerError> {
    let account = account.to_owned();
    let agent = agent.to_owned();
    scheduler::lifecycle_transaction_with(store, move |tx| {
        let Some(record) = AgentRepository::get(tx, &AgentId::from(agent.as_str()))? else {
            return Ok(None);
        };
        if record
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != account.as_str()
        {
            return Ok(None);
        }
        SchedulerRepository::stop_agent(tx, &agent, "Stopped by user")?;
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(agent.as_str()))? else {
            return Ok(None);
        };
        let attempt = &agent.extra.get("startAttempt").unwrap_or(&Value::Null);
        let turn = attempt["turnId"]
            .as_str()
            .or_else(|| attempt["observedTurnId"].as_str());
        let effect = turn
            .zip(agent.extra.get("threadId").unwrap_or(&Value::Null).as_str())
            .map(|(turn, thread)| Effect::Interrupt {
                agent: agent.id.0.clone(),
                thread: thread.into(),
                turn: turn.into(),
            });
        if let Some(effect) = &effect {
            persist_effect(tx, &account, effect)?;
        }
        Ok(effect)
    })
    .await
}
pub(super) fn interrupt_intent(
    connection: &rusqlite::Connection,
    agent: &str,
    thread: &str,
) -> Result<Option<(TurnIntent, i64)>, WorkerError> {
    let Some(a) = AgentRepository::get(connection, &AgentId::from(agent))? else {
        return Ok(None);
    };
    let attempt = &a.extra.get("startAttempt").unwrap_or(&Value::Null);
    let interrupt = &attempt["interrupt"];
    if interrupt["threadId"] == thread
        && matches!(interrupt["outcome"].as_str(), Some("unknown" | "accepted"))
    {
        return Ok(None);
    }
    Ok(attempt["id"]
        .as_str()
        .zip(attempt["epoch"].as_i64())
        .map(|(id, epoch)| {
            (
                TurnIntent {
                    agent_id: agent.into(),
                    epoch,
                    attempt_id: id.into(),
                    account_key: a
                        .extra
                        .get("accountKey")
                        .unwrap_or(&Value::Null)
                        .as_str()
                        .unwrap_or("default")
                        .into(),
                    provider: "codex".into(),
                    thread_id: Some(thread.into()),
                    cwd: None,
                    model: None,
                    events: vec![],
                    radio_authorized: false,
                },
                a.epoch,
            )
        }))
}
pub(super) fn id_string(id: &RequestId) -> String {
    match id {
        RequestId::String(id) => id.clone(),
        RequestId::Number(id) => id.to_string(),
    }
}

pub(super) async fn was_submitted(
    store: &RuntimeStore,
    operation: &str,
) -> Result<bool, WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| Ok(receipt(tx, &operation, "submitted")?.is_some()))
        .await
}

/// A persisted login bootstrap is a host-authenticated external effect. A
/// submitted row with no completion is uncertain and must never be resent.
pub(super) async fn vm_bootstrap_admission(
    store: &RuntimeStore,
    operation: &str,
    signature: &str,
) -> Result<bool, WorkerError> {
    let operation = operation.to_owned();
    let signature = signature.to_owned();
    store
        .transaction_with(move |tx| {
            if let Some(saved) = receipt(tx, &operation, "complete")? {
                if saved["signature"] != signature {
                    return Err(invalid(&operation));
                }
                return Ok(true);
            }
            if let Some(saved) = receipt(tx, &operation, "submitted")? {
                if saved["signature"] != signature {
                    return Err(invalid(&operation));
                }
                return Err(WorkerError::CallbackUnknown);
            }
            save(tx, &operation, "submitted", json!({"signature":signature}))?;
            Ok(false)
        })
        .await
}

pub(super) async fn vm_bootstrap_settled(
    store: &RuntimeStore,
    operation: &str,
    signature: &str,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    let signature = signature.to_owned();
    store
        .transaction_with(move |tx| {
            let Some(saved) = receipt(tx, &operation, "submitted")? else {
                return Err(invalid(&operation));
            };
            if saved["signature"] != signature {
                return Err(invalid(&operation));
            }
            save(tx, &operation, "complete", json!({"signature":signature}))
        })
        .await
}

// Written callback responses can be acknowledged on replay. A Submitted marker
// without Written is uncertainty, never permission to answer a second time.
pub(super) async fn callback_admission(
    store: &RuntimeStore,
    operation: &str,
    journal_scope: &str,
    generation: u64,
    cursor: u64,
) -> Result<bool, WorkerError> {
    let operation = operation.to_owned();
    let journal_scope = journal_scope.to_owned();
    store
        .transaction_with(move |tx| {
            if cursor <= checkpoint(tx, &journal_scope, generation)? {
                return Ok(true);
            }
            if receipt(tx, &operation, "written")? == Some(json!(true)) {
                settle_callback_in_transaction(tx, &journal_scope, generation, cursor)?;
                return Ok(true);
            }
            if receipt(tx, &operation, "submitted")?.is_some() {
                return Err(WorkerError::CallbackUnknown);
            }
            save(tx, &operation, "submitted", json!(true))?;
            Ok(false)
        })
        .await
}
pub(super) async fn callback_written(
    store: &RuntimeStore,
    operation: &str,
    outcome: WriteOutcome,
) -> Result<(), WorkerError> {
    let operation = operation.to_owned();
    store
        .transaction_with(move |tx| {
            if outcome == WriteOutcome::NotSent {
                tx.execute(
                    "DELETE FROM runtime_operation_receipts WHERE id=?1",
                    [receipt_id(&operation, "submitted")],
                )?;
                return Ok(());
            }
            save(
                tx,
                &operation,
                if outcome == WriteOutcome::Written {
                    "written"
                } else {
                    "unknown"
                },
                json!(true),
            )
        })
        .await
}

// The follow-on effect is part of the transition that owes it. Repeating an
// effect after a crash is safe: submission checks its durable operation identity.
pub(super) fn effect_key(account: &str, effect: &Effect) -> Option<String> {
    if matches!(effect, Effect::Callback(..)) {
        return None;
    }
    let raw = serde_json::to_vec(effect).ok()?;
    Some(format!(
        "effect:{account}:{}",
        studio_runtime_store::sha256_hex(&raw)
    ))
}
fn persist_effect(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    effect: &Effect,
) -> Result<(), WorkerError> {
    if let Some(key) = effect_key(account, effect) {
        if receipt(tx, &key, "pending")?.is_some() {
            return Ok(());
        }
        let prefix = format!("account-worker:effect:{account}:");
        let order: i64 = tx.query_row("SELECT COALESCE(MAX(json_extract(result,'$.order')),0)+1 FROM runtime_operation_receipts WHERE id>=?1 AND id<?2", params![prefix,format!("{prefix}~")], |row| row.get(0))?;
        save(
            tx,
            &key,
            "pending",
            json!({"order":order,"effect":serde_json::to_value(effect).map_err(|_|invalid(&key))?}),
        )?;
    }
    Ok(())
}
fn delete_effect(tx: &studio_runtime_store::Transaction<'_>, key: &str) -> Result<(), WorkerError> {
    tx.execute(
        "DELETE FROM runtime_operation_receipts WHERE id=?1",
        [receipt_id(key, "pending")],
    )?;
    Ok(())
}
pub(super) async fn complete_effect(store: &RuntimeStore, key: &str) -> Result<(), WorkerError> {
    let key = key.to_owned();
    store
        .transaction_with(move |tx| delete_effect(tx, &key))
        .await
}
pub(super) fn recover_effects(
    connection: &rusqlite::Connection,
    account: &str,
) -> Result<VecDeque<Effect>, WorkerError> {
    let prefix = format!("account-worker:effect:{account}:");
    let mut stmt = connection.prepare(
        "SELECT result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.order'),rowid",
    )?;
    let rows = stmt
        .query_map(params![prefix, format!("{prefix}~")], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|raw| {
            let value: Value = serde_json::from_str(&raw).map_err(|_| invalid(account))?;
            serde_json::from_value(value.get("effect").unwrap_or(&value).clone())
                .map_err(|_| invalid(account))
        })
        .collect()
}

fn forget_request(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
) -> Result<(), WorkerError> {
    for phase in ["submitted", "written", "resolved"] {
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id=?1",
            [receipt_id(operation, phase)],
        )?;
    }
    Ok(())
}

pub(super) fn replace(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    phase: &str,
    value: Value,
) -> Result<(), WorkerError> {
    tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET result=excluded.result", params![receipt_id(operation, phase),phase,value.to_string()])?;
    Ok(())
}
pub(super) fn checkpoint(
    tx: &studio_runtime_store::Transaction<'_>,
    journal_scope: &str,
    generation: u64,
) -> Result<u64, WorkerError> {
    Ok(
        receipt(tx, &format!("cursor:{journal_scope}:{generation}"), "ack")?
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    )
}
pub(super) fn journal_committed(
    tx: &studio_runtime_store::Transaction<'_>,
    journal_scope: &str,
    generation: u64,
    cursor: u64,
) -> Result<bool, WorkerError> {
    let operation = format!("journal:{journal_scope}:{generation}:{cursor}");
    Ok(receipt(tx, &operation, "settled")?.is_some_and(|value| value["committed"] == true))
}
fn compact(
    tx: &studio_runtime_store::Transaction<'_>,
    journal_scope: &str,
    generation: u64,
) -> Result<(), WorkerError> {
    let prefix = format!("account-worker:journal:{journal_scope}:{generation}:");
    let mut stmt = tx.prepare("SELECT result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.cursor') LIMIT 65")?;
    let rows = stmt
        .query_map(params![prefix, format!("{prefix}~")], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cursor = checkpoint(tx, journal_scope, generation)?;
    for raw in rows {
        let row: Value = serde_json::from_str(&raw).map_err(|_| invalid(journal_scope))?;
        let next = row["cursor"]
            .as_u64()
            .ok_or_else(|| invalid(journal_scope))?;
        // A missing earlier row may still be deferred before its dispatch.
        if row["ready"] != true {
            break;
        }
        cursor = next;
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id=?1",
            [receipt_id(
                &format!("journal:{journal_scope}:{generation}:{cursor}"),
                "settled",
            )],
        )?;
        for phase in ["submitted", "written", "unknown"] {
            tx.execute(
                "DELETE FROM runtime_operation_receipts WHERE id=?1",
                [receipt_id(
                    &format!("callback:{journal_scope}:{generation}:{cursor}"),
                    phase,
                )],
            )?;
        }
        let identity = json!({"providerHandle":ProviderHandle::for_account(journal_scope).as_str(),"generation":generation,"cursor":cursor});
        let signature = studio_runtime_store::sha256_hex(identity.to_string().as_bytes());
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id=?1",
            [format!("provider-notification:{signature}")],
        )?;
    }
    replace(
        tx,
        &format!("cursor:{journal_scope}:{generation}"),
        "ack",
        json!(cursor),
    )
}
pub(super) async fn callback_settled(
    store: &RuntimeStore,
    journal_scope: &str,
    generation: u64,
    cursor: u64,
) -> Result<(), WorkerError> {
    let journal_scope = journal_scope.to_owned();
    store
        .transaction_with(move |tx| {
            settle_callback_in_transaction(tx, &journal_scope, generation, cursor)
        })
        .await
}
fn settle_callback_in_transaction(
    tx: &studio_runtime_store::Transaction<'_>,
    journal_scope: &str,
    generation: u64,
    cursor: u64,
) -> Result<(), WorkerError> {
    let operation = format!("journal:{journal_scope}:{generation}:{cursor}");
    if let Some(mut row) = receipt(tx, &operation, "settled")? {
        row["ready"] = json!(true);
        replace(tx, &operation, "settled", row)?;
    }
    compact(tx, journal_scope, generation)
}

// Python codex_streaming.FLUSH_SECONDS is .75. The additional byte threshold
// bounds a durable batch when producers outpace that timer; answers are uncut.
pub(super) const STREAM_FLUSH_BYTES: usize = 64 * 1024;
type JournalBatchResult = Option<(
    NotificationStreamBuffer,
    Vec<(JournalEntry, Option<Effect>)>,
    Vec<JournalEntry>,
)>;
#[derive(Clone, Copy)]
struct JournalIdentity<'a> {
    account: &'a str,
    journal_scope: &'a str,
    expected_vm_actor: Option<(&'a str, i64)>,
}
pub(crate) fn persist_publication_batch(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    batch: &NotificationPublicationBatch,
) -> Result<(), WorkerError> {
    if batch.is_empty() {
        return Ok(());
    }
    let prefix = format!("account-worker:notification-publication:{account}:");
    let pending: i64 = tx.query_row(
        "SELECT count(*) FROM runtime_operation_receipts WHERE id>=?1 AND id<?2",
        params![prefix, format!("{prefix}~")],
        |row| row.get(0),
    )?;
    let pending = usize::try_from(pending).map_err(|_| WorkerError::Capacity)?;
    let mut available = MAX_PENDING_PUBLICATION_RECEIPTS.saturating_sub(pending);
    let mut retained = 0;
    let mut overflow = batch.analytics_overflow;
    let publication_overflow = 0_u64;
    let mut last_saved = None;
    for (index, effect) in batch.effects.iter().enumerate() {
        if retained == available {
            if is_analytics_effect(effect) {
                overflow = overflow.saturating_add(1);
            } else if let Some(id) = oldest_analytics_receipt(tx, &prefix)? {
                // Python drops captures at its analytics queue bound, but its
                // token-rate observer continues to receive committed events.
                // Reclaim only an analytics receipt to keep that behavior.
                tx.execute("DELETE FROM runtime_operation_receipts WHERE id=?1", [&id])?;
                overflow = overflow.saturating_add(1);
                available += 1;
                last_saved = Some(save_publication_effect(tx, account, batch, index, effect)?);
                retained += 1;
            } else {
                // The pending token-rate queue is itself full. Coalesce this
                // observation into the newest token-rate receipt rather than
                // dropping the committed token-rate update.
                coalesce_token_rate_receipt(tx, &prefix, effect)?;
            }
            continue;
        }
        last_saved = Some(save_publication_effect(tx, account, batch, index, effect)?);
        retained += 1;
    }
    if overflow > 0 || publication_overflow > 0 {
        if last_saved.is_none() && available > 0 {
            let operation = format!(
                "notification-publication:{account}:{}:{}",
                batch.sequence,
                batch.effects.len()
            );
            save(
                tx,
                &operation,
                "pending",
                json!({
                    "account":account,
                    "sequence":batch.sequence,
                    "index":batch.effects.len(),
                    "analyticsOverflow":overflow,
                    "publicationOverflow":publication_overflow,
                }),
            )?;
            last_saved = Some(operation);
        }
        let target = if let Some(operation) = last_saved {
            Some(receipt_id(&operation, "pending"))
        } else {
            tx.query_row(
                "SELECT id FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.sequence') DESC,json_extract(result,'$.index') DESC,rowid DESC LIMIT 1",
                params![prefix, format!("{prefix}~")],
                |row| row.get::<_, String>(0),
            ).optional()?
        };
        if let Some(id) = target {
            let pending_invalidation: Option<String> = tx
                .query_row(
                    "SELECT id FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 AND json_extract(result,'$.stateInvalidation')=1 ORDER BY rowid LIMIT 1",
                    params![prefix, format!("{prefix}~")],
                    |row| row.get(0),
                )
                .optional()?;
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=json_set(json_set(result,'$.analyticsOverflow',COALESCE(json_extract(result,'$.analyticsOverflow'),0)+?2),'$.publicationOverflow',COALESCE(json_extract(result,'$.publicationOverflow'),0)+?3) WHERE id=?1",
                params![
                    id,
                    overflow.min(i64::MAX as u64) as i64,
                    publication_overflow.min(i64::MAX as u64) as i64
                ],
            )?;
            if let Some(existing) = pending_invalidation {
                // Changing the receipt result makes an in-flight drain retry
                // this same coalesced invalidation after the commit.
                tx.execute(
                    "UPDATE runtime_operation_receipts SET result=json_set(result,'$.stateInvalidationGeneration',COALESCE(json_extract(result,'$.stateInvalidationGeneration'),0)+1) WHERE id=?1",
                    [&existing],
                )?;
            } else {
                tx.execute(
                    "UPDATE runtime_operation_receipts SET result=json_set(result,'$.stateInvalidation',json('true')) WHERE id=?1",
                    [&id],
                )?;
            }
        } else {
            // The only reachable case is a legacy database already beyond the
            // named cap with no publication rows to annotate. Preserve the
            // provider commit; the cap remains in force for all new receipts.
        }
    }
    Ok(())
}

fn save_publication_effect(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    batch: &NotificationPublicationBatch,
    index: usize,
    effect: &crate::notification_dispatch::NotificationPostCommit,
) -> Result<String, WorkerError> {
    let operation = format!(
        "notification-publication:{account}:{}:{index}",
        batch.sequence
    );
    save(
        tx,
        &operation,
        "pending",
        json!({"account":account,"sequence":batch.sequence,"index":index,"effect":effect}),
    )?;
    Ok(operation)
}

fn oldest_analytics_receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    prefix: &str,
) -> Result<Option<String>, WorkerError> {
    let mut statement = tx.prepare(
        "SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.sequence'),json_extract(result,'$.index'),rowid",
    )?;
    let rows = statement
        .query_map(params![prefix, format!("{prefix}~")], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, result) in rows {
        let value: Value = serde_json::from_str(&result).map_err(|_| invalid(&id))?;
        let Some(effect) = value.get("effect") else {
            continue;
        };
        let effect: crate::notification_dispatch::NotificationPostCommit =
            serde_json::from_value(effect.clone()).map_err(|_| invalid(&id))?;
        if is_analytics_effect(&effect) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn coalesce_token_rate_receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    prefix: &str,
    effect: &crate::notification_dispatch::NotificationPostCommit,
) -> Result<(), WorkerError> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 AND (json_type(result,'$.effect.TokenRateStream') IS NOT NULL OR json_type(result,'$.effect.TokenRateAssociation') IS NOT NULL OR json_type(result,'$.effect.TokenRateUsage') IS NOT NULL OR EXISTS (SELECT 1 FROM json_each(result,'$.coalescedEffects') AS effect WHERE json_type(effect.value,'$.TokenRateStream') IS NOT NULL OR json_type(effect.value,'$.TokenRateAssociation') IS NOT NULL OR json_type(effect.value,'$.TokenRateUsage') IS NOT NULL)) ORDER BY json_extract(result,'$.sequence') DESC,json_extract(result,'$.index') DESC,rowid DESC LIMIT 1",
            params![prefix, format!("{prefix}~")],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let row = match row {
        Some(row) => Some(row),
        None => tx
            .query_row(
                "SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.sequence') DESC,json_extract(result,'$.index') DESC,rowid DESC LIMIT 1",
                params![prefix, format!("{prefix}~")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?,
    };
    if let Some((id, result)) = row {
        let mut value: Value = serde_json::from_str(&result).map_err(|_| invalid(&id))?;
        let mut effects: Vec<crate::notification_dispatch::NotificationPostCommit> = Vec::new();
        if let Some(primary) = value.get("effect") {
            effects.push(serde_json::from_value(primary.clone()).map_err(|_| invalid(&id))?);
        }
        if let Some(additional) = value.get("coalescedEffects") {
            let additional = additional.as_array().ok_or_else(|| invalid(&id))?;
            effects.extend(
                additional
                    .iter()
                    .cloned()
                    .map(serde_json::from_value)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| invalid(&id))?,
            );
        }
        if effects.len().saturating_sub(1) >= MAX_COALESCED_TOKEN_RATE_EFFECTS {
            return Err(WorkerError::Capacity);
        }
        // Keep each provider delivery identity intact. Rewriting earlier deltas
        // under a later cursor can double-project them after a partial drain.
        effects.push(effect.clone());
        let primary = effects.first().ok_or_else(|| invalid(&id))?;
        value["effect"] = serde_json::to_value(primary).map_err(|_| invalid(&id))?;
        if effects.len() > 1 {
            value["coalescedEffects"] =
                serde_json::to_value(&effects[1..]).map_err(|_| invalid(&id))?;
        } else {
            value
                .as_object_mut()
                .ok_or_else(|| invalid(&id))?
                .remove("coalescedEffects");
        }
        value["tokenRateCoalesced"] = json!(true);
        let serialized = value.to_string();
        if serialized.len() > MAX_COALESCED_TOKEN_RATE_BYTES {
            return Err(WorkerError::Capacity);
        }
        tx.execute(
            "UPDATE runtime_operation_receipts SET result=?2 WHERE id=?1",
            params![id, serialized],
        )?;
    } else {
        return Err(WorkerError::Capacity);
    }
    Ok(())
}

fn is_analytics_effect(effect: &crate::notification_dispatch::NotificationPostCommit) -> bool {
    matches!(
        effect,
        crate::notification_dispatch::NotificationPostCommit::Analytics { .. }
            | crate::notification_dispatch::NotificationPostCommit::AnalyticsStream { .. }
    )
}

/// Drain in sequence/index order. Publication is outside SQLite; deleting each
/// row happens only after the sink accepts it, in its own short transaction.
pub(super) async fn drain_publications(
    store: Arc<RuntimeStore>,
    account: String,
    sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
) -> Result<usize, WorkerError> {
    drain_publications_with_hook(store, account, sink, || Ok(())).await
}

pub(super) async fn drain_publications_with_hook(
    store: Arc<RuntimeStore>,
    account: String,
    sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
    mut before_delete: impl FnMut() -> Result<(), WorkerError> + Send,
) -> Result<usize, WorkerError> {
    let prefix = format!("account-worker:notification-publication:{account}:");
    let read_store = Arc::clone(&store);
    let pending = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_publication_read",
        move || {
            let connection = read_store.read()?;
            let mut statement = connection.prepare(
                "SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.sequence'),json_extract(result,'$.index'),rowid LIMIT ?3",
            )?;
            Ok::<_, WorkerError>(statement
                .query_map(
                    params![prefix, format!("{prefix}~"), PUBLICATION_DRAIN_LIMIT as i64],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?)
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    let mut delivered = 0;
    for (id, result) in pending {
        let value: Value = serde_json::from_str(&result).map_err(|_| invalid(&id))?;
        let analytics_overflow = value
            .get("analyticsOverflow")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let publication_overflow = value
            .get("publicationOverflow")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if analytics_overflow > 0 || publication_overflow > 0 {
            crate::analytics_capture::record_publication_overflow(
                &store,
                id.clone(),
                analytics_overflow,
                publication_overflow,
            )
            .await
            .map_err(|_| WorkerError::Capacity)?;
        }
        let mut effects = Vec::new();
        if let Some(effect) = value.get("effect") {
            effects.push(serde_json::from_value(effect.clone()).map_err(|_| invalid(&id))?);
        }
        if let Some(coalesced) = value.get("coalescedEffects") {
            let coalesced = coalesced.as_array().ok_or_else(|| invalid(&id))?;
            effects.extend(
                coalesced
                    .iter()
                    .cloned()
                    .map(serde_json::from_value)
                    .collect::<Result<Vec<crate::notification_dispatch::NotificationPostCommit>, _>>()
                    .map_err(|_| invalid(&id))?,
            );
        }
        if !effects.is_empty() {
            // The sink's delivery identity is the effect's provider identity.
            // It must make a replay after a crash between publish and delete safe.
            for effect in effects {
                sink.publish(&effect).await?;
            }
        } else if value.get("analyticsOverflow").is_none()
            && value.get("publicationOverflow").is_none()
        {
            return Err(invalid(&id));
        }
        if value.get("stateInvalidation") == Some(&Value::Bool(true)) {
            let generation = value
                .get("stateInvalidationGeneration")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let request_id = format!("coalesced-state-invalidation:{id}:{generation}");
            sink.invalidate_state(&request_id).await?;
        }
        before_delete()?;
        // Overflow diagnostics are persisted to analytics_meta before receipt
        // deletion, matching Python's lossy analytics queue accounting.
        let delete_id = id.clone();
        store
            .transaction_with(move |tx| {
                let current: Option<String> = tx
                    .query_row(
                        "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                        [&delete_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if current.as_deref() == Some(result.as_str()) {
                    tx.execute(
                        "DELETE FROM runtime_operation_receipts WHERE id=?1",
                        [&delete_id],
                    )?;
                }
                Ok::<(), WorkerError>(())
            })
            .await?;
        delivered += 1;
    }
    Ok(delivered)
}

pub(crate) fn persist_streams(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    streams: &NotificationStreamBuffer,
) -> Result<(), WorkerError> {
    let mut metadata = serde_json::to_value(streams).map_err(|_| invalid(account))?;
    let mut pending = Vec::new();
    for entry in metadata["entries"]
        .as_array_mut()
        .ok_or_else(|| invalid(account))?
    {
        if entry["count"].as_u64().unwrap_or(0) > 0 {
            pending.push(entry.clone());
        }
        entry["pending"] = json!("");
        entry["count"] = json!(0);
        entry["params"] = Value::Null;
    }
    let key = format!("streams:{account}");
    replace(tx, &key, "metadata", metadata)?;
    if pending.is_empty() {
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id=?1",
            [receipt_id(&key, "pending")],
        )?;
        Ok(())
    } else {
        replace(tx, &key, "pending", json!(pending))
    }
}
pub(super) fn load_streams(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
) -> Result<NotificationStreamBuffer, WorkerError> {
    let key = format!("streams:{account}");
    let mut metadata =
        receipt(tx, &key, "metadata")?.unwrap_or_else(|| json!({"entries":[],"closed":[]}));
    if let Some(pending) = receipt(tx, &key, "pending")? {
        for entry in pending.as_array().ok_or_else(|| invalid(account))? {
            let target = metadata["entries"]
                .as_array_mut()
                .ok_or_else(|| invalid(account))?
                .iter_mut()
                .find(|stored| stored["key"] == entry["key"])
                .ok_or_else(|| invalid(account))?;
            // Metadata is the durable authority for stream identity and its
            // flushed base. The pending receipt owns only the unflushed batch;
            // replacing the whole entry can erase the base hash needed to
            // rebuild text after an item/started snapshot replaces its visible
            // transcript row.
            for field in ["pending", "count", "params"] {
                target[field] = entry[field].clone();
            }
        }
    }
    serde_json::from_value(metadata).map_err(|_| invalid(account))
}
pub(super) async fn restore_streams(
    store: &RuntimeStore,
    account: &str,
) -> Result<NotificationStreamBuffer, WorkerError> {
    let account = account.to_owned();
    store
        .transaction_with(move |tx| {
            let mut streams = load_streams(tx, &account)?;
            crate::notification_dispatch::prune_inactive_streams(tx, &mut streams, now(), true)?;
            persist_streams(tx, &account, &streams)?;
            Ok(streams)
        })
        .await
}
pub(super) async fn flush_streams(
    store: Arc<RuntimeStore>,
    account: &str,
    _streams: NotificationStreamBuffer,
) -> Result<NotificationStreamBuffer, WorkerError> {
    let read_store = Arc::clone(&store);
    let publication = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_stream_snapshot",
        move || crate::notification_dispatch::PublicationContext::load(&read_store),
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)??;
    let account = account.to_owned();
    scheduler::lifecycle_transaction_with(&store, move |tx| {
        // A failed/cancelled journal job may have moved the live buffer. The
        // committed pending record, never that possibly empty cache, owns text.
        let mut streams = load_streams(tx, &account)?;
        crate::notification_dispatch::prune_inactive_streams(tx, &mut streams, now(), false)?;
        for (owner, generation, thread) in streams.threads() {
            let (_, identity) = SchedulerRepository::find_attempt_for_thread(
                tx, &owner, generation, &thread, None,
            )?;
            if let Some((identity, _)) = identity
                && let Some(next) = flush_notification_streams(
                    tx,
                    &identity.agent_id,
                    &publication,
                    &streams,
                    now(),
                )?
            {
                let mut next = next;
                let batch = next.take_post_commit()?;
                persist_publication_batch(tx, &account, &batch)?;
                streams = next;
            }
        }
        persist_streams(tx, &account, &streams)?;
        Ok(streams)
    })
    .await
}

/// Durable retry registration survives releasing an unsubmitted claim on restart.
pub(super) fn pending_preparations(
    db: &rusqlite::Connection,
    account: &str,
) -> Result<Vec<(String, i64, String, String)>, WorkerError> {
    // Python persists deferred preparation on startAttempt.prepareError and
    // clears it when preparation resumes (codex_runtime.py:7316, 7502-7505).
    let mut query = db.prepare("SELECT id FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.autoWake')=1 AND (json_type(record,'$.startAttempt.prepareError')='text' OR (json_type(record,'$.startAttempt') IS NULL AND json_type(record,'$.lastWorkspaceWait.attemptId')='text')) AND json_extract(record,'$.deletedAt') IS NULL ORDER BY id")?;
    let ids = query
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .map(|id| {
            let row = AgentRepository::get(db, &AgentId::from(id.as_str()))?
                .ok_or_else(|| invalid(&id))?;
            let reason = row
                .extra
                .get("startAttempt")
                .and_then(|attempt| attempt["prepareError"].as_str())
                .or_else(|| {
                    row.extra
                        .get("lastWorkspaceWait")
                        .and_then(|wait| wait["attemptId"].as_str())
                        .map(|_| "workspace-readiness/pending")
                })
                .ok_or_else(|| invalid(&id))?
                .to_owned();
            let provider = row
                .extra
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                .to_owned();
            Ok((id, row.epoch, reason, provider))
        })
        .collect()
}

pub(super) async fn preparation_step<T: Send + 'static>(
    store: &RuntimeStore,
    intent: &TurnIntent,
    action: impl Future<Output = Result<T, WorkerError>> + Send,
) -> Result<Option<T>, WorkerError> {
    match action.await {
        Ok(value) => Ok(Some(value)),
        Err(WorkerError::PreparationPending(reason)) => {
            let agent_id = intent.agent_id.clone();
            let attempt_id = intent.attempt_id.clone();
            let events = intent
                .events
                .iter()
                .map(|event| event.id.clone())
                .collect::<Vec<_>>();
            let epoch = intent.epoch;
            let reason = reason.clone();
            scheduler::lifecycle_transaction_with(store, move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .ok_or_else(|| invalid(&agent_id))?;
                if row.epoch == epoch
                    && row
                        .extra
                        .get("startAttempt")
                        .is_some_and(|a| a["id"] == attempt_id && a["submitted"] == false)
                    && let Some(attempt) = row
                        .extra
                        .get_mut("startAttempt")
                        .and_then(Value::as_object_mut)
                {
                    attempt.insert("prepareError".into(), json!(reason));
                    if reason == "workspace-readiness/pending" {
                        // Python's WorkspaceWaitRecord uses these fields
                        // (codex_workspace_delivery.py:43-46).
                        row.extra.insert(
                            "lastWorkspaceWait".into(),
                            json!({
                                "attemptId": attempt_id,
                                "events": events,
                                "blockers": [],
                                "at": now(),
                            }),
                        );
                    }
                    AgentRepository::put(tx, &row)?;
                }
                Ok::<_, WorkerError>(())
            })
            .await?;
            Ok(None)
        }
        Err(WorkerError::PreparationRefused(reason)) => {
            scheduler::settle_start_outcome(store, intent, TurnStart::Blocked { reason }).await?;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn hold_thread_receipt(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    generation: u64,
    reason: &str,
) -> Result<(), WorkerError> {
    hold_before_claim(tx, &context.intent.agent_id, reason)?;
    let mut agent = AgentRepository::get(tx, &AgentId::from(context.intent.agent_id.as_str()))?
        .ok_or_else(|| invalid(&context.intent.agent_id))?;
    agent.extra.insert("startOutcomeHold".into(), json!({
        "stage":"held", "reason":reason, "operationId":context.operation, "connectionGeneration":generation,
    }));
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

fn hold_before_claim(
    tx: &studio_runtime_store::Transaction<'_>,
    id: &str,
    reason: &str,
) -> Result<(), WorkerError> {
    let mut agent = AgentRepository::get(tx, &AgentId::from(id))?.ok_or_else(|| invalid(id))?;
    agent.status = studio_runtime_store::AgentStatus::Waiting;
    agent.extra.insert("nativeFailureHold".into(), json!(true));
    agent.extra.insert("autoWake".into(), json!(false));
    agent.extra.insert("inFlight".into(), json!(false));
    agent.extra.insert("error".into(), json!(reason));
    agent.extra.insert(
        "startOutcomeHold".into(),
        json!({"stage":"held","reason":reason}),
    );
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

#[cfg(test)]
mod journal_frame_payload_tests {
    use super::*;
    use serde_json::{Map, json};
    use studio_runtime_providers::{NotificationFrame, RequestFrame, UnknownMethodFrame};

    fn assert_legacy_payload_bytes(frame: &Frame) {
        let encoded = studio_runtime_providers::encode_line(frame).expect("frame encoding");
        let frame_value: Value = serde_json::from_slice(&encoded).expect("encoded frame JSON");
        let legacy_payload = json!({"frame": frame_value}).to_string();
        assert_eq!(
            journal_frame_payload(frame, "test journal").expect("journal payload"),
            legacy_payload.as_bytes()
        );
    }

    #[test]
    fn payload_bytes_match_legacy_digest_input_for_all_frame_variants() {
        let params = json!({
            "zLast": {
                "newline": "first\nsecond",
                "unicode": "café 雪 😀",
                "escaped": "quote: \\\" slash: / tab: \\t"
            },
            "aFirst": [1, -2, 1.25, 1e30, true, null]
        });
        let mut request_fields = Map::new();
        request_fields.insert("zExtension".into(), json!({"z": 2, "a": 1}));
        request_fields.insert("id".into(), json!("superseded-id"));
        request_fields.insert("method".into(), json!("superseded/method"));
        request_fields.insert("params".into(), json!({"superseded": true}));
        let request = Frame::Request(RequestFrame {
            id: RequestId::Number(u64::MAX),
            method: Method::TurnStarted,
            params: Some(params.clone()),
            fields: request_fields.clone(),
        });
        let string_request = Frame::Request(RequestFrame {
            id: RequestId::String("claude-request-id".into()),
            method: Method::TurnStarted,
            params: Some(params.clone()),
            fields: request_fields,
        });

        let mut notification_fields = Map::new();
        notification_fields.insert("zExtension".into(), json!({"z": 2, "a": 1}));
        notification_fields.insert("method".into(), json!("superseded/method"));
        let notification = Frame::Notification(NotificationFrame {
            method: Method::TurnCompleted,
            params: Some(params.clone()),
            fields: notification_fields,
        });

        let mut result_fields = Map::new();
        result_fields.insert("zExtension".into(), json!({"nested": {"z": 1, "a": 2}}));
        result_fields.insert("id".into(), json!("superseded-result-id"));
        result_fields.insert("result".into(), json!({"superseded": true}));
        let response_result = Frame::Response {
            id: RequestId::Number(9_007_199_254_740_991),
            body: ResponseBody::Result(json!({"z": params.clone(), "a": 1e-30})),
            fields: result_fields,
        };

        let mut error_fields = Map::new();
        error_fields.insert("aExtension".into(), json!("before\nafter 雪"));
        error_fields.insert("id".into(), json!(0));
        error_fields.insert("error".into(), json!({"superseded": true}));
        let response_error = Frame::Response {
            id: RequestId::Number(17),
            body: ResponseBody::Error(json!({
                "code": -32_601,
                "message": "provider \"failed\"\n雪",
                "data": {"z": 1.0, "a": -0.0}
            })),
            fields: error_fields,
        };

        let mut unknown_fields = Map::new();
        unknown_fields.insert(
            "zExtension".into(),
            json!({"last": 0.000001, "first": -0.0}),
        );
        unknown_fields.insert("method".into(), json!("future/notification"));
        unknown_fields.insert("params".into(), params);
        unknown_fields.insert("aExtension".into(), json!("雪\n😀"));
        let unknown = Frame::UnknownMethod(UnknownMethodFrame {
            method: "future/notification".into(),
            fields: unknown_fields,
        });

        assert_legacy_payload_bytes(&request);
        assert_legacy_payload_bytes(&string_request);
        assert_legacy_payload_bytes(&notification);
        assert_legacy_payload_bytes(&response_result);
        assert_legacy_payload_bytes(&response_error);
        assert_legacy_payload_bytes(&unknown);
    }
}

#[cfg(test)]
mod pending_thread_start_tests {
    use super::*;
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};

    fn receipts() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory receipts");
        connection
            .execute_batch(
                "CREATE TABLE runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL)",
            )
            .expect("receipt table");
        connection
    }

    fn insert(connection: &Connection, id: &str, result: Value) {
        connection
            .execute(
                "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,'test',?2)",
                params![id, result.to_string()],
            )
            .expect("receipt fixture");
    }

    fn operation(account: &str, method: &str) -> String {
        format!("account:{account}:attempt:{method}")
    }

    fn context(agent: &str, method: &str) -> Value {
        json!({"context":{"intent":{"agent_id":agent},"method":method}})
    }

    fn submit(connection: &Connection, account: &str, agent: &str, method: &str) {
        insert(
            connection,
            &receipt_id(&operation(account, method), "submitted"),
            context(agent, method),
        );
    }

    fn resolve(connection: &Connection, account: &str, method: &str) {
        insert(
            connection,
            &receipt_id(&operation(account, method), "resolved"),
            json!({}),
        );
    }

    /// Small mixed fixture: unrelated receipts and one submission from another agent.
    fn seed(connection: &Connection) {
        for index in 0..64 {
            insert(
                connection,
                &format!("callback:default:{index}:ack"),
                json!({"index": index}),
            );
        }
        submit(connection, "default", "other", "thread/start");
    }

    #[test]
    fn pending_lookup_uses_the_receipt_id_index() {
        let connection = receipts();
        seed(&connection);
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {PENDING_THREAD_START_SQL}"))
            .expect("plan statement");
        let details = statement
            .query_map(
                params!["lead", submitted_account_receipt_pattern()],
                |row| row.get::<_, String>(3),
            )
            .expect("plan rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan details");
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("SEARCH r USING INDEX") && d.contains("(id>? AND id<?)")),
            "{details:?}"
        );
        assert!(
            details
                .iter()
                .all(|d| !d.starts_with("SCAN") || d.contains("CONSTANT")),
            "{details:?}"
        );
    }

    #[test]
    fn unresolved_thread_start_blocks_its_agent() {
        let connection = receipts();
        submit(&connection, "default", "lead", "thread/start");
        assert!(has_pending_thread_start(&connection, "lead").expect("lookup"));
    }

    #[test]
    fn unresolved_thread_resume_blocks_its_agent() {
        let connection = receipts();
        submit(&connection, "default", "lead", "thread/resume");
        assert!(has_pending_thread_start(&connection, "lead").expect("lookup"));
    }

    #[test]
    fn resolved_start_does_not_block() {
        let connection = receipts();
        submit(&connection, "default", "lead", "thread/start");
        resolve(&connection, "default", "thread/start");
        assert!(!has_pending_thread_start(&connection, "lead").expect("lookup"));
    }

    #[test]
    fn another_agents_start_does_not_block() {
        let connection = receipts();
        submit(&connection, "default", "other", "thread/start");
        assert!(!has_pending_thread_start(&connection, "lead").expect("lookup"));
    }

    #[test]
    fn unrelated_receipts_do_not_block() {
        let connection = receipts();
        submit(&connection, "default", "lead", "turn/start");
        insert(
            &connection,
            &receipt_id(&operation("default", "thread/start"), "written"),
            context("lead", "thread/start"),
        );
        insert(
            &connection,
            "account-worker:retained:default:attempt:submitted",
            context("lead", "thread/start"),
        );
        assert!(!has_pending_thread_start(&connection, "lead").expect("lookup"));
    }

    #[test]
    fn unresolved_start_in_another_account_still_blocks() {
        let connection = receipts();
        resolve(&connection, "default", "thread/start");
        submit(&connection, "other", "lead", "thread/start");
        assert!(has_pending_thread_start(&connection, "lead").expect("lookup"));
    }
}
