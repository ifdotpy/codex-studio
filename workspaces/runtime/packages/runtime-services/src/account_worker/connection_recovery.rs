//! Manual connection recovery, owned by the live account worker.
use super::{
    StoreBridge,
    retained::{RequestActor, RetainedSessionPort, consumers::HistoryPort},
};
use crate::{
    lifecycle_effects::ProductionRecoveryDelivery,
    recovery::{RecoveryError, RecoveryOutcome},
    recovery_absence::RecoveryObservation,
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
    recovery_runtime::{RecoveryJob, RecoveryRuntime, RetainedRecoveryReader},
    recovery_store::LifecycleCompletionWriter,
    task_recovery_store::TaskCompletionWriter,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};

pub(super) struct Command {
    pub bridge: StoreBridge,
    pub account: String,
    pub scope: ConnectionScope,
    pub session: RetainedSessionPort,
    pub agent_id: String,
}

pub(super) struct Completed {
    pub receipt: Value,
    pub streams: Option<crate::notification_dispatch::NotificationStreamBuffer>,
}

pub(super) async fn run(command: Command) -> Result<Completed, RecoveryError> {
    let store = Arc::clone(&command.bridge.store);
    let agent_id = AgentId::from(command.agent_id.as_str());
    let before = store
        .read_async(
            "runtime-services/account_connection_recovery_read",
            move |db| {
                let raw =
                    AgentRepository::get_raw(db, &agent_id)?.ok_or(RecoveryError::CommitFailed)?;
                serde_json::from_str::<Value>(&raw).map_err(|_| RecoveryError::CommitFailed)
            },
        )
        .await?;
    if before
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        != command.account
    {
        return Err(RecoveryError::CommitFailed);
    }
    if before["connectionRecovery"]["requestState"] == "result"
        && saved_receipt_matches_current(&before)
    {
        return Ok(Completed {
            receipt: before["connectionRecovery"]["requestReceipt"].clone(),
            streams: None,
        });
    }
    let snapshot =
        RecoverySnapshot::from_record(before.clone()).map_err(|_| RecoveryError::CommitFailed)?;
    let recovery_id = snapshot.connection_recovery_id();
    let pending = begin_request(&store, &command.agent_id, &before, &recovery_id).await?;
    let snapshot =
        RecoverySnapshot::from_record(pending).map_err(|_| RecoveryError::CommitFailed)?;
    let eligibility = snapshot.eligibility();
    let thread = snapshot.record()["threadId"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            (eligibility.preparation || eligibility.disconnected_preparation).then(String::new)
        })
        .ok_or(RecoveryError::CommitFailed)?;
    let at = unix_seconds();
    let observation = RecoveryObservation {
        scope: command.scope.clone(),
        at,
        supervisor: None,
        quiet: false,
        journal_drained: false,
        account_reserved: false,
        tools_refreshing: false,
        preparation_pending: false,
        completed_turn: None,
    };
    let observed_scope = command.scope.clone();
    let live_session = command.session.clone();
    let observed_value = observation.clone();
    let source: Arc<crate::recovery_runtime::ObservationSource> =
        Arc::new(move |scope: &ConnectionScope| {
            observe_live_session(&live_session, &observed_scope, &observed_value, scope)
        });
    let mut history = HistoryPort::new(
        command.session,
        RequestActor {
            agent: command.agent_id.clone(),
            epoch: snapshot.record()["epoch"]
                .as_i64()
                .ok_or(RecoveryError::CommitFailed)?,
        },
        recovery_id.clone(),
    );
    let mut reader =
        RetainedRecoveryReader::new(command.scope.clone(), thread, &mut history, source.as_ref())
            .map_err(|_| RecoveryError::CommitFailed)?;
    let completion =
        lifecycle_completion(&store, &command.account, recovery_id.clone(), at).await?;
    let task_completion: Arc<dyn TaskCompletionWriter> = Arc::new(NoTaskCompletion);
    let mut runtime = RecoveryRuntime {
        store: Arc::clone(&store),
        observation: Arc::clone(&source),
        completion,
        task_completion,
    };
    let job = RecoveryJob::Connection {
        snapshot: snapshot.clone(),
        automatic: false,
    };
    let result = match runtime.reconcile(&job, command.scope, &mut reader).await {
        Ok(outcome) => outcome,
        Err(RecoveryError::History(error)) => {
            let receipt = json!({"status":"unconfirmed","error":error.to_string()});
            finish_request(&store, &command.agent_id, &recovery_id, &receipt).await?;
            return Ok(Completed {
                receipt,
                streams: None,
            });
        }
        Err(error) => return Err(error),
    };
    let (receipt, needs_receipt_commit) = match result.outcome {
        RecoveryOutcome::Applied(value) => (value, false),
        RecoveryOutcome::Unconfirmed => (json!({"status":"unconfirmed"}), true),
        RecoveryOutcome::Superseded | RecoveryOutcome::Skipped => {
            (json!({"status":"superseded"}), true)
        }
    };
    if needs_receipt_commit {
        finish_request(&store, &command.agent_id, &recovery_id, &receipt).await?;
    }
    Ok(Completed {
        receipt,
        streams: result.stream_update,
    })
}

