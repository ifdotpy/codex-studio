//! Busy-turn input: queued user input reaches a running Codex or Claude turn
//! through `turn/steer`. `SchedulerRepository::claim_steer` owns the durable states and
//! their fail-point matrix; this module adapts them to the account worker.
//!
//! Exactly-once rules:
//! - The input is written to the provider at most once: the request identity is
//!   durable (`steer_submitted` plus the worker `submitted` receipt) before the
//!   write, and no path resubmits a `dispatching` or unconfirmed input.
//! - Delivery is recorded only from the provider acknowledgement, an
//!   `item/completed` userMessage whose `clientId` is the first input id. Codex
//!   app-server and the Claude bridge emit it when the turn records that exact
//!   input.
//! - A definite rejection or a turn-end report of unread input returns the input
//!   to the queue once; anything else unresolved becomes visibly unconfirmed.
use super::*;
use crate::notification_dispatch::{PreparedSubmission, SubmittedInput, record_user_submission};
use rusqlite::OptionalExtension;
use studio_runtime_store::CachedSql;

/// The steer request owns the same account-worker request identity format as
/// a turn start, with the steer id in the attempt position.
pub(super) fn operation(account: &str, steer_id: &str) -> String {
    format!(
        "account:{account}:{steer_id}:{}",
        Method::TurnSteer.as_str()
    )
}

/// Claim queued user input for the agent's running turn.
pub(super) fn claim(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    agent_id: &str,
    epoch: i64,
    steer_id: &str,
    created: f64,
) -> Result<Option<(TurnIntent, String)>, WorkerError> {
    let owned = AgentRepository::get(tx, &AgentId::from(agent_id))?.is_some_and(|agent| {
        agent
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            == account
    });
    if !owned {
        return Ok(None);
    }
    let Some(claimed) = SchedulerRepository::claim_steer(tx, agent_id, epoch, steer_id, created)?
    else {
        return Ok(None);
    };
    let field = |key: &str| claimed.agent.extra.get(key).and_then(Value::as_str);
    let intent = TurnIntent {
        agent_id: agent_id.into(),
        epoch,
        attempt_id: steer_id.into(),
        account_key: field("accountKey").unwrap_or("default").into(),
        provider: studio_runtime_providers::ProviderKind::of_field(
            claimed.agent.extra.get("provider"),
        )
        .map_err(|_| StoreError::RecordContract("agent provider"))?,
        thread_id: Some(claimed.thread_id.clone()),
        cwd: field("cwd").map(str::to_owned),
        model: field("model").map(str::to_owned),
        events: claimed
            .events
            .iter()
            .map(|event| scheduler::TurnEvent {
                id: event.id.0.clone(),
                kind: event.kind.clone(),
                text: event.text.clone(),
                created: event.created,
                epoch: event.epoch,
                error: event.error.clone(),
                metadata: claimed
                    .metadata
                    .get(&event.id.0)
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            })
            .collect(),
        radio_authorized: false,
    };
    Ok(Some((intent, claimed.turn_id)))
}

fn text<'a>(texts: impl Iterator<Item = &'a str>) -> String {
    texts.collect::<Vec<_>>().join("\n\n")
}

/// Provider `turn/steer` parameters. `expectedTurnId` makes a finished turn a
/// definite rejection instead of a new turn.
pub(super) fn params(
    intent: &TurnIntent,
    turn: &str,
    compiled: &crate::turn_prepare::PreparedSubmission,
) -> Value {
    json!({
        "threadId": intent.thread_id,
        "expectedTurnId": turn,
        "clientUserMessageId": compiled.event_ids.first(),
        "input": compiled.provider_inputs,
    })
}

/// Use the actual provider codec's named byte ceiling, including the request
/// envelope. Oversized input stays intact in the queue with an explicit error.
pub(super) fn validate_transport(
    intent: &TurnIntent,
    turn: &str,
    compiled: &crate::turn_prepare::PreparedSubmission,
) -> Result<(), WorkerError> {
    let frame = Frame::Request(studio_runtime_providers::RequestFrame {
        id: RequestId::String(operation(&intent.account_key, &intent.attempt_id)),
        method: Method::TurnSteer,
        params: Some(params(intent, turn, compiled)),
        fields: serde_json::Map::new(),
    });
    studio_runtime_providers::encode_line(&frame).map(|_| ()).map_err(|error| {
        WorkerError::PreparationRefused(format!("Busy-turn input cannot fit the provider transport: {error}. Full input remains queued."))
    })
}

