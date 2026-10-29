//! Adapter from validated persisted receipts to the pure operation state machine.
use super::{ToolRequestError, text};
use serde_json::{Value, json};
use studio_operations::*;
use studio_runtime_schema::persistence::{ToolRequestOutcome as O, ToolRequestStatus as S};

/// The single read boundary from persisted receipt pairs to operation lifecycle.
pub(crate) fn classify(record: &Value) -> Result<OperationState, ToolRequestError> {
    let invalid =
        || studio_runtime_store::system_reads::invalid("runtime_tool_requests", text(record, "id"));
    let status: S = record["stage"]
        .as_str()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    let outcome: O = record["outcome"]
        .as_str()
        .ok_or_else(invalid)?
        .parse()
        .map_err(|_| invalid())?;
    Ok(match (status, outcome) {
        (S::Queued, O::Pending) => OperationState::Reserved,
        (S::Running, O::Pending) if record["cancelRequested"] == true => {
            OperationState::CancelRequested
        }
        (S::Running, O::Pending) => OperationState::Dispatched,
        (S::Cancelled, O::NotApplied) => OperationState::CancelledBeforeDispatch,
        (S::Queued | S::Running | S::Interrupted | S::Failed | S::Unknown, O::Unknown) => {
            OperationState::Unknown
        }
        (S::Completed, O::Applied) => OperationState::Applied,
        // Historical handlers stored a completed delivery with a checked negative
        // outcome. Preserve that meaning without rewriting the durable receipt.
        (S::Completed | S::Failed, O::NotApplied) => OperationState::NotApplied,
        _ => {
            return Err(studio_runtime_store::system_reads::invalid(
                "runtime_tool_requests",
                text(record, "id"),
            )
            .into());
        }
    })
}

pub(crate) fn terminal(record: &Value) -> Result<bool, ToolRequestError> {
    Ok(match classify(record)? {
        OperationState::Applied
        | OperationState::NotApplied
        | OperationState::CancelledBeforeDispatch => true,
        OperationState::Reserved
        | OperationState::Dispatched
        | OperationState::CancelRequested
        | OperationState::Unknown => false,
    })
}

pub(crate) fn state(record: &Value) -> Result<State, ToolRequestError> {
    let lifecycle = classify(record)?;
    let revision = match lifecycle {
        OperationState::Reserved => 0,
        OperationState::Dispatched => 1,
        OperationState::CancelRequested => 2,
        OperationState::CancelledBeforeDispatch
        | OperationState::NotApplied
        | OperationState::Applied
        | OperationState::Unknown => 3,
    };
    let account = record["accountKey"].as_str().unwrap_or("default");
    let thread = text(record, "threadId");
    use sha2::Digest;
    let content = sha2::Sha256::digest(json!([record["signature"], record["agent"]]).to_string());
    Ok(State {
        revision: StateRevision(revision),
        tombstone: None,
        operation: Some(Operation {
            identity: OperationIdentity {
                scope: IdentityScope::ToolRequest {
                    account: account.into(),
                    thread: thread.into(),
                },
                request_id: text(record, "id").into(),
            },
            content: ContentFingerprint(content.into()),
            attempt: AttemptIdentity {
                agent: text(record, "agent").into(),
                account: account.into(),
                thread: thread.into(),
                epoch: AccountThreadEpoch(record["epoch"].as_u64().unwrap_or(0)),
                generation: record
                    .pointer("/supervisor/generation")
                    .and_then(Value::as_u64)
                    .map(ProcessGeneration),
            },
            state: lifecycle,
        }),
    })
}

fn operation(state: &State) -> Result<&Operation, ToolRequestError> {
    state
        .operation
        .as_ref()
        .ok_or(ToolRequestError::InvalidRequest(
            "Invalid request lifecycle",
        ))
}

pub(crate) fn apply(record: &mut Value, state: &State) -> Result<bool, ToolRequestError> {
    let operation = operation(state)?;
    if classify(record)? == operation.state {
        return Ok(false);
    }
    let (stage, outcome, cancel) = match operation.state {
        OperationState::Reserved => (S::Queued, O::Pending, false),
        OperationState::Dispatched => (S::Running, O::Pending, false),
        OperationState::CancelRequested => (S::Running, O::Pending, true),
        OperationState::CancelledBeforeDispatch => (S::Cancelled, O::NotApplied, true),
        OperationState::NotApplied => (S::Failed, O::NotApplied, record["cancelRequested"] == true),
        OperationState::Applied => (S::Completed, O::Applied, record["cancelRequested"] == true),
        OperationState::Unknown => (S::Failed, O::Unknown, record["cancelRequested"] == true),
    };
    record["stage"] = json!(stage);
    record["outcome"] = json!(outcome);
    record["cancelRequested"] = json!(cancel);
    Ok(true)
}

