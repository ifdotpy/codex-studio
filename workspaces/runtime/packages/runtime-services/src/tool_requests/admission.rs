//! Atomic receipt admission/claim, sharing the recovery action lifecycle.
use super::{ToolRequestError, lifecycle};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use studio_operations::*;
use studio_runtime_store::system_reads as records;

pub(crate) fn reserve(
    tx: &Transaction<'_>,
    mut candidate: Value,
    call: &str,
    prefix: &str,
    alias: Option<String>,
) -> Result<Value, ToolRequestError> {
    let id = candidate["id"]
        .as_str()
        .ok_or(ToolRequestError::InvalidRequest("Invalid request identity"))?
        .to_owned();
    let existing = records::get(tx, "runtime_tool_requests", &id)?;
    if existing.is_none() {
        let admission = crate::event_write::begin_request::<super::OpaqueToolReply>(
            tx,
            crate::event_write::RequestKey::new(
                "managed_tool",
                &[
                    candidate["agent"].as_str().unwrap_or_default(),
                    candidate["accountKey"].as_str().unwrap_or("default"),
                    candidate["threadId"].as_str().unwrap_or_default(),
                ],
                &id,
            ),
            candidate["signature"].as_str().unwrap_or_default(),
            chrono::Utc::now().timestamp_millis(),
        )?;
        match admission {
            crate::event_write::CallerAdmission::Fresh(admitted) => {
                candidate["admitted"] = studio_runtime_store::projection_value(&admitted);
            }
            crate::event_write::CallerAdmission::Replay(_) => {
                return Err(ToolRequestError::InvalidRequest(
                    "Request already completed; recover its original receipt",
                ));
            }
        }
        candidate["kind"] = json!(studio_runtime_schema::persistence::ToolRequestKind::ManagedTool);
    }
    if existing.as_ref().is_some_and(|old| {
        old["agent"] != candidate["agent"]
            || old["epoch"] != candidate["epoch"]
            || old["accountKey"] != candidate["accountKey"]
            || old["threadId"] != candidate["threadId"]
    }) {
        return Err(ToolRequestError::InvalidRequest(
            "Tool receipt belongs to another actor or turn",
        ));
    }
    let state = lifecycle::state(&candidate)?;
    let op = state
        .operation
        .as_ref()
        .ok_or(ToolRequestError::InvalidRequest(
            "Invalid request lifecycle",
        ))?;
    let decision = decide_reservation(
        &RequestSnapshot {
            existing: existing.as_ref().map(lifecycle::state).transpose()?,
        },
        &ReservationInput {
            identity: op.identity.clone(),
            alias_target: id.clone().into(),
            content: op.content,
            attempt: op.attempt.clone(),
            call_id: call.into(),
            alias_prefix: prefix.into(),
            request_alias: alias,
        },
    );
    let (mut record, state, intents) = match decision {
        ReservationDecision::Admitted { state, intents } => (candidate.clone(), state, intents),
        ReservationDecision::ReturnExisting { state, intents } => (
            existing.ok_or(ToolRequestError::InvalidRequest("Missing request"))?,
            state,
            intents,
        ),
        ReservationDecision::Rejected { .. } => {
            return Err(ToolRequestError::InvalidRequest(
                "Request identity already has different content",
            ));
        }
    };
    lifecycle::apply(&mut record, &state)?;
    let mut aliases = record["aliases"].as_array().cloned().unwrap_or_default();
    for intent in intents {
        if let RequestWriteIntent::InsertAlias(alias) = intent {
            let alias = json!(alias.alias);
            if !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
    }
    record["aliases"] = json!(aliases);
    records::validate("runtime_tool_requests", &id, record.clone())?;
    tx.execute("INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record", params![id,record.to_string()])?;
    Ok(record)
}
pub(crate) fn claim(record: &mut Value, now: f64) -> Result<bool, ToolRequestError> {
    let state = lifecycle::state(record)?;
    let op = state
        .operation
        .as_ref()
        .ok_or(ToolRequestError::InvalidRequest(
            "Invalid request lifecycle",
        ))?;
    let decision = transition(
        &state,
        &Event {
            id: "claim".into(),
            expected_revision: state.revision,
            identity: op.identity.clone(),
            attempt: op.attempt.clone(),
            kind: EventKind::ClaimDispatch,
        },
    );
    if decision.kind != DecisionKind::Transitioned {
        return Ok(false);
    }
    lifecycle::apply(record, &decision.next_state)?;
    record["updated"] = json!(now);
    record["started"] = json!(now);
    Ok(true)
}

/// A rolled-back transaction proves no service effect survived this refusal.
pub(crate) fn reject(record: &mut Value) -> Result<(), ToolRequestError> {
    let state = lifecycle::state(record)?;
    let op = state
        .operation
        .as_ref()
        .ok_or(ToolRequestError::InvalidRequest(
            "Invalid request lifecycle",
        ))?;
    let decision = transition(
        &state,
        &Event {
            id: "refused".into(),
            expected_revision: state.revision,
            identity: op.identity.clone(),
            attempt: op.attempt.clone(),
            kind: EventKind::DefinitiveRejection(NonExecutionProof::ExactPreWriteRejection),
        },
    );
    if matches!(decision.kind, DecisionKind::Rejected(_)) {
        return Err(ToolRequestError::InvalidRequest(
            "Conflicting request lifecycle evidence",
        ));
    }
    lifecycle::apply(record, &decision.next_state)?;
    Ok(())
}

/// Restart never dispatches queued work or retries a started operation.
pub(crate) fn restart(record: &mut Value) -> Result<bool, ToolRequestError> {
    let state = lifecycle::state(record)?;
    let operation = state
        .operation
        .as_ref()
        .ok_or(ToolRequestError::InvalidRequest(
            "Invalid request lifecycle",
        ))?;
    let decision = transition(
        &state,
        &Event {
            id: "restart".into(),
            expected_revision: state.revision,
            identity: operation.identity.clone(),
            attempt: operation.attempt.clone(),
            kind: EventKind::ServerRestarted,
        },
    );
    match decision.kind {
        DecisionKind::Rejected(_) => Err(ToolRequestError::InvalidRequest(
            "Conflicting request lifecycle evidence",
        )),
        DecisionKind::Transitioned => lifecycle::apply(record, &decision.next_state),
        DecisionKind::ReturnExisting
        | DecisionKind::NoEffect
        | DecisionKind::ReceiptMissingUnknown => Ok(false),
    }
}