/// Preparation refusal keeps the input durable for the next turn and prevents
/// repeated failed busy-turn compilation. The active turn continues normally.
pub(super) fn preparation_refused(
    tx: &studio_runtime_store::Transaction<'_>,
    intent: &TurnIntent,
    reason: &str,
    rejected: &BTreeMap<String, String>,
) -> Result<bool, WorkerError> {
    let retry = !rejected.is_empty();
    if retry {
        SchedulerRepository::steer_not_sent(tx, &intent.agent_id, &intent.attempt_id)?;
    } else {
        SchedulerRepository::steer_rejected(tx, &intent.agent_id, &intent.attempt_id)?;
    }
    if !retry {
        for event in &intent.events {
            tx.execute_cached("UPDATE runtime_events SET error=?1 WHERE id=?2 AND agent=?3 AND epoch=?4 AND status='pending'", rusqlite::params![reason,event.id,intent.agent_id,intent.epoch])?;
        }
    }
    SchedulerRepository::quarantine_pending_events(
        tx,
        &intent.agent_id,
        intent.epoch,
        rejected,
        reason,
    )?;
    Ok(retry)
}

/// Durable boundary before the steer request is written.
pub(super) fn submitted(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    request: &RecoveredRequest,
    prepared: &preparation::PreparedSteer,
) -> Result<bool, WorkerError> {
    let intent = &context.intent;
    if store::receipt(tx, &context.operation, "submitted")?.is_some() {
        return Ok(false);
    }
    if !SchedulerRepository::reconcile_obsolete_steer_before_submit(
        tx,
        &intent.agent_id,
        &intent.attempt_id,
    )? {
        return Ok(false);
    }
    if let Err(error) = preparation::commit_steer(tx, intent, prepared) {
        if let WorkerError::PreparationRefused(reason) = error {
            preparation::refuse_prepared_steer(tx, intent, prepared, &reason)?;
            return Ok(false);
        }
        return Err(error);
    }
    if !SchedulerRepository::steer_submitted(
        tx,
        &intent.agent_id,
        &intent.attempt_id,
        &store::id_string(&request.ticket.id),
        request.ticket.generation,
    )? {
        return Ok(false);
    }
    preparation::persist_compilation(tx, &prepared.compiled)?;
    SchedulerRepository::freeze_steer_submission(
        tx,
        &intent.agent_id,
        &intent.attempt_id,
        &json!(context),
    )?;
    store::save(
        tx,
        &context.operation,
        "submitted",
        json!({"context":context,"request":store::record(request)}),
    )?;
    Ok(true)
}

/// Apply a transport or reply outcome of a steer request. Returns the agent
/// and epoch to wake when the turn had already ended and the returned input
/// queued the agent again; nothing else would start its next turn.
pub(super) fn outcome(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    outcome: &SessionOutcome,
) -> Result<Option<(String, i64)>, WorkerError> {
    let (agent, steer) = (&context.intent.agent_id, &context.intent.attempt_id);
    let wake = |requeued: bool| requeued.then(|| (agent.clone(), context.intent.epoch));
    match outcome {
        SessionOutcome::Write {
            outcome: WriteOutcome::NotSent,
            ..
        } => return Ok(wake(SchedulerRepository::steer_not_sent(tx, agent, steer)?)),
        SessionOutcome::Write {
            outcome: WriteOutcome::Unknown,
            ..
        }
        | SessionOutcome::Resolved {
            outcome: RequestOutcome::Unknown,
            ..
        } => SchedulerRepository::steer_response(tx, agent, steer, "unknown")?,
        SessionOutcome::Resolved {
            outcome: RequestOutcome::Reply { body, .. },
            ..
        } => match body {
            ResponseBody::Result(_) => {
                SchedulerRepository::steer_response(tx, agent, steer, "accepted")?
            }
            ResponseBody::Error(_) => {
                return Ok(wake(SchedulerRepository::steer_rejected(tx, agent, steer)?));
            }
        },
        _ => {}
    }
    Ok(None)
}

