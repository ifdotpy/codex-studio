//! Durable claim and settlement operations owned by the account turn worker.

use serde_json::Value;
use studio_runtime_store::{
    AgentStatus, RuntimeStore, SchedulerRepository, StoreError, TurnAttemptIdentity,
};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct TurnEvent {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub created: f64,
    pub epoch: i64,
    pub error: Option<String>,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq)]
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
    Accepted { turn_id: String },
    Rejected { error: Value },
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

/// Run once during scheduler startup to release only persisted claims proven unsubmitted.
pub fn recover_on_startup(store: &RuntimeStore) -> Result<usize, SchedulerError> {
    Ok(store.transaction(|transaction| {
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
    Ok(store.transaction(|transaction| {
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
    Ok(store.transaction(|transaction| {
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

pub(crate) const HISTORY_UNAVAILABLE: &str = "Rust turn history persistence is unavailable: submitted user transcript/context projection and provider answer items are not stored.";

pub fn settle_terminal_attempt(
    store: &RuntimeStore,
    identity: &TurnAttemptIdentity,
    params: &Value,
) -> Result<bool, SchedulerError> {
    let Some(turn_id) = params["turn"]["id"].as_str() else {
        return Ok(false);
    };
    let (status, error) = match params["turn"]["status"].as_str() {
        Some("completed") => (
            AgentStatus::Failed,
            Some(serde_json::json!({"message":HISTORY_UNAVAILABLE})),
        ),
        Some("failed") => (AgentStatus::Failed, params["turn"].get("error").cloned()),
        Some("interrupted") => (
            AgentStatus::Interrupted,
            params["turn"].get("error").cloned(),
        ),
        _ => return Ok(false),
    };
    Ok(store.transaction(|transaction| {
        // NOTIFICATION-DISPATCH INTEGRATION: invoke dispatch(transaction, identity, params)
        // here. Until its history writes are available, successful native turns use the
        // named HISTORY_UNAVAILABLE failure; acknowledgment follows this transaction.
        SchedulerRepository::settle_terminal(
            transaction,
            &identity.agent_id,
            identity.epoch,
            &identity.attempt_id,
            turn_id,
            status,
            error,
        )
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
    Ok(store.transaction(|transaction| {
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
    let claimed = store.transaction(|transaction| {
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
    Ok(store.transaction(|transaction| {
        SchedulerRepository::settle_start_outcome(
            transaction,
            &intent.identity(),
            match &outcome {
                TurnStart::Unknown => "uncertain",
                TurnStart::Blocked { .. } => "held",
                TurnStart::NotSubmitted => "pending",
                TurnStart::Accepted { .. } | TurnStart::Rejected { .. } => return Ok(()),
            },
            match &outcome {
                TurnStart::Blocked { reason } => Some(reason.as_str()),
                _ => None,
            },
        )
    })?)
}
