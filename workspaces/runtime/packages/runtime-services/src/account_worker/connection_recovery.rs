//! Synchronous manual connection recovery, owned by the live account worker.
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
    recovery_store::{LifecycleCompletionWriter, RecoveryMessageWriter},
    task_recovery_store::TaskCompletionWriter,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};
use tokio::runtime::Handle;

pub(super) struct Command {
    pub bridge: StoreBridge,
    pub account: String,
    pub scope: ConnectionScope,
    pub session: RetainedSessionPort,
    pub agent_id: String,
    pub runtime: Handle,
}

pub(super) struct Completed {
    pub receipt: Value,
    pub streams: Option<crate::notification_dispatch::NotificationStreamBuffer>,
}

pub(super) fn run(command: Command) -> Result<Completed, RecoveryError> {
    let store = Arc::clone(&command.bridge.store);
    let before = read_agent(&store, &command.agent_id)?;
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
    let pending = begin_request(&store, &command.agent_id, &before, &recovery_id)?;
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
    let observe = move |scope: &ConnectionScope| {
        observe_live_session(&live_session, &observed_scope, &observed_value, scope)
    };
    let source: &crate::recovery_runtime::ObservationSource<'_> = &observe;
    let mut history = HistoryPort::new(
        command.session,
        RequestActor {
            agent: command.agent_id.clone(),
            epoch: snapshot.record()["epoch"]
                .as_i64()
                .ok_or(RecoveryError::CommitFailed)?,
        },
        command.runtime,
        recovery_id.clone(),
    );
    let mut reader =
        RetainedRecoveryReader::new(command.scope.clone(), thread, &mut history, source)
            .map_err(|_| RecoveryError::CommitFailed)?;
    let mut streams = super::store::restore_streams(&store, &command.account)
        .map_err(|_| RecoveryError::CommitFailed)?;
    let mut write_message: &mut RecoveryMessageWriter<'_> =
        &mut |tx, agent, account, generation, turn, item, timestamp| {
            let Some(next) = streams
                .recover_agent_message_in_transaction(
                    tx, agent, account, generation, turn, item, timestamp,
                )
                .map_err(|_| {
                    crate::notification_dispatch::NotificationDispatchError::InvalidNotification
                })?
            else {
                return Ok(None);
            };
            streams = next;
            let item_id = item["id"].as_str().ok_or(
                crate::notification_dispatch::NotificationDispatchError::InvalidNotification,
            )?;
            let turn_id = turn["id"].as_str().ok_or(
                crate::notification_dispatch::NotificationDispatchError::InvalidNotification,
            )?;
            streams.stage_recovered_notification(
                format!("connection-recovery:{recovery_id}:{turn_id}:item:{item_id}"),
                crate::notification_dispatch::RecoveredNotificationObservation {
                    agent: agent.clone(),
                    method: "item/completed".into(),
                    params: json!({"threadId":agent["threadId"],"turnId":turn_id,"item":item}),
                    observed_at: timestamp,
                },
            );
            Ok(Some(streams.clone()))
        };
    let mut delivery = ProductionRecoveryDelivery { at };
    let mut completion = LifecycleCompletionWriter {
        at,
        delivery: &mut delivery,
        write_message: &mut write_message,
        stream_update: None,
    };
    let mut task_completion = NoTaskCompletion;
    let mut runtime = RecoveryRuntime {
        store: &store,
        observation: source,
        completion: &mut completion,
        task_completion: &mut task_completion,
    };
    let job = RecoveryJob::Connection {
        snapshot: snapshot.clone(),
        automatic: false,
    };
    let outcome = match runtime.reconcile(&job, command.scope, &mut reader) {
        Ok(outcome) => outcome,
        Err(RecoveryError::History(error)) => {
            let receipt = json!({"status":"unconfirmed","error":error.to_string()});
            finish_request(&store, &command.agent_id, &recovery_id, &receipt)?;
            return Ok(Completed {
                receipt,
                streams: None,
            });
        }
        Err(error) => return Err(error),
    };
    let (receipt, needs_receipt_commit) = match outcome {
        RecoveryOutcome::Applied(value) => (value, false),
        RecoveryOutcome::Unconfirmed => (json!({"status":"unconfirmed"}), true),
        RecoveryOutcome::Superseded | RecoveryOutcome::Skipped => {
            (json!({"status":"superseded"}), true)
        }
    };
    if needs_receipt_commit {
        finish_request(&store, &command.agent_id, &recovery_id, &receipt)?;
    }
    Ok(Completed {
        receipt,
        streams: completion.stream_update.take(),
    })
}

fn read_agent(store: &RuntimeStore, id: &str) -> Result<Value, RecoveryError> {
    let db = store.read()?;
    let raw =
        AgentRepository::get_raw(&db, &AgentId::from(id))?.ok_or(RecoveryError::CommitFailed)?;
    serde_json::from_str(&raw).map_err(|_| RecoveryError::CommitFailed)
}

pub(super) fn begin_request(
    store: &RuntimeStore,
    id: &str,
    expected: &Value,
    recovery_id: &str,
) -> Result<Value, RecoveryError> {
    crate::scheduler::lifecycle_transaction_with(store, |tx| {
        let Some(raw) = AgentRepository::get_raw(tx, &AgentId::from(id))? else {
            return Err(RecoveryError::CommitFailed);
        };
        let mut current: Value =
            serde_json::from_str(&raw).map_err(|_| RecoveryError::CommitFailed)?;
        let current_id = RecoverySnapshot::from_record(current.clone())
            .map_err(|_| RecoveryError::CommitFailed)?
            .connection_recovery_id();
        if current_id != recovery_id
            || current.get("epoch") != expected.get("epoch")
            || current.get("threadId") != expected.get("threadId")
        {
            return Err(RecoveryError::CommitFailed);
        }
        if current["connectionRecovery"]["requestId"] != recovery_id {
            if !current["connectionRecovery"].is_object() {
                current["connectionRecovery"] = json!({});
            }
            current["connectionRecovery"]["requestId"] = json!(recovery_id);
            current["connectionRecovery"]["requestState"] = json!("pending");
            current["connectionRecovery"]["requestBasis"] =
                crate::recovery_policy::connection_recovery_basis(&current);
            current["connectionRecovery"]["requestReceipt"] = Value::Null;
            put_agent(tx, current.clone())?;
        }
        Ok(current)
    })
}

pub(super) fn finish_request(
    store: &RuntimeStore,
    id: &str,
    recovery_id: &str,
    receipt: &Value,
) -> Result<(), RecoveryError> {
    crate::scheduler::lifecycle_transaction_with(store, |tx| {
        let Some(raw) = AgentRepository::get_raw(tx, &AgentId::from(id))? else {
            return Err(RecoveryError::CommitFailed);
        };
        let mut agent: Value =
            serde_json::from_str(&raw).map_err(|_| RecoveryError::CommitFailed)?;
        if agent["connectionRecovery"]["requestId"] == recovery_id {
            agent["connectionRecovery"]["requestState"] = json!("result");
            agent["connectionRecovery"]["requestReceipt"] = receipt.clone();
            put_agent(tx, agent)?;
        }
        Ok(())
    })
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
    fn resolve_receipt(&mut self, _: &Value) -> Result<Value, RecoveryError> {
        Err(RecoveryError::EffectsUnavailable)
    }
    fn complete(
        &mut self,
        _: &crate::task_recovery::TaskRecoverySnapshot,
        _: &ConnectionScope,
        _: Value,
    ) -> Result<bool, RecoveryError> {
        Err(RecoveryError::EffectsUnavailable)
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
