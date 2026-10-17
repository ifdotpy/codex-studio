//! Durable claim and settlement operations owned by the account turn worker.

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, ExecutionHistoryRepository, RuntimeStore,
    SchedulerRepository, StoreError, TurnAttemptIdentity,
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnEvent {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub created: f64,
    pub epoch: i64,
    pub error: Option<String>,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TurnIntent {
    pub agent_id: String,
    pub epoch: i64,
    pub attempt_id: String,
    pub account_key: String,
    pub provider: String,
    pub thread_id: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub events: Vec<TurnEvent>,
    #[serde(default)]
    pub radio_authorized: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TurnStart {
    NotSubmitted,
    Unknown,
    Blocked { reason: String },
}

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("scheduler attempt timestamp is not finite")]
    InvalidAttemptTimestamp,
    #[error("scheduler startup recovery blocking task failed")]
    PreparationWorker,
    #[error("{0}")]
    PolicyRefused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Apply a provider-confirmed user input receipt to its exact submitted turn.
/// The provider notification receipt is the caller's durable replay key; this
/// function validates the saved attempt and settles only its exact submission.
pub(crate) fn settle_user_message_receipt_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    agent: &Value,
    event_id: &str,
    account_key: &str,
    generation: u64,
    thread_id: &str,
    turn_id: &str,
) -> Result<bool, SchedulerError> {
    let Some(agent_id) = agent.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Err(StoreError::RecordContract("agent id").into());
    };
    let attempt = agent
        .get("startAttempt")
        .filter(|attempt| !attempt.is_null())
        .or_else(|| {
            agent
                .get("disconnectRecovery")
                .and_then(|recovery| recovery.get("startAttempt"))
        })
        .unwrap_or(&Value::Null);
    let Some(epoch) = attempt.get("epoch").and_then(Value::as_i64) else {
        return Ok(false);
    };
    let attempt_events = attempt
        .get("events")
        .and_then(Value::as_array)
        .and_then(|events| {
            events
                .iter()
                .map(|event| event.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        });
    let attempt_id = attempt
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let request_ticket = attempt
        .get("requestTicket")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    if agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        != account_key
        || attempt.get("accountKey").and_then(Value::as_str) != Some(account_key)
        || attempt.get("submitted").and_then(Value::as_bool) != Some(true)
        || attempt.get("threadId").and_then(Value::as_str) != Some(thread_id)
        || agent.get("threadId").and_then(Value::as_str) != Some(thread_id)
        || attempt.get("connectionGeneration").and_then(Value::as_u64) != Some(generation)
        || !attempt_events
            .as_ref()
            .and_then(|events| events.first())
            .is_some_and(|first_event| first_event == event_id)
    {
        return Ok(false);
    }

    let Some(event) = studio_runtime_store::QueueEventRepository::get(
        transaction,
        &studio_runtime_store::QueueEventId::from(event_id),
    )?
    else {
        return Ok(false);
    };
    if event.agent.0 != agent_id
        || event.epoch != epoch
        || event
            .turn_id
            .as_deref()
            .is_some_and(|known| known != turn_id)
    {
        return Ok(false);
    }
    if !matches!(
        event.status.as_str(),
        "dispatching" | "uncertain" | "delivered"
    ) {
        return Ok(false);
    }

    if attempt_id.is_empty() || request_ticket.is_empty() {
        return Ok(false);
    }
    let accepted = SchedulerRepository::settle_started_provider(
        transaction,
        &agent_id,
        epoch,
        &attempt_id,
        &request_ticket,
        account_key,
        generation,
        thread_id,
        turn_id,
    )?;
    if accepted.is_none() {
        return Ok(false);
    }
    let Some(confirmed_event) = studio_runtime_store::QueueEventRepository::get(
        transaction,
        &studio_runtime_store::QueueEventId::from(event_id),
    )?
    else {
        return Ok(false);
    };
    if confirmed_event.status != "delivered" || confirmed_event.turn_id.as_deref() != Some(turn_id)
    {
        // Some(true) can mean the stop race won before acceptance. Preserve the
        // submitted event's uncertain status rather than claiming a delivery.
        return Ok(false);
    }

    let Some(delivered_events) = attempt_events else {
        return Ok(false);
    };
    crate::account_worker::preparation::remember_delivered_manifests(
        transaction,
        &agent_id,
        &delivered_events,
    )
    .map_err(|_| {
        SchedulerError::Store(StoreError::SchemaMismatch(
            "scheduler input receipt context manifest is invalid".into(),
        ))
    })?;
    Ok(true)
}

