//! Synchronous account transactions. Only StoreBridge calls these operations.
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

fn invalid(id: &str) -> WorkerError {
    StoreError::InvalidSyncObject {
        scope: "account_worker".into(),
        id: id.into(),
    }
    .into()
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
pub(super) fn recover(
    store: &RuntimeStore,
    account: &str,
    generation: Generation,
) -> Result<Vec<(Pending, RecoveredRequest)>, WorkerError> {
    scheduler::lifecycle_transaction_with(store, |tx| {
        SchedulerRepository::recover_unsubmitted_for_account(tx, account)?;
        SchedulerRepository::reconcile_generation(tx, account, generation.0, now())?;
        retire_generations(tx, account, generation.0)?;
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
        let mut stmt=tx.prepare("SELECT r.result FROM runtime_operation_receipts r WHERE r.id>=?1 AND r.id<?2 AND substr(r.id,-10)=':submitted' AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':resolved') ORDER BY r.id LIMIT 33")?;
        let rows = stmt
            .query_map(params![prefix, format!("{prefix}~"),], |r| {
                r.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut recovered = Vec::new();
        for raw in rows {
            let value: Value = serde_json::from_str(&raw).map_err(|_| invalid(account))?;
            let context: Pending =
                serde_json::from_value(value["context"].clone()).map_err(|_| invalid(account))?;
            if Method::from_wire(&context.method).is_none() {
                return Err(invalid(account));
            }
            let written = receipt(tx, &context.operation, "written")?;
            let request = decode(written.as_ref().unwrap_or(&value["request"]))?;
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
            if recovered.len() > MAX_PENDING_RESPONSES {
                return Err(WorkerError::Capacity);
            }
        }
        Ok(recovered)
    })
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

pub(super) fn claim(
    store: &RuntimeStore,
    account: &str,
    agent: &str,
    epoch: i64,
) -> Result<Option<TurnIntent>, WorkerError> {
    let snapshot = AgentRepository::get(&*store.read()?, &AgentId::from(agent))?;
    if snapshot.as_ref().is_none_or(|record| {
        record.epoch != epoch
            || account_key(record) != account
            || studio_runtime_store::truthy(record.extra.get("moveImportPending"))
    }) {
        return Ok(None);
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::TaskFailed)?;
    let attempt = studio_runtime_store::sha256_hex(&bytes);
    let has_radio_turn = store.read()?.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 \
         AND status='pending' AND kind='radio_turn')",
        rusqlite::params![agent, epoch],
        |row| row.get::<_, bool>(0),
    )?;
    let prepared = if has_radio_turn {
        let prepared = crate::host_project_runtime::PreparedHost::load(store)
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
        prepared
            .recheck_paths()
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
        Some(prepared)
    } else {
        None
    };
    scheduler::lifecycle_transaction_with(store, |tx| -> Result<Option<TurnIntent>, WorkerError> {
        let record = AgentRepository::get(tx, &AgentId::from(agent))?;
        if record.as_ref().is_none_or(|a| account_key(a) != account) {
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
                agent,
                "preparation/claude-frozen-input-recovery-required",
            )?;
            return Ok(None);
        }
        let preparing=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts r WHERE r.id LIKE 'account-worker:account:%:submitted' AND json_extract(r.result,'$.context.intent.agent_id')=?1 AND json_extract(r.result,'$.context.method') IN ('thread/start','thread/resume') AND NOT EXISTS(SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':resolved'))",[agent],|r|r.get::<_,bool>(0))?;
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
                .query_map(rusqlite::params![agent, epoch], |row| {
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
                    agent,
                    epoch,
                    attempt,
                    now(),
                    &event.id.0,
                )
                .map_err(Into::into);
            }
        }
        match scheduler::claim_preparation_in_transaction(tx, agent, epoch, attempt, now()) {
            Ok(intent) => Ok(intent),
            Err(scheduler::SchedulerError::PolicyRefused(reason)) => {
                hold_before_claim(tx, agent, &reason)?;
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    })
}

/// Persist a visible fail-closed state for one malformed agent without decoding
/// it through the typed repository. The original invalid field is retained for
/// repair, while this agent can no longer be selected for provider work.
pub(super) fn hold_invalid_agent_record(
    store: &RuntimeStore,
    agent_id: &str,
    diagnostic: &str,
) -> Result<(), WorkerError> {
    store.transaction(|tx| {
        let exists = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE id=?1)",
            [agent_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Ok(());
        }
        // Keep the original runtime_agents bytes untouched. Persist the hold
        // and safe, value-free diagnostic in the existing receipt table so a
        // later repair or Python reader can still recover the user's record.
        tx.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES (?1,?2,?3) \
             ON CONFLICT(id) DO UPDATE SET signature=excluded.signature,result=excluded.result",
            rusqlite::params![
                format!(
                    "{}{agent_id}",
                    studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_PREFIX
                ),
                studio_runtime_store::INVALID_AGENT_HOLD_RECEIPT_SIGNATURE,
                serde_json::to_string(&serde_json::json!({"held": true, "diagnostic": diagnostic}))
                    .map_err(StoreError::from)?
            ],
        )?;
        Ok(())
    })?;
    Ok(())
}
fn account_key(agent: &studio_runtime_store::AgentRecord) -> &str {
    agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
}

