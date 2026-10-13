//! Native submission port. Admission commits before the injected transport is called.
//! The account worker must serialize this port with stop and validate its connection
//! ticket immediately before I/O. Unknown submissions are retained, never repeated.

use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

use crate::monitor_policy::{self, Effect, Input, InputOutcome, MonitorError, Operation};
use crate::monitor_runtime::{self, PreparedStart};

pub enum Submission {
    Accepted,
    NotSubmitted { error: String },
    Unknown,
}

pub trait MonitorNative {
    fn submit(&self, operation: &Operation, effect: &Effect) -> Submission;
}

pub fn start(
    store: &RuntimeStore,
    port: &impl MonitorNative,
    id: &str,
    prepared: &PreparedStart,
    now: f64,
) -> Result<Submission, MonitorError> {
    let effect = monitor_runtime::reserve_start(store, id, prepared, now)?;
    // A transport failure does not authorize another command/exec.
    let submission = port.submit(&prepared.operation, &effect);
    if let Submission::NotSubmitted { error } = &submission {
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
    port: &impl MonitorNative,
    request: InputRequest<'_>,
    now: f64,
) -> Result<Submission, MonitorError> {
    let effect = monitor_runtime::reserve_input(
        store,
        request.id,
        request.operation,
        request.ticket,
        request.input,
        now,
    )?;
    let result = port.submit(request.operation, &effect);
    // Accepted means transport admission, not a native acknowledgement. The exact
    // ticket's callback calls settle_input after its reply is known.
    let outcome = match &result {
        Submission::Accepted => None,
        Submission::NotSubmitted { error } => Some(InputOutcome::Rejected(error.clone())),
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
    port: &impl MonitorNative,
    id: &str,
    owner: Option<&str>,
    now: f64,
) -> Result<Value, MonitorError> {
    let (mut response, operation) = store.transaction_with(|tx| {
        let (snapshot, actor) = monitor_runtime::snapshot(tx, id)?;
        let decision = monitor_policy::cancel(&snapshot, owner, now)?;
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
    })?;
    if let Some(operation) = operation {
        // Intent is already durable. An unknown terminate is not resent by a
        // duplicate cancellation; the eventual exit receipt settles the monitor.
        let submission = port.submit(
            &operation,
            &Effect::Native {
                identity: format!("monitor-cancel:{id}"),
                method: "command/exec/terminate",
                params: json!({"processId":id}),
            },
        );
        if !matches!(submission, Submission::Accepted) {
            store.transaction_with(|tx| {
                let (current, actor) = monitor_runtime::snapshot(tx, id)?;
                if current.operation() == Some(&operation)
                    && current.record()["status"] == "running"
                {
                    let mut record = current.record().clone();
                    record["error"] =
                        json!("Cancel requested. Process termination was not confirmed.");
                    response["error"] = record["error"].clone();
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
    }
    Ok(response)
}
