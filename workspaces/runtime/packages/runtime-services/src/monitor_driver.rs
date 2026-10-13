//! Native submission port. Admission commits before the injected transport is called.
//! The account worker must serialize this port with stop and validate its connection
//! ticket immediately before I/O. Unknown submissions are retained, never repeated.

use serde_json::{Value, json};
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};
use studio_runtime_store::RuntimeStore;

use crate::monitor_policy::{self, Effect, Input, InputOutcome, MonitorError, Operation};
use crate::monitor_runtime::{self, PreparedStart};

pub enum Submission {
    /// The transport returned its durable Written receipt for this effect identity.
    Written,
    /// The transport proves no bytes were sent; the same identity may be retried.
    NotSent { error: String },
    /// The transport cannot prove whether it wrote; this identity must not be resent.
    Unknown,
}

pub trait MonitorNative {
    /// Repeated calls with the same effect identity must reconcile the same native write.
    fn submit(&self, operation: &Operation, effect: &Effect) -> Submission;
}

// Hold admission and its transport submission in one sequence. A concurrent cancel
// or input cannot overtake command/exec after its running intent is committed.
fn native_sequence(id: &str) -> Result<MutexGuard<'static, ()>, MonitorError> {
    static SEQUENCES: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hash);
    SEQUENCES[hash.finish() as usize % SEQUENCES.len()]
        .try_lock()
        .map_err(|_| MonitorError::Refused("Monitor native operation pending"))
}

pub fn start(
    store: &RuntimeStore,
    port: &(impl MonitorNative + ?Sized),
    id: &str,
    prepared: &PreparedStart,
    now: f64,
) -> Result<Submission, MonitorError> {
    let _sequence = native_sequence(id)?;
    let effect = monitor_runtime::reserve_start(store, id, prepared, now)?;
    // A transport failure does not authorize another command/exec.
    let submission = port.submit(&prepared.operation, &effect);
    if let Submission::NotSent { error } = &submission {
        crate::monitor_receipts::finish(
            store,
            &crate::monitor_receipts::ResultReceipt {
                version: 1,
                key: id.into(),
                code: None,
                error: Some(error.chars().take(12000).collect()),
                operation: Some(prepared.operation.clone()),
                finished: now,
            },
        )?;
    }
    Ok(submission)
}

pub struct InputRequest<'a> {
    pub id: &'a str,
    pub operation: &'a Operation,
    pub ticket: &'a str,
    pub input: &'a Input,
}

pub fn input(
    store: &RuntimeStore,
    port: &(impl MonitorNative + ?Sized),
    request: InputRequest<'_>,
    now: f64,
) -> Result<Submission, MonitorError> {
    let _sequence = native_sequence(request.id)?;
    let effect = monitor_runtime::reserve_input(
        store,
        request.id,
        request.operation,
        request.ticket,
        request.input,
        now,
    )?;
    let result = port.submit(request.operation, &effect);
    // Written means the transport wrote the exact ticket. The exact ticket's
    // callback calls settle_input after its reply is known.
    let outcome = match &result {
        Submission::Written => None,
        Submission::NotSent { error } => Some(InputOutcome::Rejected(error.clone())),
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
        )?;
    }
    Ok(result)
}

pub fn cancel(
    store: &RuntimeStore,
    port: &(impl MonitorNative + ?Sized),
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<Value, MonitorError> {
    let _sequence = native_sequence(id)?;
    let (mut response, operation) =
        store.transaction_with(|tx| reserve_cancel(tx, id, owner, now))?;
    if let Some(operation) = operation {
        let receipt = port.submit(&operation, &cancel_effect(id));
        let (status, error) = match receipt {
            Submission::Written => ("written", None),
            Submission::NotSent { error } => ("pending", Some(error)),
            Submission::Unknown => ("unknown", Some("Cancel dispatch outcome unknown".into())),
        };
        store.transaction_with(|tx| {
            let (current, actor) = monitor_runtime::snapshot(tx, id)?;
            if current.operation() == Some(&operation)
                && current.record()["status"] == "running"
                && current.record()["cancelRequested"] == true
            {
                let mut record = current.record().clone();
                let mut dispatch = json!({"status":status,"updatedAt":now});
                if let Some(error) = &error {
                    dispatch["error"] = json!(error.chars().take(12000).collect::<String>());
                    response["error"] = dispatch["error"].clone();
                }
                record["cancelDispatch"] = dispatch;
                monitor_runtime::apply(
                    tx,
                    &current,
                    &actor,
                    &monitor_policy::Decision {
                        record,
                        effects: vec![],
                        response: Value::Null,
                    },
                    now,
                )?;
            }
            Ok::<_, MonitorError>(())
        })?;
    }
    Ok(response)
}

/// Select persisted cancellation intents whose transport receipt allows progress.
pub fn pending_cancel_ids(store: &RuntimeStore) -> Result<Vec<String>, MonitorError> {
    let connection = store.read()?;
    let mut statement = connection
        .prepare(
            "SELECT id FROM runtime_monitors WHERE json_extract(record,'$.status')='running' AND json_extract(record,'$.cancelRequested')=1 AND COALESCE(json_extract(record,'$.cancelDispatch.status'),'pending')='pending' ORDER BY id LIMIT 64",
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
            && (dispatch_status.is_none() || dispatch_status == Some("pending")));
    if needs_dispatch {
        decision.record["cancelDispatch"] = json!({"status":"pending"});
        if !decision.effects.contains(&Effect::Terminate) {
            decision.effects.push(Effect::Terminate);
        }
    }
    let operation = if decision.effects.contains(&Effect::Terminate) {
        Some(
            snapshot
                .operation()
                .cloned()
                .ok_or(MonitorError::Unavailable(
                    monitor_runtime::SESSION_DEPENDENCY,
                ))?,
        )
    } else {
        None
    };
    monitor_runtime::apply(tx, &snapshot, &actor, &decision, now)?;
    Ok::<_, MonitorError>((decision.response, operation))
}

pub fn cancel_effect(id: &str) -> Effect {
    Effect::Native {
        identity: format!("monitor-cancel:{id}"),
        method: "command/exec/terminate",
        params: json!({"processId":id}),
    }
}
