//! Native submission port. Admission commits before the injected transport is called.
//! The account worker must serialize this port with stop and validate its connection
//! ticket immediately before I/O. Unknown submissions are retained, never repeated.

use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use studio_runtime_store::RuntimeStore;
use tokio::sync::{Mutex, MutexGuard};

use crate::monitor_policy::{self, Effect, Input, InputOutcome, MonitorError, Operation};
use crate::monitor_runtime::{self, PreparedStart};

pub enum Submission {
    /// The transport returned its durable Written receipt for this effect identity.
    Written,
    /// The transport proves no bytes were sent; the same identity may be retried.
    NotSent {
        reason: NotSentReason,
        error: String,
    },
    /// The transport cannot prove whether it wrote; this identity must not be resent.
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NotSentReason {
    /// A temporary transport refusal; retry the same durable operation after backoff.
    #[default]
    Transient,
    /// The native monitor process is already gone, so cancellation is complete.
    MonitorMissing,
    /// The owning provider session is gone; the command outcome is not recoverable here.
    SessionMissing,
}

const CANCEL_RETRY_BASE_SECONDS: f64 = 1.0;
const CANCEL_RETRY_MAX_SECONDS: f64 = 64.0;
/// First wait before a failed terminal monitor write is tried again. Unit: seconds.
pub const TERMINAL_WRITE_RETRY_MIN_SECONDS: u64 = 1;
/// Longest wait between attempts of a failed terminal monitor write. Unit: seconds.
pub const TERMINAL_WRITE_RETRY_MAX_SECONDS: u64 = 30;

pub trait MonitorNative {
    /// Repeated calls with the same effect identity must reconcile the same native write.
    /// The first poll must hand the write to the ordered provider transport (or
    /// join its fair queue); later polls only wait for the reply. `start` keeps
    /// later cancel and input behind command/exec through that first poll.
    fn submit<'a>(
        &'a self,
        operation: &'a Operation,
        effect: &'a Effect,
    ) -> futures_util::future::BoxFuture<'a, Submission>;

    /// Wait before attempt `attempt + 1` of a terminal monitor write that
    /// failed with a retryable store error, for example a full disk.
    fn terminal_write_pause<'a>(
        &'a self,
        attempt: u32,
        _error: &'a MonitorError,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(tokio::time::sleep(terminal_write_retry_delay(attempt)))
    }
}

/// Exponential backoff for terminal monitor writes, from
/// `TERMINAL_WRITE_RETRY_MIN_SECONDS` to `TERMINAL_WRITE_RETRY_MAX_SECONDS`.
pub fn terminal_write_retry_delay(attempt: u32) -> std::time::Duration {
    let seconds = TERMINAL_WRITE_RETRY_MIN_SECONDS
        .saturating_mul(1_u64 << attempt.saturating_sub(1).min(16))
        .min(TERMINAL_WRITE_RETRY_MAX_SECONDS);
    std::time::Duration::from_secs(seconds)
}

/// Run an idempotent terminal monitor write until it commits. A full disk or
/// another retryable store failure holds the write and tries it again; it must
/// never leave a command whose process ended without its settled outcome. A
/// definitive refusal is returned at once.
pub async fn retry_terminal_write<T, F, Fut>(
    port: &(impl MonitorNative + ?Sized),
    label: &str,
    mut write: F,
) -> Result<T, MonitorError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, MonitorError>>,
{
    let mut attempt = 0_u32;
    loop {
        match write().await {
            Ok(value) => return Ok(value),
            Err(error) if error.retryable_store_failure() => {
                attempt = attempt.saturating_add(1);
                crate::diagnostics::log_stderr(format_args!(
                    "{label} was not recorded (attempt {attempt}), retrying: {error}"
                ));
                port.terminal_write_pause(attempt, &error).await;
            }
            Err(error) => return Err(error),
        }
    }
}

// Hold admission and its transport submission in one sequence. A concurrent cancel
// or input cannot overtake command/exec after its running intent is committed.
fn native_sequence(
    store: &RuntimeStore,
    id: &str,
) -> Result<MutexGuard<'static, ()>, MonitorError> {
    static SEQUENCES: [Mutex<()>; 64] = [const { Mutex::const_new(()) }; 64];
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    store.root().hash(&mut hash);
    id.hash(&mut hash);
    SEQUENCES[hash.finish() as usize % SEQUENCES.len()]
        .try_lock()
        .map_err(|_| MonitorError::Refused("Monitor native operation pending"))
}

