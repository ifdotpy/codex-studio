//! First-pass account-worker recovery for a restart-held input receipt.
use super::{AccountCommand, StoreBridge};
use crate::{
    account_worker::retained::{RequestActor, consumers::HistoryPort},
    recovery::{RecoveryAction, RecoveryEffectResult, RecoveryError, RecoveryOutcome},
    recovery_absence::RecoveryObservation,
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
    recovery_runtime::{
        RecoveryCursor, RecoveryJob, RecoveryRuntime, RetainedRecoveryReader, load_recovery_jobs,
    },
    recovery_store::CompletionWriter,
    task_recovery::TaskRecoverySnapshot,
    task_recovery_store::TaskCompletionWriter,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, SchedulerRepository};

const MAX_STARTUP_SCAN_PAGES: usize = 32;

pub(super) struct FirstPass {
    pub(super) bridge: StoreBridge,
    pub(super) account: String,
    pub(super) scope: ConnectionScope,
    pub(super) session: super::retained::RetainedSessionPort,
    pub(super) commands: tokio::sync::mpsc::Sender<AccountCommand>,
}

struct FirstPassResult {
    ready: Vec<(String, i64)>,
    streams: Option<crate::notification_dispatch::NotificationStreamBuffer>,
}

pub(super) fn start_first_pass(first_pass: FirstPass) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let store = first_pass.bridge.store.clone();
        let request_scope = first_pass.scope.clone();
        if let Ok(result) = reconcile_first_pass(
            store,
            &first_pass.account,
            request_scope,
            first_pass.session,
        )
        .await
        {
            if let Some(streams) = result.streams
                && first_pass
                    .commands
                    .send(AccountCommand::RestartStreams(Box::new(streams)))
                    .await
                    .is_err()
            {
                return;
            }
            for (agent_id, epoch) in result.ready {
                if first_pass
                    .commands
                    .send(AccountCommand::Wake { agent_id, epoch })
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        let _result = first_pass
            .commands
            .send(AccountCommand::RestartRecoveryDone)
            .await;
    })
}

async fn reconcile_first_pass(
    store: Arc<RuntimeStore>,
    account: &str,
    scope: ConnectionScope,
    session: super::retained::RetainedSessionPort,
) -> Result<FirstPassResult, RecoveryError> {
    let mut cursor = RecoveryCursor::default();
    let mut recovered = Vec::new();
    let mut recovered_streams = None;
    for _ in 0..MAX_STARTUP_SCAN_PAGES {
        let jobs = load_recovery_jobs_blocking(Arc::clone(&store), account, &mut cursor).await?;
        if jobs.is_empty() {
            break;
        }
        for job in jobs {
            let RecoveryJob::Turn(snapshot) = &job else {
                continue;
            };
            if snapshot.record()["restartRecovery"]["stage"] != "held"
                || snapshot.record()["contextRepairWait"].is_null()
                || !snapshot.eligibility().unconfirmed_start
            {
                continue;
            }
            let agent = snapshot.record()["id"]
                .as_str()
                .ok_or(RecoveryError::CommitFailed)?
                .to_owned();
            let epoch = snapshot.record()["epoch"]
                .as_i64()
                .ok_or(RecoveryError::CommitFailed)?;
            let thread = snapshot.record()["threadId"]
                .as_str()
                .ok_or(RecoveryError::CommitFailed)?
                .to_owned();
            let observation = RecoveryObservation {
                scope: scope.clone(),
                at: unix_seconds(),
                supervisor: None,
                quiet: false,
                journal_drained: false,
                account_reserved: true,
                tools_refreshing: false,
                preparation_pending: false,
                completed_turn: None,
            };
            let observed_scope = scope.clone();
            let observed_value = observation.clone();
            let source: Arc<crate::recovery_runtime::ObservationSource> =
                Arc::new(move |current: &ConnectionScope| {
                    (current == &observed_scope).then(|| observed_value.clone())
                });
            let mut history = HistoryPort::new(
                session.clone(),
                RequestActor {
                    agent: agent.clone(),
                    epoch,
                },
                snapshot.connection_recovery_id(),
            );
            let mut reader =
                RetainedRecoveryReader::new(scope.clone(), thread, &mut history, source.as_ref())?;
            let completion: Arc<Mutex<Box<dyn CompletionWriter>>> =
                Arc::new(Mutex::new(Box::new(RestartInputCompletion::default())));
            let task_completion: Arc<dyn TaskCompletionWriter> = Arc::new(NoTaskCompletion);
            let mut runtime = RecoveryRuntime {
                store: Arc::clone(&store),
                observation: Arc::clone(&source),
                completion,
                task_completion,
            };
            let mut scan = crate::recovery_runtime::RecoveryScan::default();
            let selected = scan
                .take([job.clone()], std::time::Instant::now(), observation.at)
                .ok_or(RecoveryError::EffectsUnavailable)?;
            let outcome = runtime
                .reconcile(&selected, scope.clone(), &mut reader)
                .await;
            scan.finish(&selected, std::time::Instant::now(), &outcome)?;
            let outcome = outcome?;
            if matches!(outcome.outcome, RecoveryOutcome::Applied(_)) {
                recovered.push((agent, epoch));
                if let Some(streams) = outcome.stream_update {
                    recovered_streams = Some(streams);
                }
            }
        }
    }
    Ok(FirstPassResult {
        ready: recovered,
        streams: recovered_streams,
    })
}