/// Lifecycle writes and canonical renderer publication share the writer transaction.
/// The existing agent change journal supplies exactly the changed IDs, so replay
/// neither republishes an agent nor scans unrelated stored payloads.
/// Writer diagnostics attribute the transaction to this function's caller.
#[track_caller]
pub fn lifecycle_transaction<T>(
    store: &RuntimeStore,
    change: impl for<'transaction> FnOnce(
        &rusqlite::Transaction<'transaction>,
    ) -> studio_runtime_store::StoreResult<T>
    + Send
    + 'static,
) -> impl std::future::Future<Output = studio_runtime_store::StoreResult<T>> + Send + '_
where
    T: Send + 'static,
{
    lifecycle_transaction_with(store, change)
}

#[track_caller]
pub(crate) fn lifecycle_transaction_with<T, E>(
    store: &RuntimeStore,
    change: impl for<'transaction> FnOnce(&rusqlite::Transaction<'transaction>) -> Result<T, E>
    + Send
    + 'static,
) -> impl std::future::Future<Output = Result<T, E>> + Send + '_
where
    T: Send + 'static,
    E: From<StoreError> + Send + 'static,
{
    let server_id = store.local_server_id();
    store.transaction_with(move |tx| {
        let server_id = server_id?;
        let generation = crate::sync_policy::agent_record_generation(tx)?;
        let result = change(tx)?;
        let mut changes = crate::sync_policy::EntityChanges::default();
        for (row_id, raw) in crate::sync_policy::agents_changed_since(tx, generation)? {
            let error = match AgentRepository::get(tx, &AgentId::from(row_id.as_str())) {
                Ok(Some(_)) => {
                    changes.agents.insert(row_id);
                    continue;
                }
                Ok(None) => {
                    return Err(StoreError::InvalidSyncObject {
                        scope: "runtime_agents".into(),
                        id: row_id,
                    }
                    .into());
                }
                Err(error) => error,
            };
            let parsed = serde_json::from_str::<Value>(&raw).ok();
            let Some(parsed) = parsed else {
                return Err(error.into());
            };
            let held = parsed.get("status").and_then(Value::as_str) == Some("failed")
                && parsed.get("nativeFailureHold") == Some(&Value::Bool(true))
                && parsed.get("autoWake") == Some(&Value::Bool(false));
            let is_agent_error = matches!(
                &error,
                StoreError::InvalidSyncObjectField { scope, id, .. }
                    if scope == "runtime_agents" && id == &row_id
            );
            let Some(message) = parsed.get("error").and_then(Value::as_str).filter(|_| held) else {
                return Err(error.into());
            };
            if !is_agent_error {
                return Err(error.into());
            }
            // Keep a valid, privacy-safe sync projection for the held
            // row while preserving the malformed source for repair.
            let agent = crate::sync_views::invalid_agent_view(&row_id, &parsed, message);
            let context =
                crate::sync_entity_context::agent_view_context(tx, &agent, server_id.as_deref())?;
            let projection =
                crate::sync_views::agent_entity_view(&agent, &context).ok_or_else(|| {
                    StoreError::InvalidSyncObject {
                        scope: "runtime_agents".into(),
                        id: row_id.clone(),
                    }
                })?;
            studio_runtime_store::SyncDocumentsRepository::put_entity(
                tx,
                "agent",
                &row_id,
                projection,
                crate::event_write::truthy(agent.get("deletedAt")),
            )?;
        }
        crate::sync_policy::publish_entity_changes(
            tx,
            &changes,
            server_id.as_deref(),
            &|_, _, _, _| true,
        )?;
        Ok(result)
    })
}