pub async fn start(
    store: &RuntimeStore,
    port: &(impl MonitorNative + ?Sized),
    id: &str,
    prepared: &PreparedStart,
    now: f64,
) -> Result<Submission, MonitorError> {
    let sequence = native_sequence(store, id)?;
    let effect = monitor_runtime::reserve_start(store, id, prepared, now).await?;
    // A transport failure does not authorize another command/exec.
    let mut submission = port.submit(&prepared.operation, &effect);
    // command/exec answers only when the process exits. Hold the sequence only
    // for the first poll, which hands the request to the ordered transport, so
    // cancel and input for this running command are admitted after it. The
    // first poll ignores the cooperative budget so it cannot yield before that.
    let first_poll = tokio::task::unconstrained(&mut submission);
    let submission = match futures_util::FutureExt::now_or_never(first_poll) {
        Some(submission) => submission,
        None => {
            drop(sequence);
            submission.await
        }
    };
    // The command may have run and ended. Its terminal write is retried until
    // it commits, so a full disk delays the outcome but never drops it.
    if let Submission::NotSent { error, .. } = &submission {
        let receipt = crate::monitor_receipts::ResultReceipt {
            version: 1,
            key: id.into(),
            code: None,
            error: Some(error.chars().take(12000).collect()),
            operation: Some(prepared.operation.clone()),
            finished: now,
        };
        retry_terminal_write(port, "monitor start refusal", || {
            crate::monitor_receipts::finish(store, &receipt)
        })
        .await?;
    }
    if matches!(submission, Submission::Unknown) {
        let observed = crate::event_write::unix_time()?;
        retry_terminal_write(port, "monitor unknown start reconciliation", || {
            monitor_runtime::reconcile_unknown(store, id, observed)
        })
        .await?;
    }
    Ok(submission)
}

pub struct InputRequest<'a> {
    pub id: &'a str,
    pub operation: &'a Operation,
    pub ticket: &'a str,
    pub input: &'a Input,
}

pub async fn input(
    store: &RuntimeStore,
    port: &(impl MonitorNative + ?Sized),
    request: InputRequest<'_>,
    now: f64,
) -> Result<Submission, MonitorError> {
    let _sequence = native_sequence(store, request.id)?;
    let effect = monitor_runtime::reserve_input(
        store,
        request.id,
        request.operation,
        request.ticket,
        request.input,
        now,
    )
    .await?;
    let result = port.submit(request.operation, &effect).await;
    // Written means the transport wrote the exact ticket. The exact ticket's
    // callback calls settle_input after its reply is known.
    let outcome = match &result {
        Submission::Written => None,
        Submission::NotSent { error, .. } => Some(InputOutcome::Rejected(error.clone())),
        Submission::Unknown => Some(InputOutcome::Unknown(
            "Input outcome unknown; do not repeat it".into(),
        )),
    };
    if let Some(outcome) = outcome {
        monitor_runtime::settle_input(
            store,
            request.id,
            request.operation,
            request.ticket,
            outcome,
            now,
        )
        .await?;
    }
    Ok(result)
}