/// Apply a provider acknowledgement. Returns the agent and epoch whose input was
/// delivered so the caller can wake it for the next queued input.
pub(super) fn acknowledge(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    method: &str,
    params: &Value,
) -> Result<Option<(String, i64)>, WorkerError> {
    let item = &params["item"];
    let (Some(thread), Some(turn), Some(first)) = (
        params["threadId"].as_str(),
        params["turnId"].as_str(),
        item["clientId"].as_str(),
    ) else {
        return Ok(None);
    };
    if method != "item/completed" || item["type"] != "userMessage" {
        return Ok(None);
    }
    let mut statement = tx.prepare_cached(
        "SELECT id FROM runtime_agents WHERE thread_id=?1 \
         AND account_key=?2 LIMIT 2",
    )?;
    let agents = statement
        .query_map(rusqlite::params![thread, account], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let [agent_id] = agents.as_slice() else {
        return Ok(None);
    };
    let Some(ack) = SchedulerRepository::acknowledge_steer(tx, agent_id, first, thread, turn)?
    else {
        return Ok(None);
    };
    let [first_event, ..] = ack.events.as_slice() else {
        return Err(StoreError::InvalidSyncObject {
            scope: "runtime_operation_receipts".into(),
            id: agent_id.clone(),
        }
        .into());
    };
    let operation = operation(account, &ack.steer_id);
    let submission = if let Some(saved) = &ack.prepared_submission {
        let context: Pending = serde_json::from_value(saved.clone())
            .map_err(|_| StoreError::RecordContract("steer submission context"))?;
        if let Some(compiled) = &context.compiled {
            let ids = ack
                .events
                .iter()
                .map(|e| e.id.0.clone())
                .collect::<Vec<_>>();
            if compiled.event_ids != ack.event_ids
                || compiled.operation_id != operation
                || compiled.actor_id != *agent_id
                || context.intent.agent_id != *agent_id
                || context.intent.thread_id.as_deref() != Some(thread)
                || context.operation != operation
                || context.method != Method::TurnSteer.as_str()
                || context
                    .intent
                    .events
                    .iter()
                    .map(|e| e.id.as_str())
                    .collect::<Vec<_>>()
                    != ack.event_ids.iter().map(String::as_str).collect::<Vec<_>>()
            {
                return Err(StoreError::RecordContract("steer acknowledged batch").into());
            }
            preparation::remember_delivered_manifests(tx, agent_id, &ids)?;
            preparation::compiled_submission(&context.intent, compiled)
        } else {
            return Err(StoreError::RecordContract("steer compiled projection").into());
        }
    } else {
        // Compatibility for already-submitted user-only steers created before
        // compiled projections were persisted. New steers always carry one.
        if ack.events.iter().any(|event| event.kind != "user") {
            return Err(StoreError::RecordContract("missing mixed steer projection").into());
        }
        legacy_submission(&ack, first_event)
    };
    if ack.prepared_submission.is_some() {
        crate::notification_dispatch::record_acknowledged_steer_submission(
            tx,
            agent_id,
            &submission,
            &ack,
        )?;
    } else {
        record_user_submission(tx, agent_id, &submission)?;
    }
    let epoch = tx
        .query_row_cached(
            "SELECT json_extract(record,'$.epoch') FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(epoch.map(|epoch| (agent_id.clone(), epoch)))
}

fn legacy_submission(
    ack: &studio_runtime_store::AcknowledgedSteer,
    first_event: &studio_runtime_store::QueueEvent,
) -> PreparedSubmission {
    PreparedSubmission {
        first_event_id: first_event.id.0.clone(),
        submitted_at: ack.submitted_at,
        created_at: first_event.created,
        text: text(ack.events.iter().map(|e| e.text.as_str())),
        inputs: ack
            .events
            .iter()
            .map(|event| SubmittedInput {
                id: event.id.0.clone(),
                at: event.created,
                kind: event.kind.clone(),
                text: event.text.clone(),
                assets: json!([]),
            })
            .collect(),
        assets: json!([]),
        radio_authorized: false,
    }
}

/// What a wake found for one agent.
pub(super) enum WakeClaim {
    /// Input claimed for the running turn, and that turn's id.
    Steer(Box<TurnIntent>, String),
    /// A running turn with nothing to steer now; its queue waits.
    Busy,
    /// Not a running turn: the ordinary turn claim applies.
    Idle,
}

/// Claim busy-turn input when the agent is a running turn. Other agents
/// return `Idle`, and a running turn with nothing claimable returns `Busy`,
/// after one read, without a write transaction.
pub(super) async fn claim_for_wake(
    store: Arc<RuntimeStore>,
    account: &str,
    agent: &str,
    epoch: i64,
) -> Result<WakeClaim, WorkerError> {
    let read_store = Arc::clone(&store);
    let read_agent = agent.to_owned();
    let read_account = account.to_owned();
    let found = read_store
        .read_async("runtime-services/account_worker_read", move |connection| {
            // An unreadable record is the ordinary claim's to hold; it is never steered.
            let Some(record) =
                AgentRepository::get(connection, &AgentId::from(read_agent.as_str()))
                    .ok()
                    .flatten()
                    .filter(|record| {
                        record.epoch == epoch
                            && record.status == studio_runtime_store::AgentStatus::Running
                            && record
                                .extra
                                .get("accountKey")
                                .and_then(Value::as_str)
                                .unwrap_or("default")
                                == read_account
                    })
            else {
                return Ok(None);
            };
            Ok::<_, WorkerError>(Some(SchedulerRepository::steer_claimable(
                connection, &record, epoch,
            )?))
        })
        .await?;
    match found {
        None => return Ok(WakeClaim::Idle),
        // A blocked or empty queue needs no write transaction.
        Some(false) => return Ok(WakeClaim::Busy),
        Some(true) => {}
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::TaskFailed)?;
    let steer_id = format!("steer-{}", studio_runtime_store::sha256_hex(&bytes));
    let (account, agent) = (account.to_owned(), agent.to_owned());
    let claimed = scheduler::lifecycle_transaction_with(&store, move |tx| {
        claim(tx, &account, &agent, epoch, &steer_id, now())
    })
    .await?;
    Ok(claimed.map_or(WakeClaim::Busy, |(intent, turn)| {
        WakeClaim::Steer(Box::new(intent), turn)
    }))
}