pub(super) fn submitted(
    store: &RuntimeStore,
    context: &Pending,
    account: &str,
    request: &RecoveredRequest,
    turn: Option<&str>,
    submission: Option<&PreparedSubmission>,
    guard: Option<&preparation::CommitGuard>,
    connection_id: &str,
) -> Result<bool, WorkerError> {
    let connection_id = connection_id.to_owned();
    let result = scheduler::lifecycle_transaction_with(store, |tx| {
        if receipt(tx, &context.operation, "submitted")?.is_some() {
            return Ok(false);
        }
        let i = &context.intent;
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(i.agent_id.as_str()))? else {
            return Ok(false);
        };
        if account_key(&agent) != account || i.account_key != account {
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
        if let Some(guard) = guard {
            preparation::commit(tx, context, guard)?;
        }
        let authorized = match context.method()? {
            Method::TurnStart if i.provider == "claude" => preparation::commit_claude(
                tx,
                i,
                &id_string(&request.ticket.id),
                request.ticket.generation,
                &connection_id,
                submission.ok_or_else(|| invalid(&i.attempt_id))?,
                guard.ok_or_else(|| invalid("Claude commit guard"))?,
            )?,
            Method::TurnStart => commit_prepared_turn(
                tx,
                i,
                &id_string(&request.ticket.id),
                request.ticket.generation,
                &connection_id,
                submission.ok_or_else(|| invalid(&i.attempt_id))?,
            )?,
            Method::TurnInterrupt => SchedulerRepository::prepare_interrupt(
                tx,
                &i.agent_id,
                i.thread_id.as_deref().unwrap_or(""),
                turn.unwrap_or(""),
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
            json!({"context":context,"request":record(request)}),
        )?;
        Ok(true)
    });
    match result {
        Err(WorkerError::PreparationRefused(reason)) => {
            scheduler::settle_start_outcome(store, &context.intent, TurnStart::Blocked { reason })?;
            Ok(false)
        }
        result => result,
    }
}
pub(super) fn outcome(
    store: &RuntimeStore,
    account: &str,
    resources: &preparation::PreparationResources,
    pending: Option<&Pending>,
    outcome: SessionOutcome,
) -> Result<(bool, Option<Effect>), WorkerError> {
    let Some(context) = pending else {
        if let SessionOutcome::GenerationClosed { generation } = outcome {
            scheduler::lifecycle_transaction(store, |tx| {
                SchedulerRepository::disconnect_account(tx, account, generation, now(), false)
            })?;
        }
        return Ok((false, None));
    };
    scheduler::lifecycle_transaction_with(store, |tx| {
        let i = &context.intent;
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
                uncertain(tx, context, ticket.as_ref(), not_sent)?;
                not_sent
            }
            SessionOutcome::Resolved {
                outcome: RequestOutcome::Unknown,
                ticket,
            } => {
                uncertain(tx, context, Some(&ticket), false)?;
                false
            }
            SessionOutcome::Resolved {
                ticket,
                outcome: RequestOutcome::Reply { body, .. },
            } => {
                match context.method()? {
                    Method::ThreadResume | Method::ThreadStart => {
                        if let ResponseBody::Result(ref value) = body
                            && let Some(next) = preparation::accept_thread(
                                tx,
                                store.root(),
                                resources,
                                context,
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
                                hold_thread_receipt(tx, context, ticket.generation, reason)?;
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
                                preparation::accept_turn(tx, context, &ticket, turn)?;
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
                && let Some(key) = effect_key(account, &Effect::Submit(i.clone()))
            {
                delete_effect(tx, &key)?;
            }
            forget_request(tx, &context.operation)?;
        }
        if let Some(effect) = &effect {
            persist_effect(tx, account, effect)?;
        }
        Ok((retired, effect))
    })
}
fn uncertain(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    ticket: Option<&studio_runtime_providers::RequestTicket>,
    not_sent: bool,
) -> Result<(), WorkerError> {
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
    Ok(())
}
pub(super) fn journal_batch(
    store: &RuntimeStore,
    account: &str,
    streams: &NotificationStreamBuffer,
    entries: Vec<JournalEntry>,
    claude_account_pin: &Value,
) -> Result<
    Option<(
        NotificationStreamBuffer,
        Vec<(JournalEntry, Option<Effect>)>,
        Vec<JournalEntry>,
    )>,
    WorkerError,
> {
    if entries.is_empty() {
        return Ok(Some((streams.clone(), Vec::new(), Vec::new())));
    }
    let publication = crate::notification_dispatch::PublicationContext::load(store)?;
    let limits_observation = if entries.iter().any(|entry| {
        matches!(
            &entry.payload,
            JournalPayload::Frame(Frame::Notification(frame))
                if frame.method.as_str() == "turn/completed"
        )
    }) {
        let reader = store.read()?;
        let limits = studio_runtime_store::account_state::limits_snapshots(&reader)?
            .snapshots
            .into_iter()
            .find(|(key, _)| key == account)
            .map(|(_, limits)| limits)
            .unwrap_or_else(
                || json!({"accountKey":account,"at":null,"error":null,"data":null,"stale":true}),
            );
        Some(
            crate::usage_resume::LimitsObservation::from_record(limits, None)
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?,
        )
    } else {
        None
    };
    scheduler::lifecycle_transaction_with(store, |tx| {
        let mut next = streams.clone();
        let mut committed = Vec::with_capacity(entries.len());
        let mut deferred = Vec::new();
        let mut persist_snapshot = false;
        let mut entries = entries.into_iter();
        while let Some(entry) = entries.next() {
            match journal_one(
                tx,
                account,
                &publication,
                &next,
                &entry,
                claude_account_pin,
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
            persist_streams(tx, account, &next)?;
        }
        Ok(Some((next, committed, deferred)))
    })
}

fn journal_one(
    tx: &rusqlite::Transaction<'_>,
    account: &str,
    publication: &crate::notification_dispatch::PublicationContext,
    streams: &NotificationStreamBuffer,
    entry: &JournalEntry,
    claude_account_pin: &Value,
    limits_observation: Option<&crate::usage_resume::LimitsObservation>,
) -> Result<Option<(NotificationStreamBuffer, Option<Effect>, bool)>, WorkerError> {
    let operation = format!(
        "journal:{account}:{}:{}",
        entry.generation.0, entry.cursor.0
    );
    let payload = match &entry.payload {
        JournalPayload::Frame(frame) => {
            json!({"frame": serde_json::from_slice::<Value>(&studio_runtime_providers::encode_line(frame).map_err(|_|invalid(&operation))?).map_err(|_|invalid(&operation))?})
        }
        JournalPayload::RawStdout { bytes, .. } => json!({"raw":bytes}),
        JournalPayload::Stderr(value) => json!({"stderr":value}),
        JournalPayload::Exit(value) => json!({"exit":value}),
        JournalPayload::Other { kind, payload } => json!({"kind":kind,"payload":payload}),
    };
    let digest = studio_runtime_store::sha256_hex(payload.to_string().as_bytes());
    if entry.cursor.0 <= checkpoint(tx, account, entry.generation.0)? {
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
        if let Some(("command/exec/outputDelta", params)) = notice {
            if let Some(process_id) = params
                .get("processId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                crate::monitor_event_queue::enqueue_output_in_transaction(
                    tx,
                    process_id,
                    account,
                    entry.generation.0,
                    entry.cursor.0,
                    params,
                )?;
            }
        }
        if let Some((method, params)) = notice
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
                                provider_handle: ProviderHandle::for_account(account)
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
                            &publication,
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
                                provider_handle: ProviderHandle::for_account(account)
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
                            &publication,
                            streams,
                            &ProviderDeliveryIdentity {
                                provider_handle: ProviderHandle::for_account(account)
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
                                &publication,
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
    compact(tx, account, entry.generation.0)?;
    Ok(Some((next, effect, true)))
}
pub(super) fn save_recovery(
    store: &RuntimeStore,
    account: &str,
    recovery: &studio_runtime_process::provider_seam::InitializationRecovery,
) -> Result<(), WorkerError> {
    store.transaction_with(|tx| save(tx,&format!("recovery:{account}:{}",recovery.generation.map_or(0,|g|g.0)),"required",json!({"handle":recovery.handle.as_str(),"generation":recovery.generation.map(|g|g.0),"phase":format!("{:?}",recovery.phase)})))
}
pub(super) fn stop(
    store: &RuntimeStore,
    account: &str,
    agent: &str,
) -> Result<Option<Effect>, WorkerError> {
    scheduler::lifecycle_transaction_with(store, |tx| {
        let Some(record) = AgentRepository::get(tx, &AgentId::from(agent))? else {
            return Ok(None);
        };
        if record
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != account
        {
            return Ok(None);
        }
        SchedulerRepository::stop_agent(tx, agent, "Stopped by user")?;
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(agent))? else {
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
            persist_effect(tx, account, effect)?;
        }
        Ok(effect)
    })
}
pub(super) fn interrupt_intent(
    store: &RuntimeStore,
    agent: &str,
    thread: &str,
) -> Result<Option<(TurnIntent, i64)>, WorkerError> {
    let Some(a) = AgentRepository::get(&*store.read()?, &AgentId::from(agent))? else {
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

pub(super) fn was_submitted(store: &RuntimeStore, operation: &str) -> Result<bool, WorkerError> {
    store.transaction_with(|tx| Ok(receipt(tx, operation, "submitted")?.is_some()))
}

// Written callback responses can be acknowledged on replay. A Submitted marker
// without Written is uncertainty, never permission to answer a second time.
pub(super) fn callback_admission(
    store: &RuntimeStore,
    operation: &str,
    account: &str,
    generation: u64,
    cursor: u64,
) -> Result<bool, WorkerError> {
    store.transaction_with(|tx| {
        if cursor <= checkpoint(tx, account, generation)? {
            return Ok(true);
        }
        if receipt(tx, operation, "written")? == Some(json!(true)) {
            settle_callback_in_transaction(tx, account, generation, cursor)?;
            return Ok(true);
        }
        if receipt(tx, operation, "submitted")?.is_some() {
            return Err(WorkerError::CallbackUnknown);
        }
        save(tx, operation, "submitted", json!(true))?;
        Ok(false)
    })
}
pub(super) fn callback_written(
    store: &RuntimeStore,
    operation: &str,
    outcome: WriteOutcome,
) -> Result<(), WorkerError> {
    store.transaction_with(|tx| {
        if outcome == WriteOutcome::NotSent {
            tx.execute(
                "DELETE FROM runtime_operation_receipts WHERE id=?1",
                [receipt_id(operation, "submitted")],
            )?;
            return Ok(());
        }
        save(
            tx,
            operation,
            if outcome == WriteOutcome::Written {
                "written"
            } else {
                "unknown"
            },
            json!(true),
        )
    })
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
pub(super) fn complete_effect(store: &RuntimeStore, key: &str) -> Result<(), WorkerError> {
    store.transaction_with(|tx| delete_effect(tx, key))
}
pub(super) fn recover_effects(
    store: &RuntimeStore,
    account: &str,
) -> Result<VecDeque<Effect>, WorkerError> {
    let connection = store.read()?;
    let prefix = format!("account-worker:effect:{account}:");
    let mut stmt = connection.prepare(
        "SELECT result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.order'),rowid LIMIT 33",
    )?;
    let rows = stmt
        .query_map(params![prefix, format!("{prefix}~")], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if rows.len() > MAX_PENDING_RESPONSES {
        return Err(WorkerError::Capacity);
    }
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
    account: &str,
    generation: u64,
) -> Result<u64, WorkerError> {
    Ok(
        receipt(tx, &format!("cursor:{account}:{generation}"), "ack")?
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    )
}
pub(super) fn journal_committed(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    generation: u64,
    cursor: u64,
) -> Result<bool, WorkerError> {
    let operation = format!("journal:{account}:{generation}:{cursor}");
    Ok(receipt(tx, &operation, "settled")?.is_some_and(|value| value["committed"] == true))
}
fn compact(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    generation: u64,
) -> Result<(), WorkerError> {
    let prefix = format!("account-worker:journal:{account}:{generation}:");
    let mut stmt = tx.prepare("SELECT result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.cursor') LIMIT 65")?;
    let rows = stmt
        .query_map(params![prefix, format!("{prefix}~")], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cursor = checkpoint(tx, account, generation)?;
    for raw in rows {
        let row: Value = serde_json::from_str(&raw).map_err(|_| invalid(account))?;
        let next = row["cursor"].as_u64().ok_or_else(|| invalid(account))?;
        // A missing earlier row may still be deferred before its dispatch.
        if row["ready"] != true {
            break;
        }
        cursor = next;
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id=?1",
            [receipt_id(
                &format!("journal:{account}:{generation}:{cursor}"),
                "settled",
            )],
        )?;
        for phase in ["submitted", "written", "unknown"] {
            tx.execute(
                "DELETE FROM runtime_operation_receipts WHERE id=?1",
                [receipt_id(
                    &format!("callback:{account}:{generation}:{cursor}"),
                    phase,
                )],
            )?;
        }
        let identity = json!({"providerHandle":ProviderHandle::for_account(account).as_str(),"generation":generation,"cursor":cursor});
        let signature = studio_runtime_store::sha256_hex(identity.to_string().as_bytes());
        tx.execute(
            "DELETE FROM runtime_operation_receipts WHERE id=?1",
            [format!("provider-notification:{signature}")],
        )?;
    }
    replace(
        tx,
        &format!("cursor:{account}:{generation}"),
        "ack",
        json!(cursor),
    )
}
pub(super) fn callback_settled(
    store: &RuntimeStore,
    account: &str,
    generation: u64,
    cursor: u64,
) -> Result<(), WorkerError> {
    store.transaction_with(|tx| settle_callback_in_transaction(tx, account, generation, cursor))
}
fn settle_callback_in_transaction(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    generation: u64,
    cursor: u64,
) -> Result<(), WorkerError> {
    let operation = format!("journal:{account}:{generation}:{cursor}");
    if let Some(mut row) = receipt(tx, &operation, "settled")? {
        row["ready"] = json!(true);
        replace(tx, &operation, "settled", row)?;
    }
    compact(tx, account, generation)
}

// Python codex_streaming.FLUSH_SECONDS is .75. The additional byte threshold
// bounds a durable batch when producers outpace that timer; answers are uncut.
pub(super) const STREAM_FLUSH_BYTES: usize = 64 * 1024;
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
pub(super) fn drain_publications(
    store: &RuntimeStore,
    account: &str,
    sink: &dyn crate::notification_runtime_effects::NotificationPostCommitSink,
) -> Result<usize, WorkerError> {
    drain_publications_with_hook(store, account, sink, || Ok(()))
}

pub(super) fn drain_publications_with_hook(
    store: &RuntimeStore,
    account: &str,
    sink: &dyn crate::notification_runtime_effects::NotificationPostCommitSink,
    mut before_delete: impl FnMut() -> Result<(), WorkerError>,
) -> Result<usize, WorkerError> {
    let prefix = format!("account-worker:notification-publication:{account}:");
    let pending = {
        let connection = store.read()?;
        let mut statement = connection.prepare(
            "SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2 ORDER BY json_extract(result,'$.sequence'),json_extract(result,'$.index'),rowid LIMIT ?3",
        )?;
        statement
            .query_map(
                params![prefix, format!("{prefix}~"), PUBLICATION_DRAIN_LIMIT as i64],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?
    };
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
                store,
                &id,
                analytics_overflow,
                publication_overflow,
            )
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
                sink.publish(&effect)?;
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
            sink.invalidate_state(&format!("coalesced-state-invalidation:{id}:{generation}"))?;
        }
        before_delete()?;
        // Overflow diagnostics are persisted to analytics_meta before receipt
        // deletion, matching Python's lossy analytics queue accounting.
        store.transaction_with(|tx| {
            let current: Option<String> = tx
                .query_row(
                    "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                    [&id],
                    |row| row.get(0),
                )
                .optional()?;
            if current.as_deref() == Some(result.as_str()) {
                tx.execute("DELETE FROM runtime_operation_receipts WHERE id=?1", [&id])?;
            }
            Ok::<(), WorkerError>(())
        })?;
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
pub(super) fn restore_streams(
    store: &RuntimeStore,
    account: &str,
) -> Result<NotificationStreamBuffer, WorkerError> {
    store.transaction_with(|tx| {
        let mut streams = load_streams(tx, account)?;
        crate::notification_dispatch::prune_inactive_streams(tx, &mut streams, now(), true)?;
        persist_streams(tx, account, &streams)?;
        Ok(streams)
    })
}
pub(super) fn flush_streams(
    store: &RuntimeStore,
    account: &str,
    _streams: NotificationStreamBuffer,
) -> Result<NotificationStreamBuffer, WorkerError> {
    let publication = crate::notification_dispatch::PublicationContext::load(store)?;
    scheduler::lifecycle_transaction_with(store, |tx| {
        // A failed/cancelled journal job may have moved the live buffer. The
        // committed pending record, never that possibly empty cache, owns text.
        let mut streams = load_streams(tx, account)?;
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
                persist_publication_batch(tx, account, &batch)?;
                streams = next;
            }
        }
        persist_streams(tx, account, &streams)?;
        Ok(streams)
    })
}

/// Durable retry registration survives releasing an unsubmitted claim on restart.
pub(super) fn pending_preparations(
    store: &RuntimeStore,
    account: &str,
) -> Result<Vec<(String, i64, String, String)>, WorkerError> {
    let db = store.read()?;
    // Python persists deferred preparation on startAttempt.prepareError and
    // clears it when preparation resumes (codex_runtime.py:7316, 7502-7505).
    let mut query = db.prepare("SELECT id FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.autoWake')=1 AND (json_type(record,'$.startAttempt.prepareError')='text' OR (json_type(record,'$.startAttempt') IS NULL AND json_type(record,'$.lastWorkspaceWait.attemptId')='text')) AND json_extract(record,'$.deletedAt') IS NULL ORDER BY id LIMIT 33")?;
    let ids = query
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if ids.len() > MAX_PENDING_RESPONSES {
        return Err(WorkerError::Capacity);
    }
    ids.into_iter()
        .map(|id| {
            let row = AgentRepository::get(&db, &AgentId::from(id.as_str()))?
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

pub(super) fn preparation_step<T>(
    store: &RuntimeStore,
    intent: &TurnIntent,
    action: impl FnOnce() -> Result<T, WorkerError>,
) -> Result<Option<T>, WorkerError> {
    match action() {
        Ok(value) => Ok(Some(value)),
        Err(WorkerError::PreparationPending(reason)) => {
            scheduler::lifecycle_transaction_with(store, |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(intent.agent_id.as_str()))?
                    .ok_or_else(|| invalid(&intent.agent_id))?;
                if row.epoch == intent.epoch
                    && row
                        .extra
                        .get("startAttempt")
                        .is_some_and(|a| a["id"] == intent.attempt_id && a["submitted"] == false)
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
                                "attemptId": intent.attempt_id,
                                "events": intent.events.iter().map(|event| &event.id).collect::<Vec<_>>(),
                                "blockers": [],
                                "at": now(),
                            }),
                        );
                    }
                    AgentRepository::put(tx, &row)?;
                }
                Ok::<_, WorkerError>(())
            })?;
            Ok(None)
        }
        Err(WorkerError::PreparationRefused(reason)) => {
            scheduler::settle_start_outcome(store, intent, TurnStart::Blocked { reason })?;
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
