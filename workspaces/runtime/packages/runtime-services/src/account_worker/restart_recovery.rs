//! First-pass account recovery for restart-held input and disconnected turns.
use super::{AccountCommand, StoreBridge};
use crate::{
    account_worker::retained::{RequestActor, consumers::HistoryPort},
    recovery::{
        RecoveryAction, RecoveryEffectResult, RecoveryError, RecoveryFailure, RecoveryOutcome,
    },
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
use studio_runtime_schema::{
    provider_lifecycle::{CodexTurnStatus, TerminalTurnStatus},
    studio_errors::{Cause, Context, Diagnostics, Kind, RecoveryPhase, StudioError, Target},
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, SchedulerRepository};

pub(super) struct FirstPass {
    pub(super) bridge: StoreBridge,
    pub(super) account: String,
    pub(super) scope: ConnectionScope,
    pub(super) session: super::retained::RetainedSessionPort,
    pub(super) control: crate::provider_session_task::SessionControl,
    pub(super) commands: tokio::sync::mpsc::Sender<AccountCommand>,
}

struct FirstPassResult {
    ready: Vec<(String, i64)>,
    streams: Option<crate::notification_dispatch::NotificationStreamBuffer>,
    result: Result<(), RecoveryError>,
}

pub(super) fn start_first_pass(first_pass: FirstPass) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(test)]
        if first_pass
            .bridge
            .faults
            .hold_restart_recovery
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            first_pass.bridge.faults.restart_recovery_held.notify_one();
            first_pass
                .bridge
                .faults
                .release_restart_recovery
                .notified()
                .await;
        }
        let store = first_pass.bridge.store.clone();
        let request_scope = first_pass.scope.clone();
        let result = reconcile_first_pass(
            Arc::clone(&store),
            &first_pass.account,
            request_scope,
            first_pass.session.clone(),
            first_pass.control.clone(),
        )
        .await;
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
        if result.result.is_ok() {
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
        }
        let _result = first_pass
            .commands
            .send(AccountCommand::RestartRecoveryDone(result.result))
            .await;
    })
}

async fn probe_provider_calls(
    store: &RuntimeStore,
    account: &str,
    scope: &ConnectionScope,
    control: &crate::provider_session_task::SessionControl,
) -> Result<(), RecoveryError> {
    use studio_runtime_providers::supervisor_protocol::{NativeId, ProviderOperationState};
    let owner = account.to_owned();
    let rows = store
        .read_async("restart provider effects", move |tx| {
            super::store::account_obligations(tx, &owner)
        })
        .await
        .map_err(|_| RecoveryError::CommitFailed)?;
    for row in rows {
        if row.stream_id.as_deref() != Some(account)
            || row.generation != i64::try_from(scope.generation).ok()
        {
            continue;
        }
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            control.operation_status(studio_runtime_process::OperationId(row.id.clone())),
        )
        .await;
        let Ok(Ok(status)) = status else {
            continue;
        };
        if status.state != ProviderOperationState::Written {
            continue;
        }
        let remote = match status.native_id {
            Some(NativeId::Integer(id)) => u64::try_from(id).ok(),
            _ => None,
        };
        let account = account.to_owned();
        store
            .transaction_with(move |tx| {
                super::store::record_supervisor_written(
                    tx,
                    &account,
                    &row.id,
                    row.generation,
                    remote,
                )
            })
            .await
            .map_err(|_| RecoveryError::CommitFailed)?;
    }
    Ok(())
}