pub async fn cancel(
    store: &RuntimeStore,
    port: &(impl MonitorNative + ?Sized),
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<Value, MonitorError> {
    cancel_checked(store, port, id, owner, now, None).await
}

pub async fn cancel_checked(
    store: &RuntimeStore,
    port: &(impl MonitorNative + ?Sized),
    id: &str,
    owner: Option<&str>,
    now: f64,
    expected: Option<Value>,
) -> Result<Value, MonitorError> {
    let _sequence = native_sequence(store, id)?;
    let id_for_reservation = id.to_owned();
    let owner_for_reservation = owner.map(str::to_owned);
    let (mut response, operation) = store
        .transaction_with(move |tx| {
            if let Some(expected) = &expected {
                let (snapshot, owner) = monitor_runtime::snapshot(tx, &id_for_reservation)?;
                let owner =
                    serde_json::to_value(owner).map_err(studio_runtime_store::StoreError::from)?;
                let source = expected["_sourceGeneration"].as_u64().map(|generation| {
                    crate::account_session_runtime::ConnectionScope {
                        account: expected["_sourceAccountKey"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                        connection: expected["_sourceConnectionId"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                        generation,
                    }
                });
                let process = crate::process_controls::project_monitor(
                    tx,
                    snapshot.record(),
                    &owner,
                    expected["_sourceConnectionId"].as_str(),
                    source.as_ref(),
                )
                .map_err(|error| match error {
                    crate::process_controls::Error::Store(error) => MonitorError::Store(error),
                    crate::process_controls::Error::Invalid(message) => {
                        MonitorError::Refused(message)
                    }
                })?;
                if !crate::process_controls::matches(&process, expected)
                    || process["capabilities"]["stop"] != true
                    || snapshot.record()["status"] != expected["_raw"]["status"]
                    || snapshot.record()["reattachedConnectionId"]
                        != expected["_raw"]["reattachedConnectionId"]
                {
                    return Err(MonitorError::Refused(
                        "The process owner changed. Refresh the process list",
                    ));
                }
            }
            reserve_cancel(
                tx,
                &id_for_reservation,
                owner_for_reservation.as_deref(),
                now,
            )
        })
        .await?;
    if let Some(operation) = operation {
        let receipt = port.submit(&operation, &cancel_effect(id)).await;
        let (status, error, retry_reason) = match &receipt {
            Submission::Written => ("written", None, None),
            Submission::NotSent {
                reason: NotSentReason::Transient,
                error,
            } => (
                "pending",
                Some(error.clone()),
                Some(NotSentReason::Transient),
            ),
            Submission::NotSent {
                reason: NotSentReason::MonitorMissing,
                error,
            } => (
                "done",
                Some(error.clone()),
                Some(NotSentReason::MonitorMissing),
            ),
            Submission::NotSent {
                reason: NotSentReason::SessionMissing,
                error,
            } => ("unknown", Some(error.clone()), None),
            Submission::Unknown => (
                "unknown",
                Some("Cancel dispatch outcome unknown".into()),
                None,
            ),
        };
        let id_for_settlement = id.to_owned();
        let operation_for_settlement = operation.clone();
        let response_error = store
            .transaction_with(move |tx| {
                let (current, actor) = monitor_runtime::snapshot(tx, &id_for_settlement)?;
                if current.operation() == Some(&operation_for_settlement)
                    && current.record()["status"] == "running"
                    && current.record()["cancelRequested"] == true
                {
                    let mut decision = if retry_reason == Some(NotSentReason::MonitorMissing) {
                        monitor_policy::finish(
                            &current,
                            Some(&operation_for_settlement),
                            None,
                            error.as_deref(),
                            now,
                        )?
                    } else {
                        monitor_policy::Decision {
                            record: current.record().clone(),
                            effects: vec![],
                            response: Value::Null,
                        }
                    };
                    let previous_attempts = current
                        .record()
                        .pointer("/cancelDispatch/attempts")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let attempts = if retry_reason == Some(NotSentReason::Transient) {
                        previous_attempts.saturating_add(1)
                    } else {
                        previous_attempts
                    };
                    let retry_at = (retry_reason == Some(NotSentReason::Transient))
                        .then(|| now + cancel_retry_delay(attempts));
                    let mut dispatch = json!({"status":status,"updatedAt":now,"attempts":attempts});
                    let mut response_error = None;
                    if let Some(retry_at) = retry_at {
                        dispatch["nextAttemptAt"] = json!(retry_at);
                    }
                    if let Some(error) = &error {
                        let error = error.chars().take(12000).collect::<String>();
                        dispatch["error"] = json!(error);
                        response_error = Some(error);
                        if status == "unknown" {
                            decision.record["error"] = dispatch["error"].clone();
                        }
                    }
                    decision.record["cancelDispatch"] = dispatch;
                    monitor_runtime::apply(tx, &current, &actor, &decision, now)?;
                    Ok::<_, MonitorError>(response_error)
                } else {
                    Ok(None)
                }
            })
            .await?;
        if let Some(error) = response_error {
            response["error"] = json!(error);
        }
    }
    Ok(response)
}

/// Select persisted cancellation intents whose transport receipt allows progress.
pub async fn pending_cancel_ids(
    store: &RuntimeStore,
    now: f64,
) -> Result<Vec<String>, MonitorError> {
    store
        .read_async("runtime-services/monitor_driver", move |connection| {
            pending_cancel_ids_in(connection, now)
        })
        .await
}

fn pending_cancel_ids_in(
    connection: &rusqlite::Connection,
    now: f64,
) -> Result<Vec<String>, MonitorError> {
    let mut statement = connection
        .prepare(
            "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.status')='running' AND json_extract(record,'$.cancelRequested')=1 AND COALESCE(json_extract(record,'$.cancelDispatch.status'),'pending')='pending' AND COALESCE(json_extract(record,'$.cancelDispatch.nextAttemptAt'),0)<=?1 ORDER BY id LIMIT 64",
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    let ids = statement
        .query_map([now], |row| row.get::<_, String>(0))
        .map_err(studio_runtime_store::StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(studio_runtime_store::StoreError::from)?;
    Ok(ids)
}

/// Starting monitors without a committed native operation are durable launch intents.
/// Rule checks have their own scheduler and are excluded here.
pub async fn pending_start_ids(store: &RuntimeStore) -> Result<Vec<String>, MonitorError> {
    store
        .read_async("runtime-services/monitor_driver", |connection| {
            pending_start_ids_in(connection)
        })
        .await
}

fn pending_start_ids_in(connection: &rusqlite::Connection) -> Result<Vec<String>, MonitorError> {
    let mut statement = connection
        .prepare(
            "SELECT m.id FROM runtime_monitors m WHERE json_extract(m.record,'$.status')='starting' AND COALESCE(json_extract(m.record,'$.reattachRecovery.status'),'')!='running' AND json_extract(m.record,'$.operation') IS NULL AND json_extract(m.record,'$.ruleId') IS NULL AND (json_extract(m.record,'$.restartAfter') IS NULL OR EXISTS (SELECT 1 FROM runtime_monitors old WHERE old.id=json_extract(m.record,'$.restartAfter') AND json_extract(old.record,'$.status') IN ('completed','failed','cancelled'))) ORDER BY json_extract(m.record,'$.created'),m.id LIMIT 64",
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    let ids = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(studio_runtime_store::StoreError::from)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(studio_runtime_store::StoreError::from)?;
    Ok(ids)
}

/// P10 calls this in the lifecycle transaction while its retained account owner
/// serializes start/stop. After commit, submit only the returned exact operation
/// with `cancel_effect`; replay the lifecycle receipt rather than staging twice.
pub fn reserve_cancel(
    tx: &studio_runtime_store::Transaction<'_>,
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<(Value, Option<Operation>), MonitorError> {
    let (snapshot, actor) = monitor_runtime::snapshot(tx, id)?;
    let mut decision = monitor_policy::cancel(&snapshot, owner, now)?;
    let dispatch_status = snapshot
        .record()
        .pointer("/cancelDispatch/status")
        .and_then(Value::as_str);
    let needs_dispatch = decision.effects.contains(&Effect::Terminate)
        || (snapshot.record()["status"] == "running"
            && snapshot.record()["cancelRequested"] == true
            && (dispatch_status.is_none()
                || (dispatch_status == Some("pending")
                    && snapshot
                        .record()
                        .pointer("/cancelDispatch/nextAttemptAt")
                        .and_then(Value::as_f64)
                        .is_none_or(|retry_at| retry_at <= now))));
    if needs_dispatch {
        let previous_attempts = snapshot
            .record()
            .pointer("/cancelDispatch/attempts")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        decision.record["cancelDispatch"] = json!({
            "status":"pending",
            "attempts":previous_attempts,
        });
        if !decision.effects.contains(&Effect::Terminate) {
            decision.effects.push(Effect::Terminate);
        }
    }
    let operation = if decision.effects.contains(&Effect::Terminate) {
        if let Some(operation) = snapshot.operation().cloned() {
            Some(operation)
        } else {
            let error = "Cancel requested. Process termination was not confirmed: monitor operation metadata is unavailable";
            decision.record["cancelDispatch"] = json!({
                "status":"unknown",
                "attempts":snapshot.record().pointer("/cancelDispatch/attempts").and_then(Value::as_u64).unwrap_or(0),
                "error":error,
            });
            decision.record["error"] = json!(error);
            decision.response["error"] = json!(error);
            None
        }
    } else {
        None
    };
    monitor_runtime::apply(tx, &snapshot, &actor, &decision, now)?;
    Ok::<_, MonitorError>((decision.response, operation))
}

fn cancel_retry_delay(attempts: u64) -> f64 {
    (CANCEL_RETRY_BASE_SECONDS * 2_f64.powi(attempts.saturating_sub(1).min(6) as i32))
        .min(CANCEL_RETRY_MAX_SECONDS)
}

pub fn cancel_effect(id: &str) -> Effect {
    Effect::Native {
        identity: format!("monitor-cancel:{id}"),
        method: "command/exec/terminate",
        params: json!({"processId":id}),
    }
}
