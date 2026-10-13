//! Synchronous account transactions. Only StoreBridge calls these operations.
use super::*;
use crate::notification_dispatch::{
    PreparedSubmission, ProviderDeliveryIdentity, dispatch_notification, flush_notification_streams,
};
use rusqlite::params;
use studio_runtime_store::EventWriteRepository;

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
fn save(
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
fn record(request: &RecoveredRequest) -> Value {
    json!({"id":request.ticket.id,"generation":request.ticket.generation,"method":request.ticket.method.as_str(),"operation":request.operation_id.0,"thread":request.thread_id,"lightweight":request.lightweight,"deadline":request.deadline_ms,"unknown":request.unknown,"remoteId":request.remote_id})
}
fn decode(record: &Value) -> Result<RecoveredRequest, WorkerError> {
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
        let mut statement = tx.prepare("SELECT id FROM runtime_agents WHERE COALESCE(json_extract(record,'$.accountKey'),'default')=?1 AND json_extract(record,'$.startAttempt') IS NULL AND COALESCE(json_extract(record,'$.disconnectRecovery.startAttempt.connectionGeneration'),json_extract(record,'$.restartRecovery.startAttempt.connectionGeneration'))=?2")?;
        let ids = statement
            .query_map(
                params![
                    account,
                    i64::try_from(generation.0)
                        .map_err(|_| StoreError::SchemaMismatch("generation overflow".into()))?
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
        let mut stmt=tx.prepare("SELECT r.result FROM runtime_operation_receipts r WHERE r.id>=?1 AND r.id<?2 AND substr(r.id,-10)=':submitted' AND json_extract(r.result,'$.request.generation')=?3 AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':resolved') ORDER BY r.id LIMIT 33")?;
        let rows = stmt
            .query_map(
                params![
                    prefix,
                    format!("{prefix}~"),
                    i64::try_from(generation.0).map_err(|_| invalid(account))?
                ],
                |r| r.get::<_, String>(0),
            )?
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
            if request.ticket.generation != generation.0 {
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
                        && a.extra.get("autoWake") == Some(&json!(true))
                        && a.extra.get("startAttempt").is_none_or(Value::is_null)
                }) {
                    hold_before_claim(
                        tx,
                        &context.intent.agent_id,
                        "thread-preparation/receipt-pending",
                    )?;
                }
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
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::TaskFailed)?;
    let attempt = studio_runtime_store::sha256_hex(&bytes);
    scheduler::lifecycle_transaction_with(store, |tx| {
        let record = AgentRepository::get(tx, &AgentId::from(agent))?;
        if record.as_ref().is_none_or(|a| account_key(a) != account) {
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
                a["submitted"] == false && a["prepareError"] == "workspace-readiness/pending"
            });
        let attempt = pending.and_then(|a| a["id"].as_str()).unwrap_or(&attempt);
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
) -> Result<bool, WorkerError> {
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
            Method::TurnStart => commit_prepared_turn(
                tx,
                i,
                &id_string(&request.ticket.id),
                request.ticket.generation,
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
                            SchedulerRepository::settle_start_outcome(
                                tx,
                                &i.identity(),
                                "held",
                                Some("Provider thread resume failed; input remains pending."),
                            )?;
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
pub(super) fn journal(
    store: &RuntimeStore,
    account: &str,
    streams: &NotificationStreamBuffer,
    entry: &JournalEntry,
) -> Result<Option<(NotificationStreamBuffer, Option<Effect>)>, WorkerError> {
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
    let publication = crate::notification_dispatch::PublicationContext::load(store.root())?;
    let digest = studio_runtime_store::sha256_hex(payload.to_string().as_bytes());
    scheduler::lifecycle_transaction_with(store, |tx| {
        if entry.cursor.0 <= checkpoint(tx, account, entry.generation.0)? {
            return Ok(Some((streams.clone(), None)));
        }
        if let Some(old) = receipt(tx, &operation, "settled")? {
            if old["digest"] != digest {
                return Err(invalid(&operation));
            }
            if old["committed"] == true {
                return Ok(Some((streams.clone(), None)));
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
                if let Some((identity, bound)) = attempt {
                    match method {
                        "turn/started" => {
                            if let Some(turn) = turn
                                && SchedulerRepository::observe_started(tx, &identity, turn)?
                                    == Some(true)
                            {
                                effect = Some(Effect::Interrupt {
                                    agent: identity.agent_id,
                                    thread: thread.into(),
                                    turn: turn.into(),
                                });
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
                            if scheduler::settle_terminal_in_transaction(tx, &identity, params)? {
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
                            let dispatched = dispatch_notification(
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
                            )?;
                            let staged =
                                dispatched.stream_update.unwrap_or_else(|| streams.clone());
                            next = if staged.pending_bytes() >= STREAM_FLUSH_BYTES {
                                flush_notification_streams(
                                    tx,
                                    &identity.agent_id,
                                    &publication,
                                    &staged,
                                    now(),
                                )?
                                .unwrap_or(staged)
                            } else {
                                staged
                            };
                        }
                    }
                }
            }
        }
        if let Some(effect) = &effect {
            persist_effect(tx, account, effect)?;
        }
        persist_streams(tx, account, &next)?;
        let callback = matches!(&entry.payload, JournalPayload::Frame(frame) if super::callback::response(frame).is_some());
        replace(
            tx,
            &operation,
            "settled",
            json!({"cursor":entry.cursor.0,"digest":digest,"ready":!callback,"committed":true}),
        )?;
        compact(tx, account, entry.generation.0)?;
        Ok(Some((next, effect)))
    })
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
    let Some(a) = AgentRepository::get(&store.read()?, &AgentId::from(agent))? else {
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

fn replace(
    tx: &studio_runtime_store::Transaction<'_>,
    operation: &str,
    phase: &str,
    value: Value,
) -> Result<(), WorkerError> {
    tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET result=excluded.result", params![receipt_id(operation, phase),phase,value.to_string()])?;
    Ok(())
}
fn checkpoint(
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
fn persist_streams(
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
fn load_streams(
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
    let publication = crate::notification_dispatch::PublicationContext::load(store.root())?;
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
                streams = next;
            }
        }
        persist_streams(tx, account, &streams)?;
        Ok(streams)
    })
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
