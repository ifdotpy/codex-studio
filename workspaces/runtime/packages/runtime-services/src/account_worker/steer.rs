//! Busy-turn input: queued user input and team messages reach a running Codex
//! or Claude turn through `turn/steer`. `SchedulerRepository::claim_steer` owns the durable states and
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

/// Claim queued input for the agent's running turn. A team message is
/// claimed only when the team-isolation gate admits it now, against the
/// stored rows of this transaction and the paths `resolved` before it; a
/// refused message stops the batch and waits for the next turn's full gates.
pub(super) fn claim(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    agent_id: &str,
    epoch: i64,
    steer_id: &str,
    created: f64,
    resolved: &std::collections::BTreeMap<String, String>,
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
    let admit = |event: &studio_runtime_store::QueueEvent| {
        Ok(event.kind == "user" || team_message_admitted(tx, agent_id, event, resolved))
    };
    let Some(claimed) =
        SchedulerRepository::claim_steer_admitted(tx, agent_id, epoch, steer_id, created, admit)?
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
                metadata: json!({}),
            })
            .collect(),
        radio_authorized: false,
    };
    Ok(Some((intent, claimed.turn_id)))
}

/// Whether the team-isolation gate admits `event` for `agent_id` now. Any
/// unreadable row or path refuses it, so it waits for the next turn.
fn team_message_admitted(
    tx: &studio_runtime_store::Transaction<'_>,
    agent_id: &str,
    event: &studio_runtime_store::QueueEvent,
    resolved: &std::collections::BTreeMap<String, String>,
) -> bool {
    let events = vec![crate::submit_gates::StoredEvent {
        id: event.id.0.clone(),
        kind: event.kind.clone(),
        text: event.text.clone(),
    }];
    let Ok(mut rows) = preparation::event_authorization_records(tx, agent_id, events) else {
        return false;
    };
    rows.resolved_paths = resolved.clone();
    crate::submit_gates::AuthorizationSnapshot::from_records(&rows)
        .is_ok_and(|snapshot| crate::submit_gates::authorize_events(&snapshot).is_ok())
}

/// The stored rows the gate of the agent's pending team messages reads, or
/// `None` without such a message. Their paths are resolved outside SQL.
fn team_message_records(
    db: &rusqlite::Connection,
    agent_id: &str,
    epoch: i64,
) -> Result<Option<crate::submit_gates::AuthorizationRecords>, WorkerError> {
    let mut statement = db.prepare_cached(
        "SELECT id,kind,text FROM runtime_events WHERE agent=?1 AND epoch=?2 \
         AND status='pending' ORDER BY created,rowid LIMIT ?3",
    )?;
    let events = statement
        .query_map(
            rusqlite::params![
                agent_id,
                epoch,
                studio_runtime_store::MAX_EVENTS_PER_TURN as i64
            ],
            |row| {
                Ok(crate::submit_gates::StoredEvent {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    text: row.get(2)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|event| {
            event.kind != "user"
                && studio_runtime_store::BUSY_TURN_EVENT_KINDS.contains(&event.kind.as_str())
        })
        .collect::<Vec<_>>();
    drop(statement);
    if events.is_empty() {
        return Ok(None);
    }
    preparation::event_authorization_records(db, agent_id, events).map(Some)
}

/// The one text a steer carries: its events' model texts in queue order.
fn text<'a>(events: impl Iterator<Item = (&'a str, &'a str, &'a str)>) -> String {
    events
        .map(|(id, kind, text)| crate::turn_prepare::busy_turn_event_text(id, kind, text))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Provider `turn/steer` parameters. `expectedTurnId` makes a finished turn a
/// definite rejection instead of a new turn.
pub(super) fn params(intent: &TurnIntent, turn: &str) -> Value {
    json!({
        "threadId": intent.thread_id,
        "expectedTurnId": turn,
        "clientUserMessageId": intent.events.first().map(|event| &event.id),
        "input": [{"type": "text", "text": text(intent.events.iter().map(|e| (e.id.as_str(), e.kind.as_str(), e.text.as_str())))}],
    })
}

/// Durable boundary before the steer request is written.
pub(super) fn submitted(
    tx: &studio_runtime_store::Transaction<'_>,
    context: &Pending,
    request: &RecoveredRequest,
) -> Result<bool, WorkerError> {
    let intent = &context.intent;
    if store::receipt(tx, &context.operation, "submitted")?.is_some()
        || !SchedulerRepository::steer_submitted(
            tx,
            &intent.agent_id,
            &intent.attempt_id,
            &store::id_string(&request.ticket.id),
            request.ticket.generation,
        )?
    {
        return Ok(false);
    }
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
    record_user_submission(
        tx,
        agent_id,
        &PreparedSubmission {
            first_event_id: first_event.id.0.clone(),
            submitted_at: ack.submitted_at,
            created_at: first_event.created,
            text: text(
                ack.events
                    .iter()
                    .map(|e| (e.id.0.as_str(), e.kind.as_str(), e.text.as_str())),
            ),
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
        },
    )?;
    let epoch = tx
        .query_row_cached(
            "SELECT json_extract(record,'$.epoch') FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(epoch.map(|epoch| (agent_id.clone(), epoch)))
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
            if !SchedulerRepository::steer_claimable(connection, &record, epoch)? {
                return Ok::<_, WorkerError>(Some(None));
            }
            // An unreadable team message row only keeps team messages
            // out of this steer: the claim's gate then refuses them.
            let records = team_message_records(connection, &read_agent, epoch)
                .ok()
                .flatten();
            Ok(Some(Some(records)))
        })
        .await?;
    let records = match found {
        None => return Ok(WakeClaim::Idle),
        // A blocked or empty queue needs no write transaction.
        Some(None) => return Ok(WakeClaim::Busy),
        Some(Some(records)) => records,
    };
    // Path resolution is filesystem work: it runs here, before the claim
    // transaction, as a turn preparation's does.
    let resolved = records
        .and_then(|records| preparation::authorization_paths(&records).ok())
        .unwrap_or_default();
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| WorkerError::TaskFailed)?;
    let steer_id = format!("steer-{}", studio_runtime_store::sha256_hex(&bytes));
    let (account, agent) = (account.to_owned(), agent.to_owned());
    let claimed = scheduler::lifecycle_transaction_with(&store, move |tx| {
        claim(tx, &account, &agent, epoch, &steer_id, now(), &resolved)
    })
    .await?;
    Ok(claimed.map_or(WakeClaim::Busy, |(intent, turn)| {
        WakeClaim::Steer(Box::new(intent), turn)
    }))
}