/// Run once during scheduler startup to release only persisted claims proven unsubmitted.
pub async fn recover_on_startup(store: &RuntimeStore) -> Result<usize, SchedulerError> {
    let registry_path = store.root().join("accounts/registry.json");
    let account_registry = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/scheduler",
        move || {
            std::fs::read(registry_path)
                .ok()
                .and_then(|bytes| crate::account_policy::decode_registry(&bytes).ok())
                .map(|(value, _)| value)
        },
    )
    .await
    .map_err(|_| SchedulerError::PreparationWorker)?;
    Ok(lifecycle_transaction(store, move |transaction| {
        let mut recovery_complete = true;
        let unsubmitted = match SchedulerRepository::recover_unsubmitted(transaction) {
            Ok(recovered) => recovered,
            Err(StoreError::InvalidSyncObject { scope, id }) if scope == "runtime_agents" => {
                eprintln!("scheduler startup skipped malformed reservation row id={id}");
                recovery_complete = false;
                0
            }
            Err(StoreError::InvalidSyncObjectField { scope, id, .. })
                if scope == "runtime_agents" =>
            {
                eprintln!("scheduler startup skipped invalid-field reservation row id={id}");
                recovery_complete = false;
                0
            }
            Err(error) => return Err(error),
        };
        let submitted = match SchedulerRepository::recover_submitted(transaction) {
            Ok(recovered) => recovered,
            Err(StoreError::InvalidSyncObject { scope, id }) if scope == "runtime_agents" => {
                eprintln!("scheduler startup skipped malformed submitted row id={id}");
                recovery_complete = false;
                0
            }
            Err(StoreError::InvalidSyncObjectField { scope, id, .. })
                if scope == "runtime_agents" =>
            {
                eprintln!("scheduler startup skipped invalid-field submitted row id={id}");
                recovery_complete = false;
                0
            }
            Err(error) => return Err(error),
        };
        if recovery_complete {
            clear_startup_agent_state(transaction, account_registry.as_ref())?;
        } else {
            eprintln!(
                "scheduler startup retained agent claims because recovery scan was incomplete"
            );
        }
        Ok(unsubmitted + submitted)
    })
    .await?)
}

/// Mirror Python's final per-agent startup cleanup after both recovery scans.
fn clear_startup_agent_state(
    transaction: &rusqlite::Transaction<'_>,
    account_registry: Option<&Value>,
) -> Result<(), StoreError> {
    let mut statement = transaction.prepare_cached(
        "SELECT id,record FROM runtime_agents \
         WHERE COALESCE(json_extract(record,'$.remoteWorker'),0)=0 \
           AND COALESCE(json_extract(record,'$.remoteAnchor'),0)=0 ORDER BY id",
    )?;
    let records = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    for (id, raw) in records {
        let mut agent: Value =
            serde_json::from_str(&raw).map_err(|_| StoreError::InvalidSyncObject {
                scope: "runtime_agents".into(),
                id: id.clone(),
            })?;
        let in_flight_changed = agent.get("inFlight") != Some(&Value::Bool(false));
        agent["inFlight"] = json!(false);
        let keep_attempt = preparation_eligible(&agent)
            || retained_auth_wait(transaction, &agent, account_registry)?;
        let removed_attempt = if keep_attempt {
            false
        } else {
            agent
                .as_object_mut()
                .is_some_and(|record| record.remove("startAttempt").is_some())
        };
        // Match Runtime.__init__: every local row is put after cleanup. Publish only
        // when this Rust pass changed a durable field.
        if in_flight_changed || removed_attempt {
            if AgentRecord::from_value(agent.clone()).is_err() {
                eprintln!("scheduler startup preserved malformed agent row id={id}");
                continue;
            }
            AgentRepository::put(transaction, &AgentRecord::from_value(agent.clone())?)?;
            crate::agents_create::publish_agent(transaction, &agent)?;
        }
    }
    Ok(())
}

