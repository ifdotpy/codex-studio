//! Durable HTTP input identity. No native write is permitted without its receipt.

use rusqlite::{Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, EventWriteRepository, MonitorsRepository, StoreError,
};

use crate::monitor_policy::{self, Effect, Input, InputOutcome, MonitorError, Operation};
use crate::monitor_runtime;

const PREFIX: &str = "monitor-http-input:";
const TICKET_PREFIX: &str = "http:";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    operation: Operation,
    monitor: String,
    status: Status,
}

#[derive(Deserialize, Serialize)]
enum Status {
    Pending,
    Acknowledged,
    Rejected,
    Unknown,
}

pub(crate) struct Reservation {
    pub operation: Operation,
    pub ticket: String,
    pub effect: Effect,
}

pub(crate) fn acknowledged(request_id: &str) -> Value {
    json!({"requestId": request_id, "status": "acknowledged"})
}

/// Use the existing process receipt read without the original stdin content.
pub(crate) fn read(connection: &Connection, query: &Value) -> Result<Value, MonitorError> {
    let missing = || json!({"found":false,"result":null});
    let request_id = query["requestId"].as_str().unwrap_or_default();
    let Some(saved) = EventWriteRepository::receipt(connection, &format!("{PREFIX}{request_id}"))?
    else {
        return Ok(missing());
    };
    let receipt: Receipt = serde_json::from_str(&saved.result).map_err(StoreError::from)?;
    if query["agent"] != receipt.operation.agent || query["epoch"] != receipt.operation.epoch {
        return Ok(missing());
    }
    let Some(actor) =
        AgentRepository::get(connection, &AgentId::from(receipt.operation.agent.as_str()))?
    else {
        return Ok(missing());
    };
    if actor.value()["accountKey"] != query["accountKey"]
        || actor.value()["epoch"] != query["epoch"]
    {
        return Ok(missing());
    }
    let Some(monitor) = MonitorsRepository::get(connection, &receipt.monitor)? else {
        return Ok(missing());
    };
    let snapshot =
        monitor_policy::MonitorSnapshot::checked(monitor.value().clone(), actor.value().clone())?;
    if !snapshot.active_owner() || snapshot.operation() != Some(&receipt.operation) {
        return Ok(missing());
    }
    let (status, terminal) = match receipt.status {
        Status::Acknowledged => ("acknowledged", true),
        Status::Rejected => ("rejected", true),
        Status::Pending => ("pending", false),
        Status::Unknown => ("unknown", false),
    };
    Ok(
        json!({"found":true,"result":{"requestId":request_id,"status":if terminal {"completed"} else {"uncertain"},"processes":[],"response":{"kind":"monitor_input","monitorId":receipt.monitor,"status":status}}}),
    )
}

pub(crate) fn reserve(
    tx: &Transaction<'_>,
    id: &str,
    request_id: &str,
    input: &Input,
    now: f64,
) -> Result<Option<Reservation>, MonitorError> {
    if request_id.is_empty() {
        return Err(MonitorError::Refused(
            "Monitor input request identity is required",
        ));
    }
    let (snapshot, actor) = monitor_runtime::snapshot(tx, id)?;
    if !snapshot.active_owner() {
        return Err(MonitorError::Refused(
            "Monitor input owner is no longer active",
        ));
    }
    let operation = snapshot.operation().ok_or(MonitorError::Refused(
        "This interactive monitor is not active",
    ))?;
    let signature = format!(
        "http-input-v1:{}",
        studio_runtime_store::sha256_hex(
            &serde_json::to_vec(&(id, operation, input)).map_err(StoreError::from)?
        )
    );
    let key = format!("{PREFIX}{request_id}");
    if let Some(saved) = EventWriteRepository::receipt(tx, &key)? {
        if saved.signature != signature {
            return Err(MonitorError::Refused(
                "Monitor input request content changed",
            ));
        }
        let receipt: Receipt = serde_json::from_str(&saved.result).map_err(StoreError::from)?;
        if receipt.monitor != id || &receipt.operation != operation {
            return Err(MonitorError::Refused("Monitor input request owner changed"));
        }
        return match receipt.status {
            Status::Acknowledged => Ok(None),
            Status::Rejected => Err(MonitorError::Refused("Monitor input rejected")),
            Status::Pending | Status::Unknown => Err(MonitorError::InputOutcomeUnknown),
        };
    }
    let ticket = format!("{TICKET_PREFIX}{request_id}");
    let decision = monitor_policy::begin_input(&snapshot, operation, &ticket, input, now)?;
    let effect = decision
        .effects
        .first()
        .cloned()
        .ok_or(MonitorError::Refused("Missing monitor input"))?;
    let receipt = Receipt {
        operation: operation.clone(),
        monitor: id.into(),
        status: Status::Pending,
    };
    monitor_runtime::apply(tx, &snapshot, &actor, &decision, now)?;
    EventWriteRepository::put_receipt(
        tx,
        &key,
        &signature,
        &serde_json::to_string(&receipt).map_err(StoreError::from)?,
    )?;
    Ok(Some(Reservation {
        operation: operation.clone(),
        ticket,
        effect,
    }))
}

/// This write shares the transaction which settles the native input attempt.
/// A late exact callback can resolve Unknown, but cannot replace a terminal receipt.
pub(crate) fn settle(
    tx: &Transaction<'_>,
    id: &str,
    operation: &Operation,
    ticket: &str,
    outcome: &InputOutcome,
) -> Result<(), MonitorError> {
    let Some(request_id) = ticket.strip_prefix(TICKET_PREFIX) else {
        return Ok(());
    };
    let key = format!("{PREFIX}{request_id}");
    let Some(saved) = EventWriteRepository::receipt(tx, &key)? else {
        return Ok(());
    };
    let mut receipt: Receipt = serde_json::from_str(&saved.result).map_err(StoreError::from)?;
    if receipt.monitor != id || &receipt.operation != operation {
        return Err(MonitorError::Refused(
            "Monitor input receipt identity changed",
        ));
    }
    if matches!(receipt.status, Status::Acknowledged | Status::Rejected) {
        return Ok(());
    }
    receipt.status = match outcome {
        InputOutcome::Acknowledged => Status::Acknowledged,
        InputOutcome::Rejected(_) => Status::Rejected,
        InputOutcome::Unknown(_) => Status::Unknown,
    };
    EventWriteRepository::replace_receipt(
        tx,
        &key,
        &saved.signature,
        &serde_json::to_string(&receipt).map_err(StoreError::from)?,
    )?;
    Ok(())
}
