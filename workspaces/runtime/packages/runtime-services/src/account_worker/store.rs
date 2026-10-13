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
    scheduler::lifecycle_transaction(store, |tx| {
        // Only this account is owned here. Global unsubmitted recovery belongs to
        // the server startup boundary, before any worker can claim new input.
        SchedulerRepository::reconcile_generation(tx, account, generation.0, now())
    })?;
    scheduler::lifecycle_transaction_with(store, |tx| {
        let prefix = format!("account-worker:account:{account}:");
        let mut stmt=tx.prepare("SELECT r.result FROM runtime_operation_receipts r WHERE substr(r.id,1,?1)=?2 AND substr(r.id,-10)=':submitted' AND json_extract(r.result,'$.request.generation')=?3 AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts done WHERE done.id=substr(r.id,1,length(r.id)-10)||':resolved') ORDER BY r.id LIMIT 33")?;
        let rows = stmt
            .query_map(
                params![
                    prefix.len() as i64,
                    prefix,
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
            if receipt(tx, &context.operation, "resolved")?.is_some() {
                continue;
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
            recovered.push((context, request));
            if recovered.len() > MAX_PENDING_RESPONSES {
                return Err(WorkerError::Capacity);
            }
        }
        Ok(recovered)
    })
}
pub(super) fn claim(
    store: &RuntimeStore,
    account: &str,
    agent: &str,
    epoch: i64,
) -> Result<Option<TurnIntent>, WorkerError> {
    let record = AgentRepository::get(&store.read()?, &AgentId::from(agent))?;
    if record.as_ref().is_none_or(|a| {
        a.extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != account
    }) {
        return Ok(None);
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::TaskFailed)?;
    let attempt = studio_runtime_store::sha256_hex(&bytes);
    match scheduler::claim_intent(store, agent, epoch, &attempt, now()) {
        Ok(intent) => Ok(intent),
        Err(scheduler::SchedulerError::PolicyRefused(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
pub(super) fn params(
    store: &RuntimeStore,
    intent: &TurnIntent,
    method: Method,
    turn: Option<&str>,
) -> Result<Option<(Value, Option<PreparedSubmission>)>, WorkerError> {
    if method == Method::TurnInterrupt {
        return Ok(Some((
            json!({"threadId":intent.thread_id,"turnId":turn}),
            None,
        )));
    }
    let agent = AgentRepository::get(&store.read()?, &AgentId::from(intent.agent_id.as_str()))?
        .ok_or_else(|| invalid(&intent.agent_id))?;
    match preparation::prepare_turn(preparation::PreparationInput {
        intent,
        agent: &agent,
        state_root: store.root(),
        submitted_at: now(),
    }) {
        Ok(prepared) => Ok(Some(if method == Method::ThreadResume {
            (
                json!({"threadId":intent.thread_id,"excludeTurns":true,"cwd":intent.cwd,"model":intent.model}),
                None,
            )
        } else {
            (prepared.params, Some(prepared.submission))
        })),
        Err(reason) => {
            scheduler::settle_start_outcome(store, intent, TurnStart::Blocked { reason })?;
            Ok(None)
        }
    }
}
pub(super) fn submitted(
    store: &RuntimeStore,
    context: &Pending,
    request: &RecoveredRequest,
    turn: Option<&str>,
    submission: Option<&PreparedSubmission>,
) -> Result<bool, WorkerError> {
    scheduler::lifecycle_transaction_with(store, |tx| {
        if receipt(tx, &context.operation, "submitted")?.is_some() {
            return Ok(false);
        }
        let i = &context.intent;
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(i.agent_id.as_str()))? else {
            return Ok(false);
        };
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
            return Ok(false);
        }
        save(
            tx,
            &context.operation,
            "submitted",
            json!({"context":context,"request":record(request)}),
        )?;
        Ok(true)
    })
}
pub(super) fn outcome(
    store: &RuntimeStore,
    account: &str,
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
    let i = &context.intent;
    let mut effect = None;
    let retired = match outcome {
        SessionOutcome::Write {
            outcome: WriteOutcome::Written,
            recovery: Some(request),
            ..
        } => {
            store
                .transaction_with(|tx| save(tx, &context.operation, "written", record(&request)))?;
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
            uncertain(store, context, ticket.as_ref(), not_sent)?;
            not_sent
        }
        SessionOutcome::Resolved {
            outcome: RequestOutcome::Unknown,
            ticket,
        } => {
            uncertain(store, context, Some(&ticket), false)?;
            false
        }
        SessionOutcome::Resolved {
            ticket,
            outcome: RequestOutcome::Reply { body, .. },
        } => {
            match context.method()? {
                Method::ThreadResume => {
                    if let ResponseBody::Result(ref value) = body
                        && value["thread"]["id"].as_str() == i.thread_id.as_deref()
                    {
                        effect = Some(Effect::Submit(i.clone()));
                    } else {
                        scheduler::settle_start_outcome(
                            store,
                            i,
                            TurnStart::Blocked {
                                reason: "Provider thread resume failed; input remains pending."
                                    .into(),
                            },
                        )?;
                    }
                }
                Method::TurnStart => match body {
                    ResponseBody::Result(value) => {
                        if let Some(turn) = value["turn"]["id"].as_str().filter(|s| !s.is_empty()) {
                            if scheduler::settle_started_provider(
                                store,
                                i,
                                &id_string(&ticket.id),
                                ticket.generation,
                                turn,
                            )? == Some(true)
                            {
                                effect = Some(Effect::Interrupt {
                                    agent: i.agent_id.clone(),
                                    thread: i.thread_id.clone().unwrap_or_default(),
                                    turn: turn.into(),
                                });
                            }
                        } else {
                            scheduler::settle_start_outcome(store, i, TurnStart::Unknown)?;
                            return Ok((false, None));
                        }
                    }
                    ResponseBody::Error(error) => {
                        scheduler::settle_start_rejected_provider(
                            store,
                            i,
                            &id_string(&ticket.id),
                            ticket.generation,
                            error,
                        )?;
                    }
                },
                Method::TurnInterrupt => {
                    scheduler::lifecycle_transaction(store, |tx| {
                        SchedulerRepository::settle_interrupt(
                            tx,
                            &id_string(&ticket.id),
                            ticket.generation,
                            if matches!(body, ResponseBody::Error(_)) {
                                "rejected"
                            } else {
                                "accepted"
                            },
                        )
                    })?;
                }
                _ => {}
            }
            true
        }
        _ => false,
    };
    if retired {
        scheduler::lifecycle_transaction_with(store, |tx| {
            save(tx, &context.operation, "resolved", json!(true))
        })?;
    }
    Ok((retired, effect))
}
fn uncertain(
    store: &RuntimeStore,
    context: &Pending,
    ticket: Option<&studio_runtime_providers::RequestTicket>,
    not_sent: bool,
) -> Result<(), WorkerError> {
    if context.method()? == Method::TurnInterrupt {
        let ticket = ticket.ok_or_else(|| invalid(&context.operation))?;
        scheduler::lifecycle_transaction(store, |tx| {
            SchedulerRepository::settle_interrupt(
                tx,
                &id_string(&ticket.id),
                ticket.generation,
                if not_sent { "unsent" } else { "unknown" },
            )
        })?;
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
        scheduler::settle_start_outcome(store, &context.intent, outcome)?;
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
    scheduler::lifecycle_transaction_with(store, |tx| {
        if let Some(old) = receipt(tx, &operation, "settled")? {
            if old != payload {
                return Err(invalid(&operation));
            }
            return Ok(Some((streams.clone(), None)));
        }
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
                                &store.root().to_string_lossy(),
                                streams,
                                now(),
                            )?
                            .unwrap_or_else(|| streams.clone());
                            if scheduler::settle_terminal_in_transaction(tx, &identity, params)? {
                                effect = Some(Effect::Wake(identity.agent_id, identity.epoch));
                            }
                        }
                        _ => {
                            let dispatched = dispatch_notification(
                                tx,
                                &identity.agent_id,
                                &store.root().to_string_lossy(),
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
                            next = flush_notification_streams(
                                tx,
                                &identity.agent_id,
                                &store.root().to_string_lossy(),
                                &staged,
                                now(),
                            )?
                            .unwrap_or(staged);
                        }
                    }
                }
            }
        }
        save(tx, &operation, "settled", payload)?;
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
    scheduler::lifecycle_transaction(store, |tx| {
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
        Ok(turn
            .zip(agent.extra.get("threadId").unwrap_or(&Value::Null).as_str())
            .map(|(turn, thread)| Effect::Interrupt {
                agent: agent.id.0.clone(),
                thread: thread.into(),
                turn: turn.into(),
            }))
    })
    .map_err(Into::into)
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
fn id_string(id: &RequestId) -> String {
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
) -> Result<bool, WorkerError> {
    store.transaction_with(|tx| {
        if receipt(tx, operation, "written")? == Some(json!(true)) {
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
    written: bool,
) -> Result<(), WorkerError> {
    store.transaction_with(|tx| {
        save(
            tx,
            operation,
            if written { "written" } else { "unknown" },
            json!(true),
        )
    })
}