/// Keep a failed, safe-to-retry preparation attempt for Python parity.
fn preparation_eligible(agent: &Value) -> bool {
    let attempt = agent.get("startAttempt").unwrap_or(&Value::Null);
    let error = agent.get("error").and_then(Value::as_str);
    let busy_claude = agent.get("provider").and_then(Value::as_str) == Some("claude")
        && crate::event_write::truthy(agent.get("threadId"))
        && error
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .is_some_and(|value| {
                value == json!({"code":-32000,"message":"Claude is still working"})
            });
    let allowed_error = error.is_some_and(|value| {
        busy_claude
            || matches!(
                value,
                "AF_UNIX path too long"
                    | "[Errno 2] No such file or directory"
                    | "Cannot verify the existing supervisor child; native outcome remains unknown"
                    | "Supervisor native launch settings changed; existing work was preserved"
                    | "Supervisor open failed: Supervisor handle exists with an incompatible or stopped child"
            )
    });
    agent.get("status").and_then(Value::as_str) == Some("failed")
        && agent.get("autoWake") == Some(&Value::Bool(true))
        && !crate::event_write::truthy(agent.get("inFlight"))
        && !crate::event_write::truthy(agent.get("turnId"))
        && !crate::event_write::truthy(agent.get("deletedAt"))
        && !crate::event_write::truthy(agent.get("nativeFailureHold"))
        && !crate::event_write::truthy(agent.get("accountTransferId"))
        && !crate::event_write::truthy(agent.get("workspaceOperation"))
        && !crate::continuation::native_thread_blocked(agent)
        && allowed_error
        && crate::event_write::truthy(attempt.get("id"))
        && attempt.get("submitted") == Some(&Value::Bool(false))
        && attempt.get("epoch") == agent.get("epoch")
        && attempt.get("accountKey").unwrap_or(&json!("default"))
            == agent.get("accountKey").unwrap_or(&json!("default"))
        && crate::event_write::truthy(attempt.get("events"))
        && !crate::event_write::truthy(attempt.get("action"))
        && !crate::event_write::truthy(attempt.get("activeAtReservation"))
        && !crate::event_write::truthy(attempt.get("turnId"))
        && !crate::event_write::truthy(attempt.get("observedTurnId"))
}

/// Retain only the durable auth-probe receipt validated by the existing policy.
fn retained_auth_wait(
    transaction: &rusqlite::Transaction<'_>,
    agent: &Value,
    account_registry: Option<&Value>,
) -> Result<bool, StoreError> {
    let attempt_id = agent
        .pointer("/startAttempt/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let Some(attempt_id) = attempt_id else {
        return Ok(false);
    };
    let agent_id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or(StoreError::RecordContract("agent id"))?;
    let receipt_id = format!("claude-auth-probe:{attempt_id}");
    let exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_usage_resumes')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let settings = preparation_settings(agent);
    let account_key = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    let receipt_exists: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_usage_resumes WHERE id=?1 AND agent=?2)",
        rusqlite::params![receipt_id, agent_id],
        |row| row.get(0),
    )?;
    if !receipt_exists {
        return Ok(false);
    }
    let Some(current_account) = account_registry
        .and_then(|registry| registry.get("accounts"))
        .and_then(|accounts| accounts.get(account_key))
    else {
        return Ok(false);
    };
    let current_pin = crate::claude_auth_wait::account_pin(current_account);
    let raw = studio_runtime_store::ContinuationRepository::snapshot(
        transaction,
        agent_id,
        studio_runtime_store::ContinuationTable::Usage,
        Some(&receipt_id),
    )?;
    let Ok(snapshot) = crate::continuation::Snapshot::from_records(raw) else {
        return Ok(false);
    };
    Ok(crate::claude_auth_wait::retained(
        &snapshot,
        &settings,
        &current_pin,
    ))
}

