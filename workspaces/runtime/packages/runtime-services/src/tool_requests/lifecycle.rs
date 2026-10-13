//! Adapter from validated persisted receipts to the pure operation state machine.
use super::{ToolRequestError, text};
use serde_json::{Value, json};
use studio_operations::*;

fn state(record: &Value) -> Result<State, ToolRequestError> {
    let (revision, lifecycle) = match (text(record, "stage"), text(record, "outcome")) {
        ("queued", "pending") => (0, OperationState::Reserved),
        ("running", "pending") if record["cancelRequested"] == true => {
            (2, OperationState::CancelRequested)
        }
        ("running", "pending") => (1, OperationState::Dispatched),
        ("cancelled", "not_applied") => (3, OperationState::CancelledBeforeDispatch),
        ("queued" | "running" | "interrupted" | "failed" | "unknown", "unknown") => {
            (3, OperationState::Unknown)
        }
        ("completed", "applied") => (3, OperationState::Applied),
        ("failed", "not_applied") => (3, OperationState::NotApplied),
        _ => {
            return Err(studio_runtime_store::system_reads::invalid(
                "runtime_tool_requests",
                text(record, "id"),
            )
            .into());
        }
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

fn apply(record: &mut Value, state: &State) -> Result<bool, ToolRequestError> {
    let operation = state
        .operation
        .as_ref()
        .ok_or(ToolRequestError::InvalidRequest(
            "Invalid request lifecycle",
        ))?;
    let (stage, outcome, cancel) = match operation.state {
        OperationState::Reserved => ("queued", "pending", false),
        OperationState::Dispatched => ("running", "pending", false),
        OperationState::CancelRequested => ("running", "pending", true),
        OperationState::CancelledBeforeDispatch => ("cancelled", "not_applied", true),
        OperationState::NotApplied => ("failed", "not_applied", record["cancelRequested"] == true),
        OperationState::Applied => ("completed", "applied", record["cancelRequested"] == true),
        OperationState::Unknown => ("failed", "unknown", record["cancelRequested"] == true),
    };
    record["stage"] = json!(stage);
    record["outcome"] = json!(outcome);
    record["cancelRequested"] = json!(cancel);
    Ok(true)
}

pub(super) fn cancel(record: &mut Value) -> Result<bool, ToolRequestError> {
    let state = state(record)?;
    let operation = state
        .operation
        .as_ref()
        .ok_or(ToolRequestError::InvalidRequest(
            "Invalid request lifecycle",
        ))?;
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
        _ => Ok(false),
    }
}

pub(super) fn settle(
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
