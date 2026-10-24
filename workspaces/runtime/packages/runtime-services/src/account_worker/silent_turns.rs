//! The account worker's check of a silent running turn (liveness
//! `turn_stalled`). A turn whose provider sent nothing for
//! [`liveness::SILENT_TURN_SECONDS`] while no tool runs is read from native
//! history with the existing turn recovery: two thread reads around an exact
//! turn read, then a compare-and-apply settlement. It settles the turn once,
//! and it never sends input: the retained reader allows only history reads of
//! the turn's thread. A Claude bridge reports a turn it holds as active, so
//! only Codex turns are checked here; the liveness list shows Claude stalls.
//!
//! Each silence episode is checked at 10, 20, 40, 80 ... minutes of silence.
//! The check identity names the episode and its step, so its retained history
//! reads stay bounded and a repeated check of one step replays its answer.
use super::{AccountCommand, StoreBridge};
use crate::{
    account_worker::retained::{RequestActor, RetainedSessionPort, consumers::HistoryPort},
    liveness,
    recovery::{RecoveryError, RecoveryOutcome},
    recovery_absence::RecoveryObservation,
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
    recovery_runtime::{RecoveryJob, RecoveryRuntime, RetainedRecoveryReader},
    task_recovery_store::TaskCompletionWriter,
};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_store::{EventWriteRepository, RuntimeStore, StoreError};

/// Most silent turns one pass checks.
const MAX_CHECKS: usize = 8;

pub(super) struct Pass {
    pub(super) bridge: StoreBridge,
    pub(super) account: String,
    pub(super) scope: ConnectionScope,
    pub(super) session: RetainedSessionPort,
    pub(super) commands: tokio::sync::mpsc::Sender<AccountCommand>,
}

/// A silent Codex turn of this account and the identity of its check.
struct Candidate {
    snapshot: RecoverySnapshot,
    agent: String,
    epoch: i64,
    thread: String,
    turn: String,
    check: String,
}

/// The check identity of a silence episode that began at `since`: one per
/// doubling of the silence after [`liveness::SILENT_TURN_SECONDS`].
fn check_id(agent: &str, turn: &str, since: f64, now: f64) -> String {
    let step = ((now - since) / liveness::SILENT_TURN_SECONDS)
        .max(1.0)
        .log2()
        .floor() as i64;
    format!("liveness-turn:{agent}:{turn}:{}:{step}", since as i64)
}