/// Match Runtime.preparation_settings when validating a retained auth wait receipt.
fn preparation_settings(agent: &Value) -> Value {
    let mut settings = serde_json::Map::new();
    for key in [
        "model",
        "effort",
        "nativeEffort",
        "fastMode",
        "yoloMode",
        "profileInstructions",
        "role",
    ] {
        settings.insert(key.into(), agent.get(key).cloned().unwrap_or(Value::Null));
    }
    for key in ["daybreakEnabled", "cyberAccessProgram"] {
        if let Some(value) = agent.get(key) {
            settings.insert(key.into(), value.clone());
        }
    }
    Value::Object(settings)
}

pub async fn settle_started_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    turn_id: &str,
) -> Result<Option<bool>, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(None);
    };
    let agent_id = intent.agent_id.clone();
    let epoch = intent.epoch;
    let attempt_id = intent.attempt_id.clone();
    let account_key = intent.account_key.clone();
    let request_id = request_id.to_owned();
    let thread_id = thread_id.to_owned();
    let turn_id = turn_id.to_owned();
    Ok(lifecycle_transaction(store, move |transaction| {
        SchedulerRepository::settle_started_provider(
            transaction,
            &agent_id,
            epoch,
            &attempt_id,
            &request_id,
            &account_key,
            generation,
            &thread_id,
            &turn_id,
        )
    })
    .await?)
}

pub async fn settle_start_rejected_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    error: Value,
) -> Result<bool, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    let agent_id = intent.agent_id.clone();
    let epoch = intent.epoch;
    let attempt_id = intent.attempt_id.clone();
    let account_key = intent.account_key.clone();
    let request_id = request_id.to_owned();
    let thread_id = thread_id.to_owned();
    Ok(lifecycle_transaction(store, move |transaction| {
        SchedulerRepository::settle_start_rejected_provider(
            transaction,
            &agent_id,
            epoch,
            &attempt_id,
            &request_id,
            &account_key,
            generation,
            &thread_id,
            error,
        )
    })
    .await?)
}

pub async fn settle_terminal_attempt(
    store: &RuntimeStore,
    identity: &TurnAttemptIdentity,
    params: &Value,
) -> Result<bool, SchedulerError> {
    let identity = identity.clone();
    let params = params.clone();
    Ok(lifecycle_transaction(store, move |tx| {
        settle_terminal_in_transaction(tx, &identity, &params)
    })
    .await?)
}

