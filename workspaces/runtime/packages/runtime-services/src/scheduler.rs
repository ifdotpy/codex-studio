//! Durable claim and settlement operations owned by the account turn worker.

use serde_json::Value;
use studio_runtime_store::{
    AgentId, AgentRepository, AgentStatus, ExecutionHistoryRepository, RuntimeStore,
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
    store.transaction_with(|tx| {
        let generation: i64 = tx.query_row("SELECT value FROM runtime_agent_record_generation WHERE id=1",[],|r|r.get(0)).map_err(StoreError::from)?;
        let result = change(tx)?;
        let mut statement = tx.prepare("SELECT a.id,a.record FROM runtime_agents a JOIN runtime_scheduler_agent_changes c ON c.id=a.id WHERE c.generation>?1 ORDER BY a.id").map_err(StoreError::from)?;
        let rows = statement.query_map([generation],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(StoreError::from)?.collect::<Result<Vec<_>,_>>().map_err(StoreError::from)?;
        let server_id = crate::sync_entity_context::local_server_id(&store.root().to_string_lossy())?;
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
    Ok(lifecycle_transaction(store, |transaction| {
        let unsubmitted = SchedulerRepository::recover_unsubmitted(transaction)?;
        let submitted = SchedulerRepository::recover_submitted(transaction)?;
        Ok(unsubmitted + submitted)
    })?)
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
    let claimed = lifecycle_transaction(store, |transaction| {
        SchedulerRepository::claim(
            transaction,
            agent_id,
            epoch,
            attempt_id,
            attempt_created,
            32,
        )
    })?;
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
