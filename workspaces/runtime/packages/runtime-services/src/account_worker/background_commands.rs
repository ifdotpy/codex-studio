//! Close the background commands of settled turns that the provider no longer
//! runs. Each new account worker checks them once, on its own connection:
//! a provider restart, a disconnect, or a backend restart can lose the report
//! of a command's end, and only the live provider can prove that the command
//! is gone. A command that the provider still lists stays running.
use super::retained::{RequestActor, RetainedSessionPort, consumers::HistoryPort};
use crate::{
    recovery_absence::RecoveryObservation,
    recovery_history::{ConnectionScope, HistoryError, HistoryRead, RecoveryReader},
    recovery_runtime::RetainedRecoveryReader,
    task_settlement::{self, CommandCheck, TerminalReport},
};
use serde_json::json;
use std::sync::Arc;
use studio_runtime_store::RuntimeStore;

/// Unit: wall time for the reads of one agent thread.
const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Check every agent of `account` with such a command. A failed read leaves
/// that agent's commands running for the next worker; it never stops the
/// worker or another agent's check.
pub(super) async fn check_account(
    store: Arc<RuntimeStore>,
    account: &str,
    scope: &ConnectionScope,
    session: &RetainedSessionPort,
) -> usize {
    let read_account = account.to_owned();
    let Ok(checks) = store
        .read_async("runtime-services/background_command_checks", move |tx| {
            task_settlement::absent_command_checks(tx, &read_account)
        })
        .await
    else {
        return 0;
    };
    let mut closed = 0;
    for check in checks {
        if !session.is_live() {
            break;
        }
        let Ok(report) = read_report(&check, scope, session).await else {
            continue;
        };
        let account = account.to_owned();
        let at = unix_seconds();
        if let Ok(count) = crate::scheduler::lifecycle_transaction_with(&store, move |tx| {
            task_settlement::close_absent_commands(tx, &check, &account, &report, at)
        })
        .await
        {
            closed += count;
        }
    }
    closed
}

/// Read what the provider runs for the check's thread, between two journal
/// barriers: a report of a command's end that the provider sent before the
/// list is committed first and keeps that command's exact result.
async fn read_report(
    check: &CommandCheck,
    scope: &ConnectionScope,
    session: &RetainedSessionPort,
) -> Result<TerminalReport, HistoryError> {
    let agent = check.agent["id"]
        .as_str()
        .ok_or(HistoryError::Invalid("agent"))?
        .to_owned();
    let epoch = check.agent["epoch"]
        .as_i64()
        .ok_or(HistoryError::Invalid("epoch"))?;
    // One check per agent thread and provider connection: its retained reads
    // keep one stable identity.
    let check_id = studio_runtime_store::sha256_hex(
        json!([
            "background-commands",
            agent,
            epoch,
            check.thread,
            scope.connection,
            scope.generation
        ])
        .to_string()
        .as_bytes(),
    );
    let observed_scope = scope.clone();
    let live_session = session.clone();
    let observation = RecoveryObservation {
        scope: scope.clone(),
        at: unix_seconds(),
        supervisor: None,
        quiet: false,
        journal_drained: false,
        account_reserved: false,
        tools_refreshing: false,
        preparation_pending: false,
        completed_turn: None,
    };
    let source = move |current: &ConnectionScope| {
        (current == &observed_scope && live_session.is_live()).then(|| observation.clone())
    };
    let mut history = HistoryPort::new(session.clone(), RequestActor { agent, epoch }, check_id);
    let mut reader =
        RetainedRecoveryReader::new(scope.clone(), check.thread.clone(), &mut history, &source)?;
    report(&mut reader, scope.clone(), &check.thread).await
}

async fn report(
    reader: &mut dyn RecoveryReader,
    scope: ConnectionScope,
    thread: &str,
) -> Result<TerminalReport, HistoryError> {
    let mut read = HistoryRead::new(reader, scope, std::time::Instant::now() + READ_BUDGET);
    read.barrier().await?;
    let state = read.thread_state(thread).await?;
    let report = match state["type"].as_str() {
        // An app-server runs a thread's commands only while it has the thread
        // loaded; its terminal list refuses a thread it has not loaded.
        Some("notLoaded") => TerminalReport::NotLoaded,
        Some(_) => TerminalReport::Listed(read.background_terminals(thread).await?),
        None => return Err(HistoryError::Invalid("thread.status")),
    };
    read.barrier().await?;
    Ok(report)
}

fn unix_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}