/// The canonical lifecycle writer for connection recovery: recovered final text
/// enters the account's notification streams, and parent delivery uses the
/// production lifecycle effects. Manual checks and automatic restart
/// reconciliation share it.
pub(super) async fn lifecycle_completion(
    store: &RuntimeStore,
    account: &str,
    recovery_id: String,
    at: f64,
) -> Result<Arc<std::sync::Mutex<Box<dyn crate::recovery_store::CompletionWriter>>>, RecoveryError>
{
    // This is only a cache: each message validates it under its transaction.
    let mut cached = super::store::restore_streams(store, account)
        .await
        .map_err(|_| RecoveryError::CommitFailed)?;
    let message_recovery_id = recovery_id;
    let write_message: Box<crate::recovery_store::RecoveryMessageWriter> = Box::new(
        move |tx, agent, account, generation, turn, item, timestamp| {
            let streams = super::store::current_streams(tx, account, Some(&cached))
                .map_err(stream_store_error)?;
            let Some(mut next) = streams
                .recover_agent_message_in_transaction(
                    tx, agent, account, generation, turn, item, timestamp,
                )
                .map_err(|_| crate::notification_dispatch::NotificationDispatchError::invalid())?
            else {
                return Ok(None);
            };
            let item_id = item["id"]
                .as_str()
                .ok_or(crate::notification_dispatch::NotificationDispatchError::invalid())?;
            let turn_id = turn["id"]
                .as_str()
                .ok_or(crate::notification_dispatch::NotificationDispatchError::invalid())?;
            next.stage_recovered_notification(
                format!("connection-recovery:{message_recovery_id}:{turn_id}:item:{item_id}"),
                crate::notification_dispatch::RecoveredNotificationObservation {
                    agent: agent.clone(),
                    method: "item/completed".into(),
                    params: json!({"threadId":agent["threadId"],"turnId":turn_id,"item":item}),
                    observed_at: timestamp,
                },
            );
            // Keep multiple recovered items staged in this same transaction.
            // Its revision changes atomically; rollback invalidates the cache.
            super::store::persist_streams(tx, account, &mut next).map_err(stream_store_error)?;
            cached = next.clone();
            Ok(Some(next))
        },
    );
    Ok(Arc::new(std::sync::Mutex::new(Box::new(
        LifecycleCompletionWriter {
            at,
            delivery: Box::new(ProductionRecoveryDelivery { at }),
            write_message,
        },
    ))))
}

fn stream_store_error(
    error: super::WorkerError,
) -> crate::notification_dispatch::NotificationDispatchError {
    match error {
        super::WorkerError::Store(error) => error.into(),
        _ => crate::notification_dispatch::NotificationDispatchError::invalid(),
    }
}

pub(super) async fn begin_request(
    store: &RuntimeStore,
    id: &str,
    expected: &Value,
    recovery_id: &str,
) -> Result<Value, RecoveryError> {
    let id = id.to_owned();
    let expected_epoch = expected.get("epoch").cloned();
    let expected_thread_id = expected.get("threadId").cloned();
    let recovery_id = recovery_id.to_owned();
    crate::scheduler::lifecycle_transaction_with(store, move |tx| {
        let Some(raw) = AgentRepository::get_raw(tx, &AgentId::from(id))? else {
            return Err(RecoveryError::CommitFailed);
        };
        let mut current: Value =
            serde_json::from_str(&raw).map_err(|_| RecoveryError::CommitFailed)?;
        let current_id = RecoverySnapshot::from_record(current.clone())
            .map_err(|_| RecoveryError::CommitFailed)?
            .connection_recovery_id();
        if current_id != recovery_id
            || current.get("epoch") != expected_epoch.as_ref()
            || current.get("threadId") != expected_thread_id.as_ref()
        {
            return Err(RecoveryError::CommitFailed);
        }
        if current["connectionRecovery"]["requestId"] != recovery_id.as_str() {
            if !current["connectionRecovery"].is_object() {
                current["connectionRecovery"] = json!({});
            }
            current["connectionRecovery"]["requestId"] = json!(recovery_id.as_str());
            current["connectionRecovery"]["requestState"] = json!("pending");
            current["connectionRecovery"]["requestBasis"] =
                crate::recovery_policy::connection_recovery_basis(&current);
            current["connectionRecovery"]["requestReceipt"] = Value::Null;
            put_agent(tx, current.clone())?;
        }
        Ok(current)
    })
    .await
}