async fn reconcile_first_pass(
    store: Arc<RuntimeStore>,
    account: &str,
    scope: ConnectionScope,
    session: super::retained::RetainedSessionPort,
    control: crate::provider_session_task::SessionControl,
) -> FirstPassResult {
    // The cursor advances past located refusals. Only a fatal error stops the sweep.
    let mut cursor = RecoveryCursor::sweep();
    let mut recovered = Vec::new();
    let mut recovered_streams = None;
    let result = async {
        probe_provider_calls(&store, account, &scope, &control).await?;
        while !cursor.swept() {
            let jobs = match load_recovery_jobs(&store, account, &mut cursor).await {
                Ok(jobs) => jobs,
                Err(error) => {
                    record_failure(&store, &scope, RecoveryPhase::Scan, None, None, &error).await?;
                    match error.classify() {
                        RecoveryFailure::Refused => continue,
                        RecoveryFailure::Fatal(error) => return Err(error),
                    }
                }
            };
            for job in jobs {
                let agent = job.agent().record()["id"].as_str().map(str::to_owned);
                let identity = job.agent().connection_recovery_id();
                match reconcile_job(&store, account, &scope, &session, &control, job).await {
                    Ok(Some((agent, epoch, outcome))) => {
                        if matches!(outcome.outcome, RecoveryOutcome::Applied(_)) {
                            recovered.push((agent, epoch));
                            if let Some(streams) = outcome.stream_update {
                                recovered_streams = Some(streams);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        record_failure(
                            &store,
                            &scope,
                            RecoveryPhase::History,
                            agent,
                            Some(identity),
                            &error,
                        )
                        .await?;
                        match error.classify() {
                            RecoveryFailure::Refused => {}
                            RecoveryFailure::Fatal(error) => return Err(error),
                        }
                    }
                }
            }
        }
        Ok(())
    }
    .await;
    FirstPassResult {
        ready: recovered,
        streams: recovered_streams,
        result,
    }
}

async fn reconcile_job(
    store: &Arc<RuntimeStore>,
    account: &str,
    scope: &ConnectionScope,
    session: &super::retained::RetainedSessionPort,
    control: &crate::provider_session_task::SessionControl,
    job: RecoveryJob,
) -> Result<Option<(String, i64, RecoveryEffectResult)>, RecoveryError> {
    // Restart and provider loss share the same guarded native-history pass.
    let recovery_turn = matches!(&job, RecoveryJob::Connection { snapshot, .. }
        if snapshot.automatic_turn().is_some());
    if recovery_turn
        && matches!(&job, RecoveryJob::Connection { snapshot, .. }
        if crate::legacy_image_workspace::is_legacy(snapshot.record()))
    {
        return Ok(None);
    }
    let mut history_id = None;
    let job = if recovery_turn {
        let RecoveryJob::Connection {
            snapshot,
            automatic,
        } = job
        else {
            return Ok(None);
        };
        let check = automatic_check_id(&snapshot, scope);
        let Some(snapshot) = claim_automatic_check(store, snapshot, &check).await? else {
            return Ok(None);
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
            if snapshot.eligibility().unconfirmed_start
                && (snapshot.is_unclean_start() || snapshot.restart_input_wait()) =>
        {
            snapshot
        }
        RecoveryJob::Connection { snapshot, .. } if recovery_turn => snapshot,
        RecoveryJob::Turn(_) | RecoveryJob::Connection { .. } | RecoveryJob::Task(_) => {
            return Ok(None);
        }
    };
    let unclean_start = snapshot.is_unclean_start();
    if unclean_start {
        if let Ok(details) =
            serde_json::from_str::<HeldStartDetails>(&snapshot.record().to_string())
        {
            let attempt = details.start_attempt.id;
            let operation = format!(
                "account:{}:{}:{}",
                account,
                attempt,
                studio_runtime_providers::Method::TurnStart.as_str()
            );
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                control.operation_status(studio_runtime_process::OperationId(operation)),
            )
            .await;
        }
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
        super::connection_recovery::lifecycle_completion(store, account, history_id, observation.at)
            .await?
    } else {
        Arc::new(Mutex::new(Box::new(RestartInputCompletion::default())))
    };
    let task_completion: Arc<dyn TaskCompletionWriter> = Arc::new(NoTaskCompletion);
    let mut runtime = RecoveryRuntime {
        store: Arc::clone(store),
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
    if unclean_start {
        return Ok(None);
    }
    Ok(Some((agent, epoch, outcome)))
}

async fn record_failure(
    store: &RuntimeStore,
    scope: &ConnectionScope,
    phase: RecoveryPhase,
    agent_id: Option<String>,
    recovery_id: Option<String>,
    error: &RecoveryError,
) -> Result<(), studio_runtime_store::StoreError> {
    let causes = match error {
        RecoveryError::Store(error) => crate::studio_errors::store_causes(error),
        RecoveryError::Record(crate::recovery_policy::InvalidRecoveryRecord::Stored {
            table,
            id,
        }) => vec![Cause::StoredRecord {
            object: (*table).into(),
            id: id.clone(),
            pointer: None,
        }],
        RecoveryError::History(_)
        | RecoveryError::Record(crate::recovery_policy::InvalidRecoveryRecord::Schema { .. })
        | RecoveryError::Dependency(_)
        | RecoveryError::EffectsUnavailable
        | RecoveryError::CommitFailed => Vec::new(),
    };
    let kind = Kind::RecoveryFailed;
    let detail = match error {
        RecoveryError::History(error) => error.to_string(),
        RecoveryError::Record(error) => error.to_string(),
        RecoveryError::Store(_)
        | RecoveryError::Dependency(_)
        | RecoveryError::EffectsUnavailable
        | RecoveryError::CommitFailed => kind.reason().into(),
    };
    let context = Context::Recovery {
        account: scope.account.clone(),
        phase,
        agent_id: agent_id.clone(),
    };
    let id = format!(
        "recovery:{}",
        studio_runtime_store::sha256_hex(
            json!([scope, context, recovery_id, causes])
                .to_string()
                .as_bytes(),
        )
    );
    crate::studio_errors::record_incident(
        store,
        StudioError {
            id,
            kind,
            level: kind.level(),
            reason: kind.reason().into(),
            target: Target::Details,
            detail,
            subject: agent_id.unwrap_or_else(|| scope.account.clone()),
            at: unix_seconds(),
            monitor_id: None,
            event_id: None,
            rule_id: None,
            count: None,
            first_at: None,
            diagnostics: Diagnostics {
                context,
                certainty: kind.certainty(),
                causes,
            },
        },
    )
    .await
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
    let record = snapshot.record();
    let marker = &record["restartRecovery"];
    // A turn adopted after a provider disconnect stays `running` the same way.
    let adopted_transport_turn =
        record["status"] == "running" && record["connectionRecovery"]["outcome"] == "active";
    if (marker["stage"] == "continued" && marker["outcome"] == "active") || adopted_transport_turn {
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

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeldStartDetails {
    id: String,
    epoch: i64,
    account_key: Option<String>,
    thread_id: String,
    start_attempt: HeldAttempt,
}
#[derive(serde::Deserialize)]
struct HeldAttempt {
    id: String,
    events: Vec<String>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryTurn {
    id: String,
    client_user_message_id: Option<String>,
    items: Vec<HistoryItem>,
}
#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum HistoryItem {
    #[serde(rename = "userMessage")]
    UserMessage {
        #[serde(rename = "clientId")]
        client_id: Option<String>,
    },
    #[serde(other)]
    Other,
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
        if expected.is_unclean_start() {
            let details: HeldStartDetails = serde_json::from_str(&expected.record().to_string())
                .map_err(|_| RecoveryError::CommitFailed)?;
            let history: HistoryTurn =
                serde_json::from_str(&turn.to_string()).map_err(|_| RecoveryError::CommitFailed)?;
            if details.account_key.as_deref().unwrap_or("default") != scope.account {
                return Ok(RecoveryEffectResult::plain(RecoveryOutcome::Superseded));
            }
            let first = details
                .start_attempt
                .events
                .first()
                .ok_or(RecoveryError::CommitFailed)?;
            if history.client_user_message_id.as_deref() != Some(first.as_str())
                && !history.items.iter().any(|item| matches!(item, HistoryItem::UserMessage { client_id: Some(id) } if id == first))
            {
                return Ok(RecoveryEffectResult::plain(RecoveryOutcome::Unconfirmed));
            }
            let accepted = SchedulerRepository::settle_unclean_start(
                tx,
                &studio_runtime_store::TurnAttemptIdentity {
                    agent_id: details.id,
                    epoch: details.epoch,
                    attempt_id: details.start_attempt.id,
                },
                &details.thread_id,
                first,
                &history.id,
            )?;
            #[derive(serde::Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Accepted<'a> {
                status: &'static str,
                turn_id: &'a str,
            }
            return Ok(RecoveryEffectResult::plain(if accepted {
                RecoveryOutcome::Applied(crate::recovery::RecoveryReceipt::Applied(
                    studio_runtime_store::projection_value(&Accepted {
                        status: "accepted",
                        turn_id: &history.id,
                    }),
                ))
            } else {
                RecoveryOutcome::Superseded
            }));
        }
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
        agent.start_outcome_hold = None;
        agent.auto_wake = Some(true);
        agent.extra.insert("error".into(), Value::Null);
        if let Some(marker) = agent.extra.get_mut("restartRecovery") {
            marker["stage"] = json!("finished");
            marker["reconciledAt"] = json!(unix_seconds());
        }
        AgentRepository::put(tx, &agent)?;
        let terminal = CodexTurnStatus::observe(turn).and_then(|status| status.terminal());
        if let Some(terminal) = terminal {
            let native_status = terminal.as_str();
            let status = match terminal {
                TerminalTurnStatus::Completed => studio_runtime_store::AgentStatus::Completed,
                TerminalTurnStatus::Failed => studio_runtime_store::AgentStatus::Failed,
                TerminalTurnStatus::Interrupted => studio_runtime_store::AgentStatus::Interrupted,
            };
            let items = turn["items"]
                .as_array()
                .ok_or(RecoveryError::CommitFailed)?;
            let mut agent_value = agent.value();
            let mut streams = super::store::load_streams(tx, &scope.account)
                .map_err(|_| RecoveryError::CommitFailed)?;
            for item in items.iter().filter(|item| item["type"] == "agentMessage") {
                let item_native_id = item["id"].as_str().ok_or(RecoveryError::CommitFailed)?;
                let item_id = format!("{agent_id}:{item_native_id}");
                let previous =
                    studio_runtime_store::NotificationDispatchRepository::transcript_item(
                        tx, agent_id, &item_id,
                    )?;
                if previous.as_ref().is_some_and(|(record, _)| {
                    record["streaming"] != true
                        && item["text"]
                            .as_str()
                            .is_some_and(|body| record["text"].as_str() == Some(body))
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
                    .map_err(RecoveryError::from)?;
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
                        "threadId":settled_agent.thread_id.map(Value::String).unwrap_or(Value::Null),
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
            super::store::persist_streams(tx, &scope.account, &mut streams)
                .map_err(|_| RecoveryError::CommitFailed)?;
            stream_update = Some(streams);
        }
        Ok(RecoveryEffectResult {
            outcome: RecoveryOutcome::Applied(crate::recovery::RecoveryReceipt::Applied(
                json!({"status":"accepted","turnId":turn_id}),
            )),
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