fn candidates(
    tx: &studio_runtime_store::Transaction<'_>,
    account: &str,
    now: f64,
) -> Result<Vec<Candidate>, StoreError> {
    let mut statement = tx.prepare_cached(
        "SELECT id,record FROM runtime_agents WHERE account_key=?1 AND deleted_at IS NULL \
         AND status='running' AND COALESCE(in_flight,0)=1 AND provider='codex' ORDER BY id",
    )?;
    let rows = statement
        .query_map([account], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut found = Vec::new();
    for (id, raw) in rows {
        let Ok(mut record) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        studio_runtime_store::agent_volatile::merge(tx, &id, &mut record)?;
        let Some(since) = liveness::silent_turn(&record, now) else {
            continue;
        };
        let (Some(turn), Some(thread), Some(epoch)) = (
            record["turnId"].as_str().map(str::to_owned),
            record["threadId"]
                .as_str()
                .filter(|thread| !thread.is_empty())
                .map(str::to_owned),
            record["epoch"].as_i64(),
        ) else {
            continue;
        };
        let check = check_id(&id, &turn, since, now);
        let checked = EventWriteRepository::receipt(tx, &liveness::turn_check_key(&id))?
            .and_then(|receipt| serde_json::from_str::<Value>(&receipt.result).ok())
            .is_some_and(|saved| saved["check"].as_str() == Some(check.as_str()));
        if checked {
            continue;
        }
        let Ok(snapshot) = RecoverySnapshot::from_record(record) else {
            continue;
        };
        found.push(Candidate {
            snapshot,
            agent: id,
            epoch,
            thread,
            turn,
            check,
        });
        if found.len() == MAX_CHECKS {
            break;
        }
    }
    Ok(found)
}

/// Check every silent Codex turn of the account once for this pass. A settled
/// turn's agent is published and woken like the startup pass does.
pub(super) async fn check_account(pass: Pass) {
    let store = Arc::clone(&pass.bridge.store);
    let now = unix_seconds();
    let account = pass.account.clone();
    let Ok(found) = store
        .read_async("runtime-services/silent_turns", move |tx| {
            candidates(tx, &account, now)
        })
        .await
    else {
        return;
    };
    for candidate in found {
        if !pass.session.is_live() {
            return;
        }
        let outcome = check(&store, &pass, &candidate, now).await;
        // A settled turn is no longer running; a superseded check (the turn
        // changed while it ran) is repeated by the next pass.
        let verdict = match &outcome {
            Ok(RecoveryOutcome::Unconfirmed) => liveness::TurnCheck::Active,
            Err(RecoveryError::History(_) | RecoveryError::Dependency(_)) => {
                liveness::TurnCheck::Unreadable
            }
            Ok(_) | Err(_) => continue,
        };
        let (agent, turn, check) = (
            candidate.agent.clone(),
            candidate.turn.clone(),
            candidate.check.clone(),
        );
        let recorded = crate::scheduler::lifecycle_transaction_with(&store, move |tx| {
            liveness::record_turn_check(tx, &agent, &turn, &check, verdict, now)
        })
        .await;
        if let Err(error) = recorded {
            crate::diagnostics::log_stderr(format_args!(
                "silent turn check of {} was not recorded: {error}",
                candidate.agent
            ));
        }
    }
}

async fn check(
    store: &Arc<RuntimeStore>,
    pass: &Pass,
    candidate: &Candidate,
    now: f64,
) -> Result<RecoveryOutcome, RecoveryError> {
    let observation = RecoveryObservation {
        scope: pass.scope.clone(),
        at: now,
        supervisor: None,
        quiet: false,
        journal_drained: false,
        account_reserved: false,
        tools_refreshing: false,
        preparation_pending: false,
        completed_turn: None,
    };
    let observed_scope = pass.scope.clone();
    let live_session = pass.session.clone();
    let observed_value = observation.clone();
    let source: Arc<crate::recovery_runtime::ObservationSource> =
        Arc::new(move |scope: &ConnectionScope| {
            super::connection_recovery::observe_live_session(
                &live_session,
                &observed_scope,
                &observed_value,
                scope,
            )
        });
    let mut history = HistoryPort::new(
        pass.session.clone(),
        RequestActor {
            agent: candidate.agent.clone(),
            epoch: candidate.epoch,
        },
        candidate.check.clone(),
    );
    let mut reader = RetainedRecoveryReader::new(
        pass.scope.clone(),
        candidate.thread.clone(),
        &mut history,
        source.as_ref(),
    )?;
    let completion = super::connection_recovery::lifecycle_completion(
        store,
        &pass.account,
        candidate.check.clone(),
        now,
    )
    .await?;
    let task_completion: Arc<dyn TaskCompletionWriter> =
        Arc::new(super::connection_recovery::NoTaskCompletion);
    let mut runtime = RecoveryRuntime {
        store: Arc::clone(store),
        observation: Arc::clone(&source),
        completion,
        task_completion,
    };
    let result = runtime
        .reconcile(
            &RecoveryJob::Turn(candidate.snapshot.clone()),
            pass.scope.clone(),
            &mut reader,
        )
        .await?;
    if matches!(result.outcome, RecoveryOutcome::Applied(_)) {
        if let Some(streams) = result.stream_update
            && pass
                .commands
                .send(AccountCommand::RestartStreams(Box::new(streams)))
                .await
                .is_err()
        {
            return Ok(result.outcome);
        }
        let _sent = pass
            .commands
            .send(AccountCommand::Wake {
                agent_id: candidate.agent.clone(),
                epoch: candidate.epoch,
            })
            .await;
    }
    Ok(result.outcome)
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}