pub(super) async fn finish_request(
    store: &RuntimeStore,
    id: &str,
    recovery_id: &str,
    receipt: &Value,
) -> Result<(), RecoveryError> {
    let id = id.to_owned();
    let recovery_id = recovery_id.to_owned();
    let receipt = receipt.clone();
    crate::scheduler::lifecycle_transaction_with(store, move |tx| {
        let Some(raw) = AgentRepository::get_raw(tx, &AgentId::from(id))? else {
            return Err(RecoveryError::CommitFailed);
        };
        let mut agent: Value =
            serde_json::from_str(&raw).map_err(|_| RecoveryError::CommitFailed)?;
        if agent["connectionRecovery"]["requestId"] == recovery_id.as_str() {
            agent["connectionRecovery"]["requestState"] = json!("result");
            agent["connectionRecovery"]["requestReceipt"] = receipt.clone();
            put_agent(tx, agent)?;
        }
        Ok(())
    })
    .await
}

fn put_agent(
    tx: &studio_runtime_store::Transaction<'_>,
    value: Value,
) -> Result<(), RecoveryError> {
    RecoverySnapshot::from_record(value.clone()).map_err(|_| RecoveryError::CommitFailed)?;
    let record = serde_json::from_value(value).map_err(|_| RecoveryError::CommitFailed)?;
    AgentRepository::put(tx, &record)?;
    Ok(())
}

fn saved_receipt_matches_current(agent: &Value) -> bool {
    let Some(saved) = agent["connectionRecovery"]["requestBasis"].as_array() else {
        return false;
    };
    let basis = crate::recovery_policy::connection_recovery_basis(agent);
    let Some(current) = basis.as_array() else {
        return false;
    };
    if saved.len() != 13
        || current.len() != 13
        || saved[..4] != current[..4]
        || saved[5..] != current[5..]
    {
        return false;
    }
    current[4] == saved[4]
        || (current[4].is_null() && agent["connectionRecovery"]["turnId"] == saved[4])
}

fn observe_live_session(
    session: &RetainedSessionPort,
    expected_scope: &ConnectionScope,
    observation: &RecoveryObservation,
    scope: &ConnectionScope,
) -> Option<RecoveryObservation> {
    (scope == expected_scope && session.is_live()).then(|| observation.clone())
}

struct NoTaskCompletion;
impl TaskCompletionWriter for NoTaskCompletion {
    fn resolve_receipt<'a>(
        &'a self,
        _: Arc<Value>,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, RecoveryError>> {
        Box::pin(async { Err(RecoveryError::EffectsUnavailable) })
    }
    fn complete<'a>(
        &'a self,
        _: &'a crate::task_recovery::TaskRecoverySnapshot,
        _: &'a ConnectionScope,
        _: Value,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RecoveryError>> {
        Box::pin(async { Err(RecoveryError::EffectsUnavailable) })
    }
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::{RecoveryObservation, observe_live_session, saved_receipt_matches_current};
    use crate::{account_worker::retained::RetainedSessionPort, recovery_history::ConnectionScope};
    use serde_json::json;

    #[test]
    fn replacing_worker_invalidates_the_effect_transaction_observation() {
        let scope = ConnectionScope {
            account: "default".into(),
            connection: "connection-1".into(),
            generation: 1,
        };
        let (commands, receiver) = tokio::sync::mpsc::channel(1);
        let stop = studio_runtime_process::provider_seam::Shutdown::default();
        let session = RetainedSessionPort {
            scope: scope.clone(),
            commands,
            stop: stop.clone(),
        };
        let observation = RecoveryObservation {
            scope: scope.clone(),
            at: 1.0,
            supervisor: None,
            quiet: false,
            journal_drained: false,
            account_reserved: false,
            tools_refreshing: false,
            preparation_pending: false,
            completed_turn: None,
        };
        assert!(observe_live_session(&session, &scope, &observation, &scope).is_some());
        stop.cancel(); // The old account worker was replaced while recovery was reading.
        assert!(observe_live_session(&session, &scope, &observation, &scope).is_none());
        drop(receiver);
    }

    #[test]
    fn saved_receipt_survives_its_completion_but_not_a_new_disconnect() {
        let mut agent = json!({
            "id":"agent", "epoch":2, "accountKey":"account", "threadId":"thread",
            "turnId":"turn-1", "disconnectRecovery":{"connectionId":"connection-1","at":10.0},
            "connectionRecovery":{"turnId":"turn-1","requestState":"result"}
        });
        agent["connectionRecovery"]["requestBasis"] =
            crate::recovery_policy::connection_recovery_basis(&agent);
        agent["turnId"] = serde_json::Value::Null;
        assert!(saved_receipt_matches_current(&agent));

        agent["disconnectRecovery"]["connectionId"] = json!("connection-2");
        agent["disconnectRecovery"]["at"] = json!(11.0);
        assert!(!saved_receipt_matches_current(&agent));
    }
}