pub(super) fn cancel(record: &mut Value) -> Result<bool, ToolRequestError> {
    let state = state(record)?;
    let operation = operation(&state)?;
    let decision = transition(
        &state,
        &Event {
            id: "cancel".into(),
            expected_revision: state.revision,
            identity: operation.identity.clone(),
            attempt: operation.attempt.clone(),
            kind: EventKind::Cancel,
        },
    );
    match decision.kind {
        DecisionKind::Rejected(_) => Err(ToolRequestError::InvalidRequest(
            "Conflicting request lifecycle evidence",
        )),
        DecisionKind::Transitioned => apply(record, &decision.next_state),
        DecisionKind::ReturnExisting
        | DecisionKind::NoEffect
        | DecisionKind::ReceiptMissingUnknown => Ok(false),
    }
}

pub(crate) fn settle(
    record: &mut Value,
    result: &Value,
    committed: bool,
) -> Result<bool, ToolRequestError> {
    let state = state(record)?;
    let first = result["contentItems"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["type"] == "inputText")
        .and_then(|item| item["text"].as_str());
    let tool: RequestTool =
        serde_json::from_value(record["tool"].clone()).unwrap_or(RequestTool::Other);
    let server_outcome = if tool == RequestTool::OrchestrationServers {
        first
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| match v["outcome"].as_str() {
                Some("applied") => Some(Outcome::Applied),
                Some("not_applied") => Some(Outcome::NotApplied),
                Some("unknown") => Some(Outcome::Unknown),
                _ => None,
            })
    } else {
        None
    };
    let decision = decide_settlement(
        &state,
        &SettlementInput {
            request_id: text(record, "id").into(),
            event_id: "settle".into(),
            evidence: ResultEvidence {
                tool,
                success: result["success"].as_bool(),
                read_only: record["readOnly"] == true,
                first_error: first.map(str::to_owned),
                server_outcome,
            },
            explicit_outcome: None,
            proof: None,
            operation_receipt_present: committed,
            result_changed: record.get("result") != Some(result),
        },
    )
    .map_err(|_| ToolRequestError::InvalidRequest("Conflicting request lifecycle evidence"))?;
    if decision.kind == SettlementKind::NoChange {
        return Ok(false);
    }
    apply(record, &decision.state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_completed_negative_receipt_keeps_its_meaning() {
        let record = json!({"id":"request","stage":"completed","outcome":"not_applied"});
        let decoded = state(&record).expect("historical negative receipt");
        assert_eq!(
            decoded.operation.expect("operation").state,
            OperationState::NotApplied
        );
        assert_eq!(record["stage"], "completed");
        let mut retained = record.clone();
        assert!(!cancel(&mut retained).expect("already settled"));
        assert_eq!(retained, record);
    }

    #[test]
    fn invalid_pair_cannot_prove_terminal_completion() {
        let mut connection = rusqlite::Connection::open_in_memory().expect("connection");
        let tx = connection.transaction().expect("transaction");
        for (stage, outcome) in [("queued", "applied"), ("completed", "unknown")] {
            let record = json!({"id":"request","stage":stage,"outcome":outcome});
            assert!(terminal(&record).is_err());
            assert!(state(&record).is_err());
            for specific in [false, true] {
                assert!(
                    super::super::refresh(
                        &tx,
                        &mut record.clone(),
                        &json!({}),
                        1.0,
                        specific,
                        None
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn receipt_classification_has_one_typed_owner_and_unknown_is_not_terminal() {
        let classify: fn(&Value) -> Result<OperationState, ToolRequestError> = classify;
        for (stage, outcome, cancel_requested, expected, finished) in [
            ("queued", "pending", false, OperationState::Reserved, false),
            (
                "running",
                "pending",
                false,
                OperationState::Dispatched,
                false,
            ),
            (
                "running",
                "pending",
                true,
                OperationState::CancelRequested,
                false,
            ),
            (
                "cancelled",
                "not_applied",
                true,
                OperationState::CancelledBeforeDispatch,
                true,
            ),
            (
                "failed",
                "not_applied",
                false,
                OperationState::NotApplied,
                true,
            ),
            ("completed", "applied", false, OperationState::Applied, true),
            ("running", "unknown", false, OperationState::Unknown, false),
            ("failed", "unknown", false, OperationState::Unknown, false),
        ] {
            let record = json!({"id":"request","stage":stage,"outcome":outcome,"cancelRequested":cancel_requested});
            assert_eq!(classify(&record).expect("valid receipt"), expected);
            assert_eq!(terminal(&record).expect("valid receipt"), finished);
        }
    }
}