pub(crate) fn settle_terminal_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    identity: &TurnAttemptIdentity,
    params: &Value,
) -> studio_runtime_store::StoreResult<bool> {
    let Some(turn_id) = params["turn"]["id"].as_str() else {
        return Ok(false);
    };

    let input =
        serde_json::json!({"agent":identity.agent_id,"epoch":identity.epoch,"params":params});
    if let Some(saved) = ExecutionHistoryRepository::replay(
        transaction,
        &identity.attempt_id,
        "terminalInput",
        &input,
    )? {
        return saved
            .as_bool()
            .ok_or_else(|| StoreError::InvalidSyncObject {
                scope: "runtime_execution_attempts".into(),
                id: identity.attempt_id.clone(),
            });
    }
    let agent = AgentRepository::get(transaction, &AgentId::from(identity.agent_id.as_str()))
        .map_err(|_| StoreError::InvalidSyncObject {
            scope: "runtime_agents".into(),
            id: identity.agent_id.clone(),
        })?;
    let saved_error = agent.as_ref().and_then(|a| a.extra.get("nativeTurnError"));
    let Some((native_status, status, error)) = terminal_outcome(&params["turn"], saved_error)
    else {
        return Ok(false);
    };
    let result = SchedulerRepository::settle_terminal(
        transaction,
        &identity.agent_id,
        identity.epoch,
        &identity.attempt_id,
        turn_id,
        status.clone(),
        error.clone(),
        native_status,
        params["turn"].get("error"),
        &params["turn"]["unappliedSteers"]
            .as_array()
            .map(|ids| ids.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default(),
    )?;
    if result {
        if let Some(agent) = agent.as_ref() {
            crate::account_worker::history::terminal(
                transaction,
                agent,
                turn_id,
                match status {
                    AgentStatus::Failed => "failed",
                    AgentStatus::Interrupted => "interrupted",
                    _ => "completed",
                },
                error.as_ref(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64()),
            )?;
        }
        ExecutionHistoryRepository::save_receipt(
            transaction,
            &identity.attempt_id,
            "terminalInput",
            input,
            serde_json::json!(result),
        )?;
    }
    Ok(result)
}

// Pure native/visible classification is shared with the future asynchronous worker.
fn terminal_outcome<'a>(
    turn: &'a Value,
    previous: Option<&Value>,
) -> Option<(&'a str, AgentStatus, Option<Value>)> {
    let mut native = turn["status"].as_str()?;
    let error = turn.get("error").filter(|v| !v.is_null());
    let info = &turn["error"]["codexErrorInfo"];
    if native == "interrupted" && (info == "tooManyDenials" || info.get("tooManyDenials").is_some())
    {
        native = "failed";
    }
    let saved = previous
        .filter(|p| p["turnId"] == turn["id"])
        .and_then(|p| p.get("error"));
    let visible_error = error.or(saved).filter(|v| !v.is_null());
    let kind = visible_error.and_then(|e| e.get("codexErrorInfo"));
    let refusal = kind.is_some_and(|k| {
        k == "cyberPolicy"
            || k == "misalignmentPolicyViolation"
            || k.get("cyberPolicy").is_some()
            || k.get("misalignmentPolicyViolation").is_some()
    });
    let visible = if refusal { "failed" } else { native };
    let (status, error) = match visible {
        "completed" => (AgentStatus::Completed, None),
        "failed" => (
            AgentStatus::Failed,
            Some(visible_error.cloned().unwrap_or_else(
                || serde_json::json!({"message":"Codex ended this turn with an error."}),
            )),
        ),
        "interrupted" => (AgentStatus::Interrupted, error.cloned()),
        _ => return None,
    };
    Some((native, status, error))
}

/// The durable result can replay true; only the first successful application authorizes transport.
pub async fn authorize_submission(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    connection_id: &str,
) -> Result<bool, SchedulerError> {
    let Some(thread) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    let agent_id = intent.agent_id.clone();
    let epoch = intent.epoch;
    let attempt_id = intent.attempt_id.clone();
    let account_key = intent.account_key.clone();
    let request_id = request_id.to_owned();
    let connection_id = connection_id.to_owned();
    let thread = thread.to_owned();
    Ok(lifecycle_transaction(store, move |tx| {
        let replay = ExecutionHistoryRepository::attempt(tx, &attempt_id)?
            .is_some_and(|a| a["receipts"]["submission"].is_object());
        let result = SchedulerRepository::mark_submitted_identity(
            tx,
            &agent_id,
            epoch,
            &attempt_id,
            &request_id,
            &account_key,
            generation,
            &connection_id,
            &thread,
        )?;
        Ok(result && !replay)
    })
    .await?)
}

pub async fn mark_submitted_for_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    connection_id: &str,
) -> Result<bool, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    let agent_id = intent.agent_id.clone();
    let epoch = intent.epoch;
    let attempt_id = intent.attempt_id.clone();
    let account_key = intent.account_key.clone();
    let request_id = request_id.to_owned();
    let connection_id = connection_id.to_owned();
    let thread_id = thread_id.to_owned();
    Ok(lifecycle_transaction(store, move |transaction| {
        SchedulerRepository::mark_submitted_identity(
            transaction,
            &agent_id,
            epoch,
            &attempt_id,
            &request_id,
            &account_key,
            generation,
            &connection_id,
            &thread_id,
        )
    })
    .await?)
}