async fn load_recovery_jobs_blocking(
    store: Arc<RuntimeStore>,
    account: &str,
    cursor: &mut RecoveryCursor,
) -> Result<Vec<RecoveryJob>, RecoveryError> {
    let account = account.to_owned();
    let mut page_cursor = std::mem::take(cursor);
    let (jobs, updated_cursor) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/restart_recovery_scan",
        move || {
            let jobs = load_recovery_jobs(&store, &account, &mut page_cursor)?;
            Ok::<_, RecoveryError>((jobs, page_cursor))
        },
    )
    .await
    .map_err(|_| RecoveryError::CommitFailed)??;
    *cursor = updated_cursor;
    Ok(jobs)
}

#[derive(Default)]
pub(super) struct RestartInputCompletion {}
impl CompletionWriter for RestartInputCompletion {
    fn start_held(
        &mut self,
        _: &studio_runtime_store::Transaction<'_>,
        _: &Value,
        _: &str,
        _: &str,
    ) -> Result<(), RecoveryError> {
        Err(RecoveryError::EffectsUnavailable)
    }

    fn complete(
        &mut self,
        tx: &studio_runtime_store::Transaction<'_>,
        expected: &RecoverySnapshot,
        scope: &ConnectionScope,
        action: &RecoveryAction,
    ) -> Result<RecoveryEffectResult, RecoveryError> {
        let mut stream_update = None;
        let RecoveryAction::AcceptStart { turn } = action else {
            return Err(RecoveryError::EffectsUnavailable);
        };
        let record = expected.record();
        let agent_id = record["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
        let epoch = record["epoch"]
            .as_i64()
            .ok_or(RecoveryError::CommitFailed)?;
        let attempt = expected.start_attempt();
        let attempt_id = attempt["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
        let turn_id = turn["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
        if record["accountKey"].as_str().unwrap_or("default") != scope.account {
            return Ok(RecoveryEffectResult::plain(RecoveryOutcome::Superseded));
        }
        let accepted = SchedulerRepository::settle_restart_started(
            tx,
            agent_id,
            epoch,
            attempt,
            &record["contextRepairWait"],
            turn_id,
        )?;
        if !accepted {
            return Ok(RecoveryEffectResult::plain(RecoveryOutcome::Superseded));
        }
        let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(agent_id))? else {
            return Err(RecoveryError::CommitFailed);
        };
        agent.extra.remove("contextRepairWait");
        agent.extra.remove("nativeFailureHold");
        agent.extra.remove("startOutcomeHold");
        agent.extra.insert("autoWake".into(), json!(true));
        agent.extra.insert("error".into(), Value::Null);
        if let Some(marker) = agent.extra.get_mut("restartRecovery") {
            marker["stage"] = json!("finished");
            marker["reconciledAt"] = json!(unix_seconds());
        }
        AgentRepository::put(tx, &agent)?;
        let native_status = turn["status"].as_str().ok_or(RecoveryError::CommitFailed)?;
        let terminal = match native_status {
            "completed" => Some(studio_runtime_store::AgentStatus::Completed),
            "failed" => Some(studio_runtime_store::AgentStatus::Failed),
            "interrupted" => Some(studio_runtime_store::AgentStatus::Interrupted),
            _ => None,
        };
        if let Some(status) = terminal {
            let items = turn["items"]
                .as_array()
                .ok_or(RecoveryError::CommitFailed)?;
            let mut agent_value = agent.value();
            let mut streams = super::store::load_streams(tx, &scope.account)
                .map_err(|_| RecoveryError::CommitFailed)?;
            for item in items.iter().filter(|item| item["type"] == "agentMessage") {
                let item_native_id = item["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
                let item_id = format!("{agent_id}:{item_native_id}");
                let body = item["text"].as_str().unwrap_or_default();
                let previous =
                    studio_runtime_store::NotificationDispatchRepository::transcript_item(
                        tx, agent_id, &item_id,
                    )?;
                if previous.as_ref().is_some_and(|(record, _)| {
                    record["streaming"] != true && record["text"].as_str() == Some(body)
                }) {
                    continue;
                }
                let next = streams
                    .recover_agent_message_in_transaction(
                        tx,
                        &mut agent_value,
                        &scope.account,
                        scope.generation,
                        turn,
                        item,
                        unix_seconds(),
                    )
                    .map_err(|_| RecoveryError::CommitFailed)?;
                if let Some(next) = next {
                    streams = next;
                }
                let params = json!({
                    "threadId":agent_value["threadId"],
                    "turnId":turn_id,
                    "item":item
                });
                streams.stage_recovered_notification(
                    format!("restart:{attempt_id}:{turn_id}:item:{item_native_id}"),
                    crate::notification_dispatch::RecoveredNotificationObservation {
                        agent: agent_value.clone(),
                        method: "item/completed".into(),
                        params,
                        observed_at: unix_seconds(),
                    },
                );
            }
            AgentRepository::put(
                tx,
                &studio_runtime_store::AgentRecord::from_value(agent_value)
                    .map_err(|_| RecoveryError::CommitFailed)?,
            )?;
            let error = turn.get("error").filter(|v| !v.is_null()).cloned();
            let settled = SchedulerRepository::settle_terminal(
                tx,
                agent_id,
                epoch,
                attempt_id,
                turn_id,
                status,
                error,
                native_status,
                turn.get("error"),
                // A thread read cannot prove input unread: open steers become unconfirmed.
                &[],
            )?;
            if !settled {
                return Err(RecoveryError::CommitFailed);
            }
            crate::lifecycle_effects::notify_parent_of_turn(tx, agent_id, turn_id, unix_seconds())?;
            let Some(settled_agent) = AgentRepository::get(tx, &AgentId::from(agent_id))? else {
                return Err(RecoveryError::CommitFailed);
            };
            crate::account_worker::history::terminal(
                tx,
                &settled_agent,
                turn_id,
                native_status,
                turn.get("error"),
                unix_seconds(),
            )?;
            streams.stage_recovered_notification(
                format!("restart:{attempt_id}:{turn_id}:turn"),
                crate::notification_dispatch::RecoveredNotificationObservation {
                    agent: settled_agent.value(),
                    method: "turn/completed".into(),
                    params: json!({
                        "threadId":settled_agent.extra.get("threadId").cloned().unwrap_or(Value::Null),
                        "turnId":turn_id,
                        "turn":turn
                    }),
                    observed_at: unix_seconds(),
                },
            );
            let batch = streams
                .take_post_commit()
                .map_err(|_| RecoveryError::CommitFailed)?;
            super::store::persist_publication_batch(tx, &scope.account, &batch)
                .map_err(|_| RecoveryError::CommitFailed)?;
            super::store::persist_streams(tx, &scope.account, &streams)
                .map_err(|_| RecoveryError::CommitFailed)?;
            stream_update = Some(streams);
        }
        Ok(RecoveryEffectResult {
            outcome: RecoveryOutcome::Applied(json!({"status":"accepted","turnId":turn_id})),
            stream_update,
        })
    }
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
        _: &'a TaskRecoverySnapshot,
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
