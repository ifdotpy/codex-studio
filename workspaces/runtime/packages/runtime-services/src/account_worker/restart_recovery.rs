//! First-pass account recovery for restart-held input and disconnected turns.
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
            Arc::clone(&store),
            &first_pass.account,
            request_scope,
            first_pass.session.clone(),
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
        // Restart turns are settled above, so their commands are checked too.
        super::background_commands::check_account(
            Arc::clone(&store),
            &first_pass.account,
            &first_pass.scope,
            &first_pass.session,
        )
        .await;
        // A turn stopped while no worker was attached still runs in this
        // provider generation; end it on this connection.
        super::stopped_turns::end_account(
            store,
            &first_pass.account,
            &first_pass.scope,
            &first_pass.session,
        )
        .await;
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
    // Every agent of the account gets this pass. A load returns nothing for a
    // full page without an eligible row, so only the sweep end stops the scan.
    let mut cursor = RecoveryCursor::sweep();
    let mut recovered = Vec::new();
    let mut recovered_streams = None;
    while !cursor.swept() {
        let jobs = load_recovery_jobs(&store, account, &mut cursor).await?;
        for job in jobs {
            // Restart and provider loss share the same guarded native-history pass.
            let recovery_turn = matches!(&job, RecoveryJob::Connection { snapshot, .. }
                if snapshot.automatic_turn().is_some());
            if recovery_turn
                && matches!(&job, RecoveryJob::Connection { snapshot, .. }
                if !crate::image_workspace::restart_follow_up_allowed(
                    snapshot.record(),
                    crate::image_workspace::IMAGE_EXECUTION_SUPPORTED,
                ))
            {
                continue;
            }
            let mut history_id = None;
            let job = if recovery_turn {
                let RecoveryJob::Connection {
                    snapshot,
                    automatic,
                } = job
                else {
                    continue;
                };
                let check = automatic_check_id(&snapshot, &scope);
                let Some(snapshot) = claim_automatic_check(&store, snapshot, &check).await? else {
                    continue;
                };
                history_id = Some(check);
                RecoveryJob::Connection {
                    snapshot,
                    automatic,
                }
            } else {
                job
            };
            let snapshot = match &job {
                RecoveryJob::Turn(snapshot)
                    if snapshot.record()["restartRecovery"]["stage"] == "held"
                        && !snapshot.record()["contextRepairWait"].is_null()
                        && snapshot.eligibility().unconfirmed_start =>
                {
                    snapshot
                }
                RecoveryJob::Connection { snapshot, .. } if recovery_turn => snapshot,
                _ => continue,
            };
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
            let history_id = history_id.unwrap_or_else(|| snapshot.connection_recovery_id());
            let mut history = HistoryPort::new(
                session.clone(),
                RequestActor {
                    agent: agent.clone(),
                    epoch,
                },
                history_id.clone(),
            );
            let mut reader =
                RetainedRecoveryReader::new(scope.clone(), thread, &mut history, source.as_ref())?;
            let completion: Arc<Mutex<Box<dyn CompletionWriter>>> = if recovery_turn {
                super::connection_recovery::lifecycle_completion(
                    &store,
                    account,
                    history_id,
                    observation.at,
                )
                .await?
            } else {
                Arc::new(Mutex::new(Box::new(RestartInputCompletion::default())))
            };
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
            let outcome = match outcome {
                Ok(outcome) => outcome,
                // An unreadable turn keeps its interrupted state and its
                // error; the user's next message still runs. Later rows proceed.
                Err(RecoveryError::History(_) | RecoveryError::Dependency(_)) if recovery_turn => {
                    continue;
                }
                Err(error) => return Err(error),
            };
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

/// The identity of one automatic native-history check; its retained reads and
/// recovered messages use it too. A pending restart marker gets one check: it
/// survives every later provider generation, so checking it on each worker
/// start would retain receipts without bound. An adopted turn (`continued`,
/// outcome `active`) was still running at its check, and its turn/completed
/// can be lost with the worker that should have committed it (live
/// 2026-10-19: three agents stayed `running` after their turns ended, because
/// their one check had seen the turns running hours earlier). It is checked
/// once per provider connection instead. The agent stays `running`, so idle
/// unload keeps the account open; only a backend restart or a worker failure
/// starts a new connection.
fn automatic_check_id(snapshot: &RecoverySnapshot, scope: &ConnectionScope) -> String {
    let check = snapshot.connection_recovery_id();
    let marker = &snapshot.record()["restartRecovery"];
    if marker["stage"] == "continued" && marker["outcome"] == "active" {
        studio_runtime_store::sha256_hex(
            json!([check, scope.account, scope.connection, scope.generation])
                .to_string()
                .as_bytes(),
        )
    } else {
        check
    }
}

/// Claim the automatic native-history check `check` (see [`automatic_check_id`]).
/// A manual connection check and the user's next message remain available.
async fn claim_automatic_check(
    store: &Arc<RuntimeStore>,
    snapshot: RecoverySnapshot,
    check: &str,
) -> Result<Option<RecoverySnapshot>, RecoveryError> {
    let id = snapshot.record()["id"]
        .as_str()
        .ok_or(RecoveryError::CommitFailed)?
        .to_owned();
    let check = check.to_owned();
    let expected = snapshot.record().clone();
    let marker_key = snapshot
        .automatic_turn()
        .ok_or(RecoveryError::CommitFailed)?
        .marker_key();
    crate::scheduler::lifecycle_transaction_with(store, move |tx| {
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(id.as_str()))? else {
            return Ok(None);
        };
        let mut current = agent.value();
        if current != expected || current[marker_key]["automaticCheck"] == check.as_str() {
            return Ok(None);
        }
        current[marker_key]["automaticCheck"] = json!(check);
        let snapshot = RecoverySnapshot::from_record(current.clone())?;
        AgentRepository::put(
            tx,
            &studio_runtime_store::AgentRecord::from_value(current)
                .map_err(|_| RecoveryError::CommitFailed)?,
        )?;
        Ok(Some(snapshot))
    })
    .await
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
            // A restart reconciliation stages no retry, so a failed turn reports now.
            let settled_at = unix_seconds();
            crate::lifecycle_effects::notify_parent_of_turn(tx, agent_id, turn_id, settled_at)?;
            crate::lifecycle_effects::notify_parent_of_failed_turn(
                tx, agent_id, turn_id, settled_at,
            )?;
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