/// The account worker claims and settles its own input; the scheduler only queues wakes.
pub async fn claim_intent(
    store: &RuntimeStore,
    agent_id: &str,
    epoch: i64,
    attempt_id: &str,
    attempt_created: f64,
) -> Result<Option<TurnIntent>, SchedulerError> {
    if !attempt_created.is_finite() {
        return Err(SchedulerError::InvalidAttemptTimestamp);
    }
    let agent_id = agent_id.to_owned();
    let attempt_id = attempt_id.to_owned();
    lifecycle_transaction_with(store, move |tx| {
        claim_intent_in_transaction(tx, &agent_id, epoch, &attempt_id, attempt_created)
    })
    .await
}

pub(crate) fn claim_intent_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    attempt_id: &str,
    attempt_created: f64,
) -> Result<Option<TurnIntent>, SchedulerError> {
    claim_intent_selected(
        transaction,
        agent_id,
        epoch,
        attempt_id,
        attempt_created,
        false,
    )
}

pub(crate) fn claim_preparation_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    attempt_id: &str,
    attempt_created: f64,
) -> Result<Option<TurnIntent>, SchedulerError> {
    claim_intent_selected(
        transaction,
        agent_id,
        epoch,
        attempt_id,
        attempt_created,
        true,
    )
}

pub(crate) fn claim_radio_turn_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    attempt_id: &str,
    attempt_created: f64,
    event_id: &str,
) -> Result<Option<TurnIntent>, SchedulerError> {
    if !attempt_created.is_finite() {
        return Err(SchedulerError::InvalidAttemptTimestamp);
    }
    let claimed = SchedulerRepository::claim_selected_radio_turn(
        transaction,
        agent_id,
        epoch,
        attempt_id,
        attempt_created,
        event_id,
    )?;
    let Some(claimed) = claimed else {
        return Ok(None);
    };
    if let Some(reason) = claimed.refusal_reason {
        return Err(SchedulerError::PolicyRefused(reason));
    }
    let field = |key| claimed.agent.extra.get(key).and_then(Value::as_str);
    let intent = TurnIntent {
        agent_id: agent_id.to_owned(),
        epoch,
        attempt_id: attempt_id.to_owned(),
        account_key: field("accountKey").unwrap_or("default").to_owned(),
        provider: field("provider").unwrap_or("codex").to_owned(),
        thread_id: field("threadId").map(str::to_owned),
        cwd: field("cwd").map(str::to_owned),
        model: field("model").map(str::to_owned),
        events: claimed
            .events
            .iter()
            .map(|event| TurnEvent {
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
                    .unwrap_or_else(|| serde_json::json!({})),
            })
            .collect(),
        radio_authorized: true,
    };
    Ok(Some(intent))
}

fn claim_intent_selected(
    transaction: &rusqlite::Transaction<'_>,
    agent_id: &str,
    epoch: i64,
    attempt_id: &str,
    attempt_created: f64,
    preparation: bool,
) -> Result<Option<TurnIntent>, SchedulerError> {
    let claim = if preparation {
        SchedulerRepository::claim_for_preparation
    } else {
        SchedulerRepository::claim
    };
    let claimed = claim(
        transaction,
        agent_id,
        epoch,
        attempt_id,
        attempt_created,
        32,
    )?;
    let Some(claimed) = claimed else {
        return Ok(None);
    };
    if let Some(reason) = claimed.refusal_reason {
        return Err(SchedulerError::PolicyRefused(reason));
    }
    let field = |key| claimed.agent.extra.get(key).and_then(Value::as_str);
    let intent = TurnIntent {
        agent_id: agent_id.to_owned(),
        epoch,
        attempt_id: attempt_id.to_owned(),
        account_key: field("accountKey").unwrap_or("default").to_owned(),
        provider: field("provider").unwrap_or("codex").to_owned(),
        thread_id: field("threadId").map(str::to_owned),
        cwd: field("cwd").map(str::to_owned),
        model: field("model").map(str::to_owned),
        events: claimed
            .events
            .iter()
            .map(|event| TurnEvent {
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
                    .unwrap_or_else(|| serde_json::json!({})),
            })
            .collect(),
        radio_authorized: false,
    };
    Ok(Some(intent))
}

