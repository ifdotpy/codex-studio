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
    #[error("{0}")]
    PolicyRefused(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Lifecycle writes and canonical renderer publication share the writer transaction.
/// The existing agent change journal supplies exactly the changed IDs, so replay
/// neither republishes an agent nor scans unrelated stored payloads.
pub fn lifecycle_transaction<T>(
    store: &RuntimeStore,
    change: impl FnOnce(&rusqlite::Transaction<'_>) -> studio_runtime_store::StoreResult<T>,
) -> studio_runtime_store::StoreResult<T> {
    lifecycle_transaction_with(store, change)
}

pub(crate) fn lifecycle_transaction_with<T, E: From<StoreError>>(
    store: &RuntimeStore,
    change: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, E>,
) -> Result<T, E> {
    let server_id = store.local_server_id()?;
    store.transaction_with(|tx| {
        let generation: i64 = tx.query_row("SELECT value FROM runtime_agent_record_generation WHERE id=1",[],|r|r.get(0)).map_err(StoreError::from)?;
        let result = change(tx)?;
        let mut statement = tx.prepare("SELECT a.id,a.record FROM runtime_agents a JOIN runtime_scheduler_agent_changes c ON c.id=a.id WHERE c.generation>?1 ORDER BY a.id").map_err(StoreError::from)?;
        let rows = statement.query_map([generation],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(StoreError::from)?.collect::<Result<Vec<_>,_>>().map_err(StoreError::from)?;
        for (id,raw) in rows {
            let agent: Value = serde_json::from_str(&raw).map_err(|_|StoreError::InvalidSyncObject {scope:"runtime_agents".into(),id:id.clone()})?;
            let context = crate::sync_entity_context::agent_view_context(tx,&agent,server_id.as_deref())?;
            let projection = crate::sync_views::agent_entity_view(&agent,&context).ok_or_else(||StoreError::InvalidSyncObject {scope:"runtime_agents".into(),id:id.clone()})?;
            studio_runtime_store::SyncDocumentsRepository::put_entity(tx,"agent",&id,projection,crate::event_write::truthy(agent.get("deletedAt")))?;
        }
        Ok(result)
    })
}

/// Run once during scheduler startup to release only persisted claims proven unsubmitted.
pub fn recover_on_startup(store: &RuntimeStore) -> Result<usize, SchedulerError> {
    let account_registry = std::fs::read(store.root().join("accounts/registry.json"))
        .ok()
        .and_then(|bytes| crate::account_policy::decode_registry(&bytes).ok())
        .map(|(value, _)| value);
    Ok(lifecycle_transaction(store, |transaction| {
        let unsubmitted = SchedulerRepository::recover_unsubmitted(transaction)?;
        let submitted = SchedulerRepository::recover_submitted(transaction)?;
        clear_startup_agent_state(transaction, account_registry.as_ref())?;
        Ok(unsubmitted + submitted)
    })?)
}

/// Mirror Python's final per-agent startup cleanup after both recovery scans.
fn clear_startup_agent_state(
    transaction: &rusqlite::Transaction<'_>,
    account_registry: Option<&Value>,
) -> Result<(), StoreError> {
    let mut statement = transaction.prepare(
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

pub fn settle_started_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    turn_id: &str,
) -> Result<Option<bool>, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(None);
    };
    Ok(lifecycle_transaction(store, |transaction| {
        SchedulerRepository::settle_started_provider(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            request_id,
            &intent.account_key,
            generation,
            thread_id,
            turn_id,
        )
    })?)
}

pub fn settle_start_rejected_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    error: Value,
) -> Result<bool, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    Ok(lifecycle_transaction(store, |transaction| {
        SchedulerRepository::settle_start_rejected_provider(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            request_id,
            &intent.account_key,
            generation,
            thread_id,
            error,
        )
    })?)
}

pub fn settle_terminal_attempt(
    store: &RuntimeStore,
    identity: &TurnAttemptIdentity,
    params: &Value,
) -> Result<bool, SchedulerError> {
    Ok(lifecycle_transaction(store, |tx| {
        settle_terminal_in_transaction(tx, identity, params)
    })?)
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
pub fn authorize_submission(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
) -> Result<bool, SchedulerError> {
    let Some(thread) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    Ok(lifecycle_transaction(store, |tx| {
        let replay = ExecutionHistoryRepository::attempt(tx, &intent.attempt_id)?
            .is_some_and(|a| a["receipts"]["submission"].is_object());
        let result = SchedulerRepository::mark_submitted_identity(
            tx,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            request_id,
            &intent.account_key,
            generation,
            thread,
        )?;
        Ok(result && !replay)
    })?)
}

pub fn mark_submitted_for_provider(
    store: &RuntimeStore,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
) -> Result<bool, SchedulerError> {
    let Some(thread_id) = intent.thread_id.as_deref() else {
        return Ok(false);
    };
    Ok(lifecycle_transaction(store, |transaction| {
        SchedulerRepository::mark_submitted_identity(
            transaction,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
            request_id,
            &intent.account_key,
            generation,
            thread_id,
        )
    })?)
}

/// The account worker claims and settles its own input; the scheduler only queues wakes.
pub fn claim_intent(
    store: &RuntimeStore,
    agent_id: &str,
    epoch: i64,
    attempt_id: &str,
    attempt_created: f64,
) -> Result<Option<TurnIntent>, SchedulerError> {
    if !attempt_created.is_finite() {
        return Err(SchedulerError::InvalidAttemptTimestamp);
    }
    lifecycle_transaction_with(store, |tx| {
        claim_intent_in_transaction(tx, agent_id, epoch, attempt_id, attempt_created)
    })
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
pub fn settle_start_outcome(
    store: &RuntimeStore,
    intent: &TurnIntent,
    outcome: TurnStart,
) -> Result<(), SchedulerError> {
    Ok(lifecycle_transaction(store, |transaction| {
        SchedulerRepository::settle_start_outcome(
            transaction,
            &intent.identity(),
            match &outcome {
                TurnStart::Unknown => "uncertain",
                TurnStart::Blocked { .. } => "held",
                TurnStart::NotSubmitted => "pending",
            },
            match &outcome {
                TurnStart::Blocked { reason } => Some(reason.as_str()),
                _ => None,
            },
        )
    })?)
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
