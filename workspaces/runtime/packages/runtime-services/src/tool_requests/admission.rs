//! Atomic receipt admission/claim, sharing the recovery action lifecycle.
use super::{ToolRequestError, lifecycle};
use rusqlite::{Transaction, params};
use serde_json::{Value, json};
use studio_operations::*;
use studio_runtime_store::system_reads as records;

pub(crate) fn reserve(
    tx: &Transaction<'_>,
    candidate: Value,
    call: &str,
    prefix: &str,
    alias: Option<String>,
) -> Result<Value, ToolRequestError> {
    let id = candidate["id"]
        .as_str()
        .ok_or(ToolRequestError::InvalidRequest("Invalid request identity"))?;
    let existing = records::get(tx, "runtime_tool_requests", id)?;
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
            alias_target: id.into(),
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
    records::validate("runtime_tool_requests", id, record.clone())?;
    tx.execute(
        "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2) ON CONFLICT(id) DO NOTHING",
        params![id, record.to_string()],
    )?;
    for intent in intents {
        if let RequestWriteIntent::InsertAlias(alias) = intent {
            tx.execute("INSERT OR IGNORE INTO runtime_tool_request_aliases(agent,alias,request) VALUES(?1,?2,?3)",params![alias.agent.as_str(),alias.alias,alias.request.as_str()])?;
        }
    }
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