impl TurnIntent {
    pub fn identity(&self) -> TurnAttemptIdentity {
        TurnAttemptIdentity {
            agent_id: self.agent_id.clone(),
            epoch: self.epoch,
            attempt_id: self.attempt_id.clone(),
        }
    }
}

/// Persist definite pre-submission failures or uncertainty once, in the account worker.
pub async fn settle_start_outcome(
    store: &RuntimeStore,
    intent: &TurnIntent,
    outcome: TurnStart,
) -> Result<(), SchedulerError> {
    let identity = intent.identity();
    let status = match &outcome {
        TurnStart::Unknown => "uncertain",
        TurnStart::Blocked { .. } => "held",
        TurnStart::NotSubmitted => "pending",
    };
    let reason = match outcome {
        TurnStart::Blocked { reason } => Some(reason),
        _ => None,
    };
    Ok(lifecycle_transaction(store, move |transaction| {
        SchedulerRepository::settle_start_outcome(transaction, &identity, status, reason.as_deref())
    })
    .await?)
}

#[cfg(test)]
mod terminal_policy_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn native_denials_fail_both_outcomes_without_fabricating_other_interrupts() {
        for info in [json!("tooManyDenials"), json!({"tooManyDenials":{}})] {
            let turn = json!({"status":"interrupted","error":{"codexErrorInfo":info}});
            let (native, visible, error) = terminal_outcome(&turn, None).expect("terminal outcome");
            assert_eq!(native, "failed");
            assert_eq!(visible, AgentStatus::Failed);
            assert_eq!(error, Some(turn["error"].clone()));
        }
        assert_eq!(
            terminal_outcome(&json!({"status":"interrupted"}), None),
            Some(("interrupted", AgentStatus::Interrupted, None))
        );
        assert!(terminal_outcome(&json!({"status":"inProgress"}), None).is_none());
    }
}

#[cfg(test)]
mod invalid_record_recovery_tests {
    use super::*;

    #[tokio::test]
    async fn startup_recovery_skips_invalid_field_agent_rows_without_blocking_startup() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('bad',?1)",
                    [json!({
                        "id":"bad", "rootId":17, "epoch":1, "status":"running",
                        "autoWake":true, "inFlight":true,
                        "startAttempt":{"id":"attempt","epoch":1,"submitted":true,"events":[]}
                    })
                    .to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                     VALUES('reserved','bad','user','input','reserved',1,1,NULL,NULL)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("seed malformed reserved row");

        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('healthy',?1)",
                    [json!({
                        "id":"healthy", "rootId":"healthy", "epoch":1,
                        "status":"running", "autoWake":true, "inFlight":true,
                        "startAttempt":{"id":"live-attempt","submitted":true,
                            "connectionId":"fcd7c814-1048-4599-9b2d-4151fbc40b68"}
                    })
                    .to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("seed healthy neighboring agent");

        let recovered = recover_on_startup(&store)
            .await
            .expect("startup tolerates invalid field");

        assert_eq!(recovered, 0);
        let connection = store.read().expect("state reader opens");
        let raw: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='bad'",
                [],
                |row| row.get(0),
            )
            .expect("original malformed row remains available for repair");
        let preserved =
            serde_json::from_str::<Value>(&raw).expect("malformed fixture remains valid JSON");
        assert_eq!(preserved["rootId"], 17);
        assert_eq!(preserved["inFlight"], true);
        assert_eq!(preserved["startAttempt"]["submitted"], true);

        let healthy: String = connection
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='healthy'",
                [],
                |row| row.get(0),
            )
            .expect("healthy neighboring agent remains stored");
        let healthy =
            serde_json::from_str::<Value>(&healthy).expect("healthy fixture is valid JSON");
        assert_eq!(healthy["inFlight"], true);
        assert_eq!(healthy["startAttempt"]["id"], "live-attempt");
    }
}
